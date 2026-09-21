use crate::device::Charger;
use crate::pack::{Pack, Snapshot};
use anyhow::{Result, bail};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// Charge to the cell ceiling and stop when current reaches the floor.
    Bulk,
    /// Bulk, then hold at the floor current so the balancers can work, until
    /// the lowest cell reaches the target or the hold times out.
    TopBalance,
    /// Stop the moment any cell first touches the ceiling. No taper, no hold:
    /// the fastest way to a known full-ish state before a discharge test.
    Unbalanced,
    /// Bulk at full current, then drop to the float voltage by itself and stay
    /// there until the BMS calls the pack full. What you want for a plain
    /// "fill it up": no setpoint to choose, and the cell ceiling still governs.
    Auto,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub pack_cv: f64,
    /// Where `Auto` holds once the pack is up: below the bulk setpoint, so a
    /// full pack rests instead of being pushed.
    pub float_v: f64,
    /// SOC that ends an `Auto` charge.
    pub soc_target: u8,
    /// Stop at this SOC in any mode. Storage charging: fill to 50% and stop.
    /// Separate from `soc_target` so a partial charge does not change what
    /// "full" means to the automatic mode.
    pub stop_at_soc: Option<u8>,
    pub cell_ceiling_mv: u16,
    pub cell_hard_mv: u16,
    pub cell_resume_mv: u16,
    pub cell_target_mv: u16,
    pub i_max: f64,
    pub i_min: f64,
    pub i_start: f64,
    pub temp_max_c: f64,
    /// Give up if the output is on this long and the pack still reports no
    /// meaningful current: a breaker, a BMS that refuses charge, or a lead
    /// that is not where you think it is. Zero disables the check.
    pub stall_timeout: Duration,
    /// Current below which the pack counts as not charging at all.
    pub stall_current_a: f64,
    /// How far above the CV setpoint counts as a fault on a pack with no
    /// cells to watch. Used only when the pack is blind.
    pub pack_hard_margin_v: f64,
    /// How far below the CV setpoint the pack must fall before current is
    /// ramped again. Blind packs only.
    pub pack_resume_margin_v: f64,
    pub hold_max: Duration,
    pub interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Bulk,
            pack_cv: 51.5,
            float_v: 51.0,
            soc_target: 100,
            stop_at_soc: None,
            cell_ceiling_mv: 3500,
            cell_hard_mv: 3550,
            cell_resume_mv: 3460,
            cell_target_mv: 3450,
            i_max: 3.0,
            i_min: 0.2,
            i_start: 1.0,
            temp_max_c: 45.0,
            stall_timeout: Duration::from_secs(90),
            stall_current_a: 0.05,
            pack_hard_margin_v: 0.5,
            pack_resume_margin_v: 0.6,
            hold_max: Duration::from_secs(48 * 3600),
            interval: Duration::from_secs(20),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Ramping,
    Tapering,
    Floating,
    Holding,
    Done(Reason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Interrupted,
    NoCurrent,
    FloorCurrent,
    SocTarget,
    CeilingReached,
    Balanced,
    HoldTimeout,
    OverTemp,
    LostTelemetry,
}

/// The control loop as a pure state machine, so it can be tested without a
/// battery or a power supply attached.
#[derive(Debug, Clone)]
pub struct Controller {
    pub cfg: Config,
    /// Voltage the charger should hold: the bulk setpoint, or the float
    /// setpoint once `Auto` has topped the pack.
    pub set_v: f64,
    pub set_a: f64,
    pub cap_a: f64,
    pub phase: Phase,
    pub output_on: bool,
    pub note: String,
    hold_since: Option<Instant>,
    flowing_since: Option<Instant>,
    fails: u32,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        Self {
            set_v: cfg.pack_cv,
            set_a: cfg.i_start,
            cap_a: cfg.i_max,
            phase: Phase::Ramping,
            output_on: false,
            note: "starting".into(),
            hold_since: None,
            flowing_since: None,
            fails: 0,
            cfg,
        }
    }

    pub fn finished(&self) -> Option<Reason> {
        match self.phase {
            Phase::Done(r) => Some(r),
            _ => None,
        }
    }

    pub fn on_read_failure(&mut self) -> bool {
        self.fails += 1;
        if self.fails >= 2 {
            self.output_on = false;
            self.phase = Phase::Done(Reason::LostTelemetry);
            self.note = "lost BMS telemetry".into();
            return true;
        }
        false
    }

    /// One control decision from one cell snapshot. `now` is injected so tests
    /// can drive the hold timer without sleeping.
    pub fn step(&mut self, s: &Snapshot, now: Instant) -> Phase {
        self.fails = 0;
        let hi = s.high_mv();
        let lo = s.low_mv();
        let c = &self.cfg;

        // With cells, every limit is a cell limit. Without them (lead-acid, a
        // bare pack, anything on a plain charger) the only thing to watch is
        // the terminal voltage, so the same thresholds are expressed there.
        let blind = !s.has_cells();
        let at_ceiling = if blind {
            s.pack_v >= c.pack_cv
        } else {
            hi >= c.cell_ceiling_mv
        };
        let at_hard = if blind {
            s.pack_v >= c.pack_cv + c.pack_hard_margin_v
        } else {
            hi >= c.cell_hard_mv
        };
        let below_resume = if blind {
            s.pack_v < c.pack_cv - c.pack_resume_margin_v
        } else {
            hi < c.cell_resume_mv
        };

        // A blind pack with no reading at all: the supply is off and nothing
        // else can see the battery, so there is no voltage to regulate to.
        if blind && s.pack_v <= 0.5 && self.output_on {
            self.note = "no voltage reading: is the battery connected?".into();
        }

        // Watchdog: the output is on, so current should be flowing. Charging
        // a pack that is not taking anything means something is in the way.
        // The clock starts when the output is commanded on, not here, or the
        // first poll after switching on would reset it every time.
        if self.output_on && s.current_a.abs() >= c.stall_current_a {
            self.flowing_since = None;
        } else if self.output_on
            && !c.stall_timeout.is_zero()
            // A pack that is full and floating, or sitting in a balance hold,
            // legitimately takes nothing. Those phases end on their own terms.
            && !matches!(self.phase, Phase::Holding | Phase::Floating)
        {
            let since = self.flowing_since.unwrap_or(now);
            if now.saturating_duration_since(since) >= c.stall_timeout {
                self.output_on = false;
                self.phase = Phase::Done(Reason::NoCurrent);
                self.note = format!(
                    "no charge current after {:.0}s at {:.2} A set: check the connection",
                    c.stall_timeout.as_secs_f64(),
                    self.set_a
                );
                return self.phase;
            }
        }

        if s.temp_c > c.temp_max_c {
            self.output_on = false;
            self.note = format!("{:.1} C over limit", s.temp_c);
            self.phase = Phase::Done(Reason::OverTemp);
            return self.phase;
        }

        // A storage charge stops on the pack's own gauge, whatever the mode.
        if let Some(target) = c.stop_at_soc
            && s.soc >= target
        {
            self.output_on = false;
            self.phase = Phase::Done(Reason::SocTarget);
            self.note = format!("stopped at {}% soc, cells {lo}-{hi} mV", s.soc);
            return self.phase;
        }

        if at_ceiling && c.mode == Mode::Unbalanced {
            self.output_on = false;
            self.note = format!("cell {} reached {hi} mV", s.high_cell());
            self.phase = Phase::Done(Reason::CeilingReached);
            return self.phase;
        }

        // Auto drops to float the first time the pack reaches its ceiling,
        // then rides there until the BMS says full.
        if c.mode == Mode::Auto {
            if self.phase == Phase::Floating || at_ceiling {
                self.set_v = c.float_v;
                self.phase = Phase::Floating;
                // A blind pack has no gauge, so "full" is the current falling
                // away at the float voltage instead of a reported SOC.
                let full = if blind {
                    s.current_a.abs() <= c.i_min
                } else {
                    s.soc >= c.soc_target
                };
                if full && !at_hard {
                    self.output_on = false;
                    self.phase = Phase::Done(Reason::SocTarget);
                    self.note = if blind {
                        format!("full: {:.2} A at {:.2} V float", s.current_a, self.set_v)
                    } else {
                        format!("full at {}% soc, cells {lo}-{hi} mV", s.soc)
                    };
                    return self.phase;
                }
                // Holding a lower voltage is itself the current limit, so a
                // floating pack does not taper on the ceiling as well. Only a
                // cell past the hard limit interrupts it.
                if !at_hard {
                    self.output_on = true;
                    self.flowing_since.get_or_insert(now);
                    self.note = format!(
                        "float {:.2} V, {}% soc, cells {lo}-{hi} mV",
                        self.set_v, s.soc
                    );
                    return self.phase;
                }
            } else {
                self.set_v = c.pack_cv;
            }
        }

        if at_hard {
            self.output_on = false;
            self.cap_a = (self.set_a * 0.5).max(c.i_min);
            self.set_a = self.cap_a;
            self.phase = Phase::Tapering;
            self.note = if blind {
                format!(
                    "over {:.2} V, paused, resume at {:.2} A",
                    c.pack_cv + c.pack_hard_margin_v,
                    self.set_a
                )
            } else {
                format!("hard limit {hi} mV, paused, resume at {:.2} A", self.set_a)
            };
        } else if at_ceiling {
            let next = (self.set_a * 0.6).max(c.i_min);
            if next < self.set_a {
                self.cap_a = next;
                self.set_a = next;
                self.phase = Phase::Tapering;
                self.note = if blind {
                    format!("taper to {next:.2} A at {:.2} V", s.pack_v)
                } else {
                    format!("taper to {next:.2} A at {hi} mV")
                };
            } else if self.set_a <= c.i_min + f64::EPSILON {
                return self.at_floor(lo, hi, blind, now);
            }
        } else if below_resume {
            self.output_on = true;
            self.flowing_since.get_or_insert(now);
            if self.set_a < self.cap_a {
                self.set_a = (self.set_a + 0.2).min(self.cap_a);
                self.phase = Phase::Ramping;
                self.note = format!("ramp to {:.2} A (cap {:.2})", self.set_a, self.cap_a);
            } else {
                self.note = format!("holding {:.2} A (cap {:.2})", self.set_a, self.cap_a);
            }
        }
        self.phase
    }

    fn at_floor(&mut self, lo: u16, hi: u16, blind: bool, now: Instant) -> Phase {
        let c = &self.cfg;
        // Balancing is a per-cell idea; a pack with no cells cannot do it.
        if blind || c.mode != Mode::TopBalance || lo >= c.cell_target_mv {
            self.output_on = false;
            self.phase = Phase::Done(if lo >= c.cell_target_mv && c.mode == Mode::TopBalance {
                Reason::Balanced
            } else {
                Reason::FloorCurrent
            });
            self.note = if blind {
                format!("done at floor current, {:.2} V", self.set_v)
            } else {
                format!("done at floor current, cells {lo}-{hi} mV")
            };
            return self.phase;
        }
        let since = *self.hold_since.get_or_insert(now);
        let held = now.saturating_duration_since(since);
        if held >= c.hold_max {
            self.output_on = false;
            self.phase = Phase::Done(Reason::HoldTimeout);
            self.note = format!("hold timed out after {:.1} h, low cell {lo} mV", held.as_secs_f64() / 3600.0);
        } else {
            self.output_on = true;
            self.flowing_since.get_or_insert(now);
            self.phase = Phase::Holding;
            self.note = format!(
                "balance hold {:.1} h at {:.2} A, cells {lo}-{hi} mV",
                held.as_secs_f64() / 3600.0,
                c.i_min
            );
        }
        self.phase
    }
}

/// Drive a real charger and pack until the controller finishes.
pub fn run(
    pack: &mut dyn Pack,
    charger: &mut dyn Charger,
    cfg: Config,
    mut on_sample: impl FnMut(&Snapshot, &Controller),
) -> Result<Reason> {
    if cfg.cell_hard_mv <= cfg.cell_ceiling_mv || cfg.cell_resume_mv > cfg.cell_ceiling_mv {
        bail!("cell thresholds must be resume <= ceiling < hard");
    }
    let mut ctl = Controller::new(cfg.clone());
    charger.stop()?;
    charger.set(cfg.pack_cv, ctl.set_a)?;

    loop {
        if crate::interrupt::requested() {
            charger.stop()?;
            return Ok(Reason::Interrupted);
        }
        match pack.read() {
            Ok(s) => {
                let was_on = ctl.output_on;
                let set_before = ctl.set_a;
                ctl.step(&s, Instant::now());
                if ctl.output_on {
                    if (ctl.set_a - set_before).abs() > f64::EPSILON || !was_on {
                        charger.set(cfg.pack_cv, ctl.set_a)?;
                    }
                    if !was_on {
                        charger.start()?;
                    }
                } else if was_on || ctl.finished().is_some() {
                    charger.stop()?;
                }
                on_sample(&s, &ctl);
                if let Some(reason) = ctl.finished() {
                    charger.stop()?;
                    return Ok(reason);
                }
            }
            Err(e) => {
                eprintln!("BMS read failed: {e}");
                if ctl.on_read_failure() {
                    charger.stop()?;
                    return Ok(Reason::LostTelemetry);
                }
            }
        }
        std::thread::sleep(cfg.interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(cells: &[u16]) -> Snapshot {
        Snapshot {
            cells_mv: cells.to_vec(),
            temp_c: 25.0,
            ..Default::default()
        }
    }

    fn cfg(mode: Mode) -> Config {
        Config {
            mode,
            ..Default::default()
        }
    }

    #[test]
    fn ramps_up_while_cells_are_low() {
        let mut c = Controller::new(cfg(Mode::Bulk));
        let now = Instant::now();
        c.step(&snap(&[3300, 3310]), now);
        assert!(c.output_on);
        assert!((c.set_a - 1.2).abs() < 1e-9);
        c.step(&snap(&[3300, 3310]), now);
        assert!((c.set_a - 1.4).abs() < 1e-9);
    }

    #[test]
    fn tapers_at_ceiling_and_latches_the_cap() {
        let mut c = Controller::new(cfg(Mode::Bulk));
        let now = Instant::now();
        c.set_a = 3.0;
        c.cap_a = 3.0;
        c.step(&snap(&[3300, 3505]), now);
        assert!((c.set_a - 1.8).abs() < 1e-9);
        assert!((c.cap_a - 1.8).abs() < 1e-9);
        // Falling back below resume must not climb past the latched cap.
        for _ in 0..20 {
            c.step(&snap(&[3300, 3400]), now);
        }
        assert!(c.set_a <= 1.8 + 1e-9);
    }

    #[test]
    fn unbalanced_stops_at_first_ceiling_touch() {
        let mut c = Controller::new(cfg(Mode::Unbalanced));
        c.step(&snap(&[3300, 3500]), Instant::now());
        assert_eq!(c.finished(), Some(Reason::CeilingReached));
        assert!(!c.output_on);
    }

    #[test]
    fn bulk_finishes_at_floor_current() {
        let mut c = Controller::new(cfg(Mode::Bulk));
        c.set_a = c.cfg.i_min;
        c.step(&snap(&[3300, 3505]), Instant::now());
        assert_eq!(c.finished(), Some(Reason::FloorCurrent));
    }

    #[test]
    fn top_balance_holds_then_times_out() {
        let mut cf = cfg(Mode::TopBalance);
        cf.hold_max = Duration::from_secs(3600);
        let mut c = Controller::new(cf);
        c.set_a = c.cfg.i_min;
        let t0 = Instant::now();
        assert_eq!(c.step(&snap(&[3300, 3505]), t0), Phase::Holding);
        assert!(c.output_on);
        let late = t0 + Duration::from_secs(3601);
        assert_eq!(c.step(&snap(&[3300, 3505]), late), Phase::Done(Reason::HoldTimeout));
        assert!(!c.output_on);
    }

    #[test]
    fn top_balance_completes_when_low_cell_reaches_target() {
        let mut c = Controller::new(cfg(Mode::TopBalance));
        c.set_a = c.cfg.i_min;
        c.step(&snap(&[3455, 3505]), Instant::now());
        assert_eq!(c.finished(), Some(Reason::Balanced));
    }

    #[test]
    fn auto_bulks_then_floats_until_the_pack_reads_full() {
        let mut c = Controller::new(Config {
            mode: Mode::Auto,
            pack_cv: 51.5,
            float_v: 51.0,
            ..Default::default()
        });
        let now = Instant::now();

        // Bulk: full setpoint, current ramping.
        let mut s = snap(&[3300, 3310]);
        s.soc = 60;
        c.step(&s, now);
        assert_eq!(c.phase, Phase::Ramping);
        assert!((c.set_v - 51.5).abs() < 1e-9);
        assert!(c.output_on);

        // Touching the ceiling hands over to float, without stopping.
        let mut s = snap(&[3300, 3500]);
        s.soc = 96;
        c.step(&s, now);
        assert_eq!(c.phase, Phase::Floating);
        assert!((c.set_v - 51.0).abs() < 1e-9);
        assert!(c.output_on);

        // Float holds while the pack finishes absorbing.
        let mut s = snap(&[3400, 3450]);
        s.soc = 99;
        assert_eq!(c.step(&s, now), Phase::Floating);
        assert!(c.output_on);

        // The BMS calling it full is what ends the charge.
        s.soc = 100;
        assert_eq!(c.step(&s, now), Phase::Done(Reason::SocTarget));
        assert!(!c.output_on);
    }

    #[test]
    fn auto_still_tapers_on_a_runaway_cell() {
        let mut c = Controller::new(Config {
            mode: Mode::Auto,
            ..Default::default()
        });
        c.set_a = 3.0;
        c.cap_a = 3.0;
        let mut s = snap(&[3300, 3560]);
        s.soc = 100;
        // A cell past the hard limit outranks the SOC reading.
        c.step(&s, Instant::now());
        assert!(!c.output_on);
        assert!(c.set_a < 3.0);
    }

    #[test]
    fn a_storage_charge_stops_at_its_soc_in_any_mode() {
        let mut c = Controller::new(Config {
            mode: Mode::Bulk,
            stop_at_soc: Some(50),
            ..Default::default()
        });
        let now = Instant::now();
        let mut s = snap(&[3300, 3310]);
        s.soc = 40;
        c.step(&s, now);
        assert!(c.output_on);
        s.soc = 50;
        assert_eq!(c.step(&s, now), Phase::Done(Reason::SocTarget));
        assert!(!c.output_on);
    }

    #[test]
    fn gives_up_when_the_pack_takes_no_current() {
        let mut c = Controller::new(Config {
            stall_timeout: Duration::from_secs(60),
            ..Default::default()
        });
        let t = Instant::now();
        let s = snap(&[3300, 3310]);
        c.step(&s, t);
        assert!(c.output_on);
        // Still nothing flowing a minute later.
        assert_eq!(
            c.step(&s, t + Duration::from_secs(61)),
            Phase::Done(Reason::NoCurrent)
        );
        assert!(!c.output_on);
    }

    #[test]
    fn current_flowing_keeps_the_watchdog_quiet() {
        let mut c = Controller::new(Config {
            stall_timeout: Duration::from_secs(60),
            ..Default::default()
        });
        let t = Instant::now();
        let mut s = snap(&[3300, 3310]);
        c.step(&s, t);
        s.current_a = 2.0;
        c.step(&s, t + Duration::from_secs(30));
        assert!(c.step(&s, t + Duration::from_secs(120)) != Phase::Done(Reason::NoCurrent));
        assert!(c.output_on);
    }

    fn blind(volts: f64, amps: f64) -> Snapshot {
        Snapshot {
            pack_v: volts,
            current_a: amps,
            temp_c: 25.0,
            ..Default::default()
        }
    }

    #[test]
    fn a_pack_with_no_cells_is_limited_by_its_terminal_voltage() {
        // A 12 V lead-acid battery: absorb at 14.4 V, float at 13.6.
        let mut c = Controller::new(Config {
            mode: Mode::Bulk,
            pack_cv: 14.4,
            float_v: 13.6,
            i_max: 10.0,
            ..Default::default()
        });
        let now = Instant::now();
        c.set_a = 6.0;
        c.cap_a = 6.0;
        c.step(&blind(12.6, 6.0), now);
        assert!(c.output_on, "should still be bulking at 12.6 V");
        // Reaching the absorb voltage tapers, exactly as a cell ceiling does.
        c.step(&blind(14.4, 6.0), now);
        assert!(c.set_a < 6.0);
        assert_eq!(c.phase, Phase::Tapering);
    }

    #[test]
    fn a_blind_auto_charge_ends_when_the_current_falls_away() {
        let mut c = Controller::new(Config {
            mode: Mode::Auto,
            pack_cv: 14.4,
            float_v: 13.6,
            i_min: 0.2,
            ..Default::default()
        });
        let now = Instant::now();
        c.step(&blind(14.4, 4.0), now);
        assert_eq!(c.phase, Phase::Floating);
        assert!((c.set_v - 13.6).abs() < 1e-9);
        // Still absorbing.
        assert_eq!(c.step(&blind(13.6, 1.0), now), Phase::Floating);
        // Accepting nothing: full.
        assert_eq!(
            c.step(&blind(13.6, 0.1), now),
            Phase::Done(Reason::SocTarget)
        );
        assert!(!c.output_on);
    }

    #[test]
    fn over_temperature_stops_everything() {
        let mut c = Controller::new(cfg(Mode::Bulk));
        let mut s = snap(&[3300, 3310]);
        s.temp_c = 50.0;
        c.step(&s, Instant::now());
        assert_eq!(c.finished(), Some(Reason::OverTemp));
        assert!(!c.output_on);
    }

    #[test]
    fn two_read_failures_stop_the_charge() {
        let mut c = Controller::new(cfg(Mode::Bulk));
        assert!(!c.on_read_failure());
        assert!(c.on_read_failure());
        assert_eq!(c.finished(), Some(Reason::LostTelemetry));
    }
}
