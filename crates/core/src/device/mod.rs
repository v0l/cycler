pub mod dl24;
pub mod owon;
pub mod owon_load;
pub mod passive;
pub mod scpi;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub volts: f64,
    pub amps: f64,
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_volts: f64,
    pub max_amps: f64,
    pub max_watts: f64,
}

pub trait Device {
    fn name(&self) -> String;
    fn limits(&self) -> Limits;
    fn measure(&mut self) -> Result<Sample>;
    fn stop(&mut self) -> Result<()>;
    /// Whether the device is actually delivering, read back from the device
    /// itself. Never inferred from what it was last told: a supply left on by
    /// a killed process reports the truth here and nowhere else.
    fn output_on(&mut self) -> Result<Option<bool>> {
        Ok(None)
    }
}

/// What a supply is regulating right now, which says more than its setpoints:
/// a charger in CC is still filling, one in CV is tapering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regulation {
    Standby,
    Cv,
    Cc,
    Fault,
}

impl Regulation {
    pub fn label(self) -> &'static str {
        match self {
            Regulation::Standby => "standby",
            Regulation::Cv => "CV",
            Regulation::Cc => "CC",
            Regulation::Fault => "fault",
        }
    }
}

pub trait Charger: Device {
    /// Regulation state, when the supply reports it.
    fn regulation(&mut self) -> Result<Option<Regulation>> {
        Ok(None)
    }

    /// Clamp the supply's own hardware limits, so a crashed or wild
    /// controller still cannot exceed them. This is the one protection that
    /// survives this program dying: everything else here assumes the loop is
    /// still running. Returns whether the supply took them.
    fn arm(&mut self, _max_volts: f64, _max_amps: f64) -> Result<bool> {
        Ok(false)
    }

    fn set(&mut self, volts: f64, amps: f64) -> Result<()>;
    fn start(&mut self) -> Result<()>;
}

/// How an electronic load regulates: the quantity it holds constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMode {
    /// Constant current, in amps. The mode a capacity test wants.
    Cc,
    /// Constant voltage, in volts.
    Cv,
    /// Constant resistance, in ohms.
    Cr,
    /// Constant power, in watts.
    Cp,
}

impl LoadMode {
    pub fn label(self) -> &'static str {
        match self {
            LoadMode::Cc => "CC",
            LoadMode::Cv => "CV",
            LoadMode::Cr => "CR",
            LoadMode::Cp => "CP",
        }
    }

    /// What the setpoint means in this mode.
    pub fn unit(self) -> &'static str {
        match self {
            LoadMode::Cc => "A",
            LoadMode::Cv => "V",
            LoadMode::Cr => "ohm",
            LoadMode::Cp => "W",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct LoadState {
    /// Setpoint the load reports holding, in the units of its mode.
    pub setpoint: f64,
    pub volts: f64,
    pub amps: f64,
    pub watts: f64,
    pub amp_hours: f64,
    pub watt_hours: f64,
    pub temp_c: f64,
    pub runtime_s: f64,
    pub on: bool,
}

pub trait Discharger: Device {
    /// Whether the load can be switched and set from here. A dumb sink cannot,
    /// so the controller must ask the operator instead of commanding, and a
    /// cell-floor cutoff becomes an instruction rather than an action.
    fn controllable(&self) -> bool {
        true
    }

    /// Modes this load can be put into, best first.
    fn modes(&self) -> &'static [LoadMode] {
        &[LoadMode::Cc]
    }

    /// Select a regulation mode and its setpoint. A load that only does
    /// constant current rejects everything else.
    fn set_mode(&mut self, mode: LoadMode, value: f64) -> Result<()> {
        if mode != LoadMode::Cc {
            bail!("{} only does constant current", self.name());
        }
        self.set_current(value)
    }

    fn set_current(&mut self, amps: f64) -> Result<()>;
    fn start(&mut self) -> Result<()>;
    /// Amp-hours counted by the device itself, when it keeps its own total.
    fn amp_hours(&mut self) -> Result<Option<f64>> {
        Ok(None)
    }
    /// Escape hatch for device-specific extras (raw payload dumps, hardware
    /// cutoffs) without widening the trait for one model's features.
    fn as_any(&mut self) -> &mut dyn std::any::Any;

    /// Everything the load knows about the discharge in progress. The default
    /// builds what it can from [`Device::measure`].
    fn state(&mut self) -> Result<LoadState> {
        let s = self.measure()?;
        Ok(LoadState {
            volts: s.volts,
            amps: s.amps,
            watts: s.volts * s.amps,
            amp_hours: self.amp_hours()?.unwrap_or(0.0),
            ..Default::default()
        })
    }
}

pub use crate::discover::{Backend, Transport};

/// The OWON ships with a CH340 bridge, but any adapter will do, so the ids
/// only rank the list rather than filtering it.
pub const CHARGER_BACKENDS: &[Backend] = &[Backend {
    kind: "owon",
    label: "OWON SPE series (SCPI)",
    transport: Transport::Serial {
        usb: Some((0x1a86, 0x7523)),
    },
}];

pub const LOAD_BACKENDS: &[Backend] = &[
    Backend {
        kind: "dl24",
        label: "Atorch DL24 (USB HID)",
        transport: Transport::Usb {
            vid: dl24::VID,
            pid: dl24::PID,
        },
    },
    Backend {
        kind: "oel",
        label: "OWON OEL series load (SCPI)",
        transport: Transport::Serial {
            usb: Some((0x1a86, 0x7523)),
        },
    },
    Backend {
        kind: "passive",
        label: "Uncontrolled load (BMS shunt)",
        transport: Transport::Manual,
    },
];

pub fn open_charger(spec: &str) -> Result<Box<dyn Charger>> {
    let (kind, target) = split(spec);
    match kind {
        "owon" | "spe" => {
            let backend = CHARGER_BACKENDS[0];
            let owned;
            let path = if target.is_empty() {
                owned = backend.default_target()?;
                owned.as_str()
            } else {
                target
            };
            Ok(Box::new(owon::OwonSpe::open(path)?))
        }
        _ => bail!("unknown charger {kind:?}; known: owon"),
    }
}

pub fn open_discharger(spec: &str) -> Result<Box<dyn Discharger>> {
    let (kind, target) = split(spec);
    match kind {
        "dl24" | "atorch" => Ok(Box::new(dl24::Dl24::open(target)?)),
        "oel" | "owon-load" => {
            let owned;
            let path = if target.is_empty() {
                owned = LOAD_BACKENDS
                    .iter()
                    .find(|b| b.kind == "oel")
                    .context("no oel backend")?
                    .default_target()?;
                owned.as_str()
            } else {
                target
            };
            Ok(Box::new(owon_load::OwonLoad::open(path)?))
        }
        "passive" | "manual" => Ok(Box::new(passive::PassiveLoad::open(target)?)),
        _ => bail!("unknown discharger {kind:?}; known: dl24, oel, passive"),
    }
}

fn split(spec: &str) -> (&str, &str) {
    match spec.split_once(':') {
        Some((k, t)) => (k, t),
        None => (spec, ""),
    }
}
