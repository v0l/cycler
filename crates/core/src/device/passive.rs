//! A load with no control channel: a resistor bank, a bulb, an inverter, or
//! any dumb sink the operator switches by hand. Current comes from the pack's
//! own shunt through the BMS, so the discharge is still measured and still
//! stopped on a cell, just not by a relay.

use super::{Device, Discharger, Limits, LoadState, Sample};
use anyhow::Result;

pub struct PassiveLoad {
    name: String,
    limits: Limits,
}

impl PassiveLoad {
    /// `target` is a free-text description of what is actually wired up, so
    /// the log says "2x 12V 55W bulb" rather than "load".
    pub fn open(target: &str) -> Result<Self> {
        let name = if target.is_empty() {
            "uncontrolled load".to_string()
        } else {
            target.to_string()
        };
        Ok(Self {
            name,
            limits: Limits {
                max_volts: f64::INFINITY,
                max_amps: f64::INFINITY,
                max_watts: f64::INFINITY,
            },
        })
    }
}

impl Device for PassiveLoad {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn limits(&self) -> Limits {
        self.limits.clone()
    }

    /// Nothing to measure here: the pack's current is the measurement, and the
    /// caller already has it.
    fn measure(&mut self) -> Result<Sample> {
        Ok(Sample {
            volts: 0.0,
            amps: 0.0,
        })
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    fn output_on(&mut self) -> Result<Option<bool>> {
        Ok(None)
    }
}

impl Discharger for PassiveLoad {
    /// The whole point: the controller must not believe it can switch this.
    fn controllable(&self) -> bool {
        false
    }

    fn set_current(&mut self, _amps: f64) -> Result<()> {
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    fn amp_hours(&mut self) -> Result<Option<f64>> {
        Ok(None)
    }

    fn state(&mut self) -> Result<LoadState> {
        Ok(LoadState::default())
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
