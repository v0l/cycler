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

pub struct OwonSpe {
    port: Box<dyn serialport::SerialPort>,
    idn: String,
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
        let mut psu = Self {
            port,
            idn: String::new(),
            limits: Limits {
                max_volts: 60.0,
                max_amps: 10.0,
                max_watts: 600.0,
            },
        };
        psu.idn = psu.ask("*IDN?")?;
        if !psu.idn.contains("SPE") {
            bail!("not an OWON SPE supply: {:?}", psu.idn);
        }
        if let (Ok(v), Ok(a)) = (
            psu.ask("VOLT:LIM?").and_then(|s| Ok(s.parse::<f64>()?)),
            psu.ask("CURR:LIM?").and_then(|s| Ok(s.parse::<f64>()?)),
        ) {
            psu.limits.max_volts = v;
            psu.limits.max_amps = a;
        }
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

    /// Clamp the supply's own hardware limits so a crashed controller cannot
    /// command more than the pack tolerates.
    pub fn arm_limits(&mut self, max_volts: f64, max_amps: f64) -> Result<()> {
        self.send(&format!("VOLT:LIM {max_volts:.3}"))?;
        self.send(&format!("CURR:LIM {max_amps:.3}"))?;
        self.limits.max_volts = max_volts;
        self.limits.max_amps = max_amps;
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
    fn reads_the_space_form_the_manual_documents() {
        let (v, _, _, r) = parse("2.000 5.000 10.000 0 0 0 1");
        assert_eq!(v, 2.0);
        assert_eq!(r, Regulation::Cv);
    }
}
