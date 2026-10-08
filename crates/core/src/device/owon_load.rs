//! OWON OEL15/30/60 series programmable DC loads, over their USB (CH340)
//! serial port speaking SCPI. Commands follow the OEL15&30 SCPI programming
//! manual; nothing here is guessed from captures.
//!
//! Battery test mode is used when available: the instrument then counts
//! amp-hours itself and enforces its own cut-off voltage, which keeps a pack
//! safe even if this program dies mid-discharge.

use super::scpi::Scpi;
use super::{Device, Discharger, Limits, LoadMode, LoadState, Sample};
use anyhow::{Context, Result, bail};
use std::time::Instant;

pub struct OwonLoad {
    io: Scpi,
    idn: String,
    limits: Limits,
    battery_mode: bool,
    tally: Tally,
}

#[derive(Debug, Default)]
struct Tally {
    amp_hours: f64,
    watt_hours: f64,
    runtime_s: f64,
    last_on: Option<Instant>,
}

impl Tally {
    fn add(&mut self, now: Instant, on: bool, amps: f64, watts: f64) {
        if let Some(prev) = self.last_on.filter(|_| on) {
            let secs = now.saturating_duration_since(prev).as_secs_f64();
            self.amp_hours += amps.abs() * secs / 3600.0;
            self.watt_hours += watts.abs() * secs / 3600.0;
            self.runtime_s += secs;
        }
        self.last_on = on.then_some(now);
    }
}

fn input_is_on(reply: &str) -> bool {
    matches!(reply.trim().to_ascii_uppercase().as_str(), "1" | "ON")
}

fn func(mode: LoadMode) -> &'static str {
    match mode {
        LoadMode::Cc => "CURRent",
        LoadMode::Cv => "VOLTage",
        LoadMode::Cr => "RESistance",
        LoadMode::Cp => "POWer",
    }
}

/// Ratings from the model number: OEL<volts/10><amps>, e.g. OEL3030 is
/// 300 V, 30 A. Power is 150 W on the 15-amp models and 300 W otherwise.
fn limits_from_model(idn: &str) -> Limits {
    let digits: String = idn
        .split(',')
        .find(|f| f.to_ascii_uppercase().contains("OEL"))
        .and_then(|f| {
            let up = f.to_ascii_uppercase();
            up.find("OEL").map(|i| up[i + 3..].to_string())
        })
        .unwrap_or_default()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let (volts, amps) = if digits.len() >= 4 {
        (
            digits[..2].parse::<f64>().unwrap_or(15.0) * 10.0,
            digits[2..4].parse::<f64>().unwrap_or(15.0),
        )
    } else {
        (150.0, 15.0)
    };
    Limits {
        max_volts: volts,
        max_amps: amps,
        max_watts: if amps <= 15.0 { 150.0 } else { 300.0 },
    }
}

impl OwonLoad {
    pub fn open(path: &str) -> Result<Self> {
        let bauds: Vec<u32> = match std::env::var("OWON_LOAD_BAUD")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            Some(b) => vec![b],
            None => vec![115_200, 9600, 19_200, 38_400, 57_600],
        };
        let (mut io, idn) = Self::find_baud(path, &bauds)?;
        if !idn.to_ascii_uppercase().contains("OEL") {
            bail!("not an OWON OEL load: {idn:?}");
        }
        // Without this the panel keeps control and commands are ignored.
        io.send("SYSTem:REMote")?;
        let limits = limits_from_model(&idn);
        Ok(Self {
            io,
            idn,
            limits,
            battery_mode: false,
            tally: Tally::default(),
        })
    }

    fn find_baud(path: &str, bauds: &[u32]) -> Result<(Scpi, String)> {
        let mut last = None;
        for &baud in bauds {
            let mut io = Scpi::open(path, baud).context("opening OWON load")?;
            match io.identify() {
                Ok(idn) => return Ok((io, idn)),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no baud rates to try")))
            .with_context(|| format!("no SCPI reply on {path} at {bauds:?}"))
    }

    fn num(&mut self, q: &str) -> Result<f64> {
        let r = self.io.ask(q)?;
        r.split(',')
            .next()
            .unwrap_or_default()
            .trim()
            .parse()
            .with_context(|| format!("{q} returned {r:?}"))
    }

    /// Put the instrument into its own battery test, so it counts capacity and
    /// stops at `vstop` without help from the host.
    pub fn battery_test(&mut self, amps: f64, vstop: f64) -> Result<()> {
        self.io.send("MODE BATTERY")?;
        self.io.send(&format!("BAT {amps:.3}"))?;
        self.io.send(&format!("BAT:VSTop {vstop:.3}"))?;
        self.io.send("BAT:VENabstop 1")?;
        self.battery_mode = true;
        Ok(())
    }

    pub fn normal_mode(&mut self) -> Result<()> {
        self.io.send("MODE NORM")?;
        self.battery_mode = false;
        Ok(())
    }
}

impl Device for OwonLoad {
    fn name(&self) -> String {
        self.idn.clone()
    }

    fn limits(&self) -> Limits {
        self.limits.clone()
    }

    fn measure(&mut self) -> Result<Sample> {
        Ok(Sample {
            volts: self.num("MEAS:VOLT?")?,
            amps: self.num("MEAS:CURR?")?,
        })
    }

    fn stop(&mut self) -> Result<()> {
        self.io.send("INP 0")
    }

    fn output_on(&mut self) -> Result<Option<bool>> {
        Ok(Some(input_is_on(&self.io.ask("INP?")?)))
    }
}

impl Discharger for OwonLoad {
    fn modes(&self) -> &'static [LoadMode] {
        &[LoadMode::Cc, LoadMode::Cp, LoadMode::Cv, LoadMode::Cr]
    }

    fn set_mode(&mut self, mode: LoadMode, value: f64) -> Result<()> {
        if self.battery_mode {
            return self.io.send(&format!("BAT {value:.3}"));
        }
        self.io.send(&format!("FUNC {}", func(mode)))?;
        let cmd = match mode {
            LoadMode::Cc => format!("CURR {value:.3}"),
            LoadMode::Cv => format!("VOLT {value:.3}"),
            LoadMode::Cr => format!("RES {value:.3}"),
            LoadMode::Cp => format!("POW {value:.3}"),
        };
        self.io.send(&cmd)
    }

    fn set_current(&mut self, amps: f64) -> Result<()> {
        if amps > self.limits.max_amps {
            bail!(
                "{amps:.2} A is over this load's {:.0} A rating",
                self.limits.max_amps
            );
        }
        self.set_mode(LoadMode::Cc, amps)
    }

    fn start(&mut self) -> Result<()> {
        let v = self.num("MEAS:VOLT?").unwrap_or(0.0);
        if v > self.limits.max_volts {
            bail!(
                "{v:.1} V is over this load's {:.0} V rating",
                self.limits.max_volts
            );
        }
        self.io.send("INP 1")
    }

    fn amp_hours(&mut self) -> Result<Option<f64>> {
        if self.battery_mode {
            return Ok(self.num("BAT:CAPA?").ok());
        }
        Ok(Some(self.tally.amp_hours))
    }

    fn state(&mut self) -> Result<LoadState> {
        let volts = self.num("MEAS:VOLT?")?;
        let amps = self.num("MEAS:CURR?")?;
        let watts = self.num("MEAS:POW?").unwrap_or(volts * amps);
        let on = self.output_on()?.unwrap_or(false);
        self.tally.add(Instant::now(), on, amps, watts);
        let (amp_hours, watt_hours, runtime_s) = if self.battery_mode {
            (
                self.num("BAT:CAPA?").unwrap_or(self.tally.amp_hours),
                self.num("BAT:ENERGY?").unwrap_or(self.tally.watt_hours),
                self.num("BAT:TIME?").unwrap_or(self.tally.runtime_s),
            )
        } else {
            (
                self.tally.amp_hours,
                self.tally.watt_hours,
                self.tally.runtime_s,
            )
        };
        Ok(LoadState {
            setpoint: self.num("CURR?").unwrap_or(0.0),
            volts,
            amps,
            watts,
            ohms: self.num("MEAS:RES?").ok(),
            amp_hours,
            watt_hours,
            temp_c: 0.0,
            runtime_s,
            on,
        })
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratings_come_from_the_model_number() {
        let l = limits_from_model("OWON,OEL3030,2612345,FV:V1.0.0");
        assert_eq!(l.max_volts, 300.0);
        assert_eq!(l.max_amps, 30.0);
        assert_eq!(l.max_watts, 300.0);

        let l = limits_from_model("OWON,OEL1515,2612345,FV:V1.0.0");
        assert_eq!(l.max_volts, 150.0);
        assert_eq!(l.max_amps, 15.0);
        assert_eq!(l.max_watts, 150.0);
    }

    #[test]
    fn input_state_reads_words_and_digits() {
        assert!(input_is_on("ON"));
        assert!(input_is_on("1\n"));
        assert!(!input_is_on("OFF"));
        assert!(!input_is_on("0"));
        assert!(!input_is_on(""));
    }

    #[test]
    fn tally_counts_only_while_on() {
        let t0 = Instant::now();
        let mut t = Tally::default();
        t.add(t0, true, 2.0, 30.0);
        t.add(t0 + std::time::Duration::from_secs(1800), true, 2.0, 30.0);
        t.add(t0 + std::time::Duration::from_secs(3600), false, 0.0, 0.0);
        t.add(t0 + std::time::Duration::from_secs(5400), true, 2.0, 30.0);
        t.add(t0 + std::time::Duration::from_secs(5401), false, 0.0, 0.0);
        assert!((t.amp_hours - 1.0).abs() < 1e-9);
        assert!((t.watt_hours - 15.0).abs() < 1e-9);
        assert!((t.runtime_s - 1800.0).abs() < 1e-9);
    }

    #[test]
    fn mode_names_match_the_manual() {
        assert_eq!(func(LoadMode::Cc), "CURRent");
        assert_eq!(func(LoadMode::Cr), "RESistance");
    }
}
