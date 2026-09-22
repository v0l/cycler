use super::{Charger, Device, Limits, Regulation, Sample};
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Info {
    pub volts: f64,
    pub amps: f64,
    pub watts: f64,
    pub over_voltage: bool,
    pub over_current: bool,
    pub over_temp: bool,
    pub regulation: Regulation,
}

/// Volts, amps and watts each model can actually deliver. `VOLT:LIM` and
/// `CURR:LIM` are the OVP and OCP trip registers, not the rating, so they say
/// nothing about the supply's capability and everything about what the last
/// run left behind.
const MODELS: &[(&str, f64, f64, f64)] = &[
    ("SPE3051", 30.0, 5.0, 150.0),
    ("SPE3102", 30.0, 10.0, 200.0),
    ("SPE6102", 60.0, 10.0, 200.0),
    ("SPE6053", 60.0, 5.0, 300.0),
    ("SPE3103", 30.0, 10.0, 300.0),
    ("SPE6103", 60.0, 10.0, 300.0),
];

fn rating(idn: &str) -> Option<Limits> {
    let idn = idn.to_ascii_uppercase();
    MODELS
        .iter()
        .find(|(model, ..)| idn.contains(model))
        .map(|&(_, max_volts, max_amps, max_watts)| Limits {
            max_volts,
            max_amps,
            max_watts,
        })
}

pub struct OwonSpe {
    port: Box<dyn serialport::SerialPort>,
    idn: String,
    rating: Limits,
    limits: Limits,
}

impl OwonSpe {
    pub fn open(path: &str) -> Result<Self> {
        let path = if path.is_empty() {
            "/dev/serial/by-id/usb-1a86_USB_Serial-if00-port0"
        } else {
            path
        };
        let port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(400))
            .open()
            .with_context(|| format!("opening PSU at {path}"))?;
        let unknown = Limits {
            max_volts: 30.0,
            max_amps: 5.0,
            max_watts: 150.0,
        };
        let mut psu = Self {
            port,
            idn: String::new(),
            rating: unknown.clone(),
            limits: unknown,
        };
        psu.idn = psu.ask("*IDN?")?;
        if !psu.idn.contains("SPE") {
            bail!("not an OWON SPE supply: {:?}", psu.idn);
        }
        if let Some(r) = rating(&psu.idn) {
            psu.rating = r;
        }
        psu.limits = psu.rating.clone();
        Ok(psu)
    }

    fn send(&mut self, cmd: &str) -> Result<()> {
        self.port.write_all(format!("{cmd}\r\n").as_bytes())?;
        self.port.flush()?;
        Ok(())
    }

    fn ask(&mut self, cmd: &str) -> Result<String> {
        for _ in 0..3 {
            let _ = self.port.clear(serialport::ClearBuffer::Input);
            self.send(cmd)?;
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            let deadline = Instant::now() + Duration::from_millis(800);
            while Instant::now() < deadline {
                match self.port.read(&mut byte) {
                    Ok(1) => {
                        if byte[0] == b'\n' {
                            break;
                        }
                        buf.push(byte[0]);
                    }
                    _ => continue,
                }
            }
            let line = String::from_utf8_lossy(&buf).trim().to_string();
            if !line.is_empty() {
                return Ok(line);
            }
        }
        bail!("no reply to {cmd}")
    }

    pub fn info(&mut self) -> Result<Info> {
        let reply = self.ask("MEAS:ALL:INFO?")?;
        // The manual shows space-separated fields; the SPE6103 actually sends
        // commas, so accept either.
        let f: Vec<&str> = reply
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect();
        if f.len() < 3 {
            bail!("unexpected MEAS:ALL:INFO? reply {reply:?}");
        }
        let flag = |i: usize| {
            f.get(i)
                .map(|v| *v == "1" || v.eq_ignore_ascii_case("on"))
                .unwrap_or(false)
        };
        Ok(Info {
            volts: f[0].parse().unwrap_or(0.0),
            amps: f[1].parse().unwrap_or(0.0),
            watts: f[2].parse().unwrap_or(0.0),
            over_voltage: flag(3),
            over_current: flag(4),
            over_temp: flag(5),
            regulation: match f.get(6).and_then(|v| v.parse::<u8>().ok()) {
                Some(1) => Regulation::Cv,
                Some(2) => Regulation::Cc,
                Some(3) => Regulation::Fault,
                _ => Regulation::Standby,
            },
        })
    }

    /// Write the supply's own OVP and OCP trip points so a crashed controller
    /// cannot command more than the pack tolerates, and read them back: a
    /// value the supply would not take is a trip waiting to happen.
    pub fn arm_limits(&mut self, max_volts: f64, max_amps: f64) -> Result<()> {
        self.send(&format!("VOLT:LIM {max_volts:.3}"))?;
        self.send(&format!("CURR:LIM {max_amps:.3}"))?;
        let got_v: f64 = self.ask("VOLT:LIM?")?.parse().unwrap_or(0.0);
        let got_a: f64 = self.ask("CURR:LIM?")?.parse().unwrap_or(0.0);
        if (got_v - max_volts).abs() > 0.05 || (got_a - max_amps).abs() > 0.05 {
            bail!(
                "supply kept its protection at {got_v:.2} V {got_a:.3} A \
                 instead of {max_volts:.2} V {max_amps:.3} A"
            );
        }
        self.limits.max_volts = got_v;
        self.limits.max_amps = got_a;
        Ok(())
    }
}

impl Device for OwonSpe {
    fn name(&self) -> String {
        self.idn.clone()
    }

    fn limits(&self) -> Limits {
        self.limits.clone()
    }

    /// One query for volts, amps, watts, the fault flags and the regulation
    /// mode, instead of three round trips at 250 ms each.
    fn measure(&mut self) -> Result<Sample> {
        let info = self.info()?;
        Ok(Sample {
            volts: info.volts,
            amps: info.amps,
        })
    }

    fn stop(&mut self) -> Result<()> {
        self.send("OUTP OFF")
    }

    fn output_on(&mut self) -> Result<Option<bool>> {
        Ok(Some(self.ask("OUTP?")?.trim().eq_ignore_ascii_case("ON")))
    }
}

impl Charger for OwonSpe {
    fn regulation(&mut self) -> Result<Option<Regulation>> {
        Ok(Some(self.info()?.regulation))
    }

    /// On this supply the limits are trip points: reaching one cuts the
    /// output mid-charge. So they are written from what this charge asks for,
    /// a little above it, and clamped only by what the model can deliver.
    /// Whatever the last run left on the panel does not decide this one.
    fn arm(&mut self, max_volts: f64, max_amps: f64) -> Result<bool> {
        let volts = max_volts.min(self.rating.max_volts);
        let amps = max_amps
            .min(self.rating.max_amps)
            .min(self.rating.max_watts / volts.max(1.0));
        self.arm_limits(volts, amps)?;
        Ok(true)
    }

    fn set(&mut self, volts: f64, amps: f64) -> Result<()> {
        if volts > self.limits.max_volts || amps > self.limits.max_amps {
            bail!("{volts} V {amps} A exceeds device limits {:?}", self.limits);
        }
        self.send(&format!("VOLT {volts:.3}"))?;
        self.send(&format!("CURR {amps:.3}"))
    }

    fn start(&mut self) -> Result<()> {
        self.send("OUTP ON")
    }
}

impl Drop for OwonSpe {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(reply: &str) -> (f64, f64, f64, Regulation) {
        let f: Vec<&str> = reply
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect();
        (
            f[0].parse().unwrap(),
            f[1].parse().unwrap(),
            f[2].parse().unwrap(),
            match f.get(6).and_then(|v| v.parse::<u8>().ok()) {
                Some(1) => Regulation::Cv,
                Some(2) => Regulation::Cc,
                Some(3) => Regulation::Fault,
                _ => Regulation::Standby,
            },
        )
    }

    #[test]
    fn reads_the_comma_form_the_supply_actually_sends() {
        let (v, a, w, r) = parse("0.000,0.000,0.000,OFF,OFF,OFF,0");
        assert_eq!((v, a, w), (0.0, 0.0, 0.0));
        assert_eq!(r, Regulation::Standby);
        let (_, a, _, r) = parse("51.500,2.730,140.595,OFF,OFF,OFF,2");
        assert_eq!(a, 2.73);
        assert_eq!(r, Regulation::Cc);
    }

    #[test]
    fn the_rating_comes_from_the_model_not_the_trip_points() {
        let r = rating("OWON,SPE6103,25521912,FV:V5.5.0").unwrap();
        assert_eq!((r.max_volts, r.max_amps, r.max_watts), (60.0, 10.0, 300.0));
        assert!(rating("OWON,SPE9999,1,FV:V1").is_none());
    }

    #[test]
    fn the_armed_current_stays_inside_the_power_envelope() {
        let r = rating("SPE6103").unwrap();
        let volts = 52.25_f64.min(r.max_volts);
        let amps = 6.25_f64.min(r.max_amps).min(r.max_watts / volts);
        assert!((amps - 5.741).abs() < 0.01);
    }

    #[test]
    fn reads_the_space_form_the_manual_documents() {
        let (v, _, _, r) = parse("2.000 5.000 10.000 0 0 0 1");
        assert_eq!(v, 2.0);
        assert_eq!(r, Regulation::Cv);
    }
}
