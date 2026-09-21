use crate::device::{LoadMode, LoadState};
use crate::pack::Snapshot;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Config {
    /// What the load holds constant, and the value it holds. Current is the
    /// usual choice for a capacity test; constant power keeps the discharge
    /// rate steady as the pack sags, and constant resistance mimics a real
    /// load.
    pub mode: LoadMode,
    pub setpoint: f64,
    /// Stop when any cell reaches this. The point of per-cell monitoring: the
    /// BMS trip is a fault, not a measurement.
    pub cell_floor_mv: u16,
    pub pack_floor_v: f64,
    /// Stop once the pack reports this SOC or less. Storage discharging: run a
    /// full pack down to 50% and stop.
    pub stop_at_soc: Option<u8>,
    pub temp_max_c: f64,
    /// Give up if the load is on this long and the pack reports no current
    /// leaving it: a load over its voltage rating, a breaker, or an
    /// uncontrolled load nobody switched on. Zero disables the check.
    pub stall_timeout: Duration,
    pub stall_current_a: f64,
    pub max_duration: Duration,
    pub interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: LoadMode::Cc,
            setpoint: 3.0,
            cell_floor_mv: 3000,
            pack_floor_v: 45.0,
            stop_at_soc: None,
            temp_max_c: 50.0,
            stall_timeout: Duration::from_secs(90),
            stall_current_a: 0.05,
            max_duration: Duration::from_secs(24 * 3600),
            interval: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    NoCurrent,
    SocTarget,
    CellFloor,
    PackFloor,
    OverTemp,
    TimeLimit,
}

#[derive(Debug, Clone)]
pub struct Controller {
    pub cfg: Config,
    /// Set when the load cannot be switched from here, so the notes tell the
    /// operator what to do instead of pretending the controller did it.
    pub manual: bool,
    pub load_on: bool,
    pub note: String,
    /// Amp-hours taken, counted by the load itself where possible and by
    /// integrating pack current where not.
    pub amp_hours: f64,
    baseline_ah: Option<f64>,
    integrated_ah: f64,
    started: Option<Instant>,
    flowing_since: Option<Instant>,
    last: Option<Instant>,
    finished: Option<Reason>,
}

impl Controller {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            manual: false,
            load_on: false,
            note: "starting".into(),
            amp_hours: 0.0,
            baseline_ah: None,
            integrated_ah: 0.0,
            started: None,
            flowing_since: None,
            last: None,
            finished: None,
        }
    }

    pub fn finished(&self) -> Option<Reason> {
        self.finished
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.started
            .map(|s| now.saturating_duration_since(s))
            .unwrap_or_default()
    }

    pub fn step(&mut self, s: &Snapshot, load: Option<LoadState>, now: Instant) -> Option<Reason> {
        let started = *self.started.get_or_insert(now);
        // The load's own counter is the better number, but it counts from
        // whatever it held when we arrived, so only the delta is ours.
        if let Some(l) = load {
            let base = *self.baseline_ah.get_or_insert(l.amp_hours);
            self.amp_hours = (l.amp_hours - base).max(0.0);
        } else {
            if let Some(prev) = self.last {
                let dt = now.saturating_duration_since(prev).as_secs_f64() / 3600.0;
                self.integrated_ah += s.current_a.abs() * dt;
            }
            self.amp_hours = self.integrated_ah;
        }
        self.last = Some(now);

        // The load says it is on, so the pack should be losing current.
        let flowing = s.current_a.abs() >= self.cfg.stall_current_a;
        if flowing || !self.load_on {
            self.flowing_since = None;
        }
        // Timed from when the load was switched on, not from this poll.
        let stalled = self.load_on
            && !flowing
            && !self.cfg.stall_timeout.is_zero()
            && now.saturating_duration_since(self.flowing_since.unwrap_or(now))
                >= self.cfg.stall_timeout;

        let lo = s.low_mv();
        let stop = if stalled {
            Some(Reason::NoCurrent)
        } else if self.cfg.stop_at_soc.is_some_and(|t| s.soc <= t) {
            Some(Reason::SocTarget)
        // A blind pack has no cell floor to hit, so the pack floor is the
        // only limit and it has to be set for the chemistry.
        } else if s.has_cells() && lo <= self.cfg.cell_floor_mv {
            Some(Reason::CellFloor)
        } else if s.pack_v <= self.cfg.pack_floor_v {
            Some(Reason::PackFloor)
        } else if s.temp_c > self.cfg.temp_max_c {
            Some(Reason::OverTemp)
        } else if now.saturating_duration_since(started) >= self.cfg.max_duration {
            Some(Reason::TimeLimit)
        } else {
            None
        };

        match stop {
            Some(r) => {
                self.load_on = false;
                self.finished = Some(r);
                self.note = if !s.has_cells() {
                    format!(
                        "{} {:.2} {}, {:.2} V, {:.3} Ah taken",
                        self.cfg.mode.label(),
                        self.cfg.setpoint,
                        self.cfg.mode.unit(),
                        s.pack_v,
                        self.amp_hours
                    )
                } else if self.manual {
                    format!(
                        "DISCONNECT THE LOAD: {r:?}, cell {} at {lo} mV, {:.3} Ah taken",
                        s.low_cell(),
                        self.amp_hours
                    )
                } else {
                    format!(
                        "stopped on {r:?}: cell {} at {lo} mV, {:.3} Ah taken",
                        s.low_cell(),
                        self.amp_hours
                    )
                };
            }
            None => {
                self.load_on = true;
                self.flowing_since.get_or_insert(now);
                self.note = if !s.has_cells() {
                    format!(
                        "{} {:.2} {}, {:.2} V, {:.3} Ah taken",
                        self.cfg.mode.label(),
                        self.cfg.setpoint,
                        self.cfg.mode.unit(),
                        s.pack_v,
                        self.amp_hours
                    )
                } else if self.manual {
                    format!(
                        "connect the load, low cell {lo} mV, {:.3} Ah taken (counted from the BMS)",
                        self.amp_hours
                    )
                } else {
                    format!(
                        "{} {:.2} {}, low cell {lo} mV, {:.3} Ah taken",
                        self.cfg.mode.label(),
                        self.cfg.setpoint,
                        self.cfg.mode.unit(),
                        self.amp_hours
                    )
                };
            }
        }
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(cells: &[u16]) -> Snapshot {
        Snapshot {
            pack_v: cells.iter().map(|c| *c as f64).sum::<f64>() / 1000.0,
            cells_mv: cells.to_vec(),
            temp_c: 25.0,
            current_a: -3.0,
            ..Default::default()
        }
    }

    fn cfg() -> Config {
        Config {
            cell_floor_mv: 3000,
            pack_floor_v: 1.0,
            ..Default::default()
        }
    }

    #[test]
    fn runs_until_the_first_cell_hits_its_floor() {
        let mut c = Controller::new(cfg());
        let t = Instant::now();
        assert_eq!(c.step(&snap(&[3200, 3300, 3250]), None, t), None);
        assert!(c.load_on);
        assert_eq!(
            c.step(&snap(&[2999, 3300, 3250]), None, t),
            Some(Reason::CellFloor)
        );
        assert!(!c.load_on);
    }

    #[test]
    fn counts_amp_hours_from_the_load_delta() {
        let mut c = Controller::new(cfg());
        let t = Instant::now();
        let with = |ah: f64| {
            Some(LoadState {
                amp_hours: ah,
                ..Default::default()
            })
        };
        // The load arrives holding 5 Ah from an earlier test.
        c.step(&snap(&[3200]), with(5.0), t);
        assert_eq!(c.amp_hours, 0.0);
        c.step(&snap(&[3200]), with(7.5), t);
        assert!((c.amp_hours - 2.5).abs() < 1e-9);
    }

    #[test]
    fn integrates_pack_current_when_the_load_cannot_count() {
        let mut c = Controller::new(cfg());
        let t = Instant::now();
        c.step(&snap(&[3200]), None, t);
        c.step(&snap(&[3200]), None, t + Duration::from_secs(3600));
        assert!((c.amp_hours - 3.0).abs() < 1e-6);
    }

    #[test]
    fn the_note_names_the_mode_and_its_unit() {
        let mut c = Controller::new(Config {
            mode: LoadMode::Cp,
            setpoint: 150.0,
            ..cfg()
        });
        c.step(&snap(&[3200]), None, Instant::now());
        assert!(c.note.contains("CP 150.00 W"), "{}", c.note);
    }

    #[test]
    fn a_manual_load_is_told_to_the_operator() {
        let mut c = Controller::new(cfg());
        c.manual = true;
        let t = Instant::now();
        c.step(&snap(&[3200]), None, t);
        assert!(c.note.contains("connect the load"));
        c.step(&snap(&[2999]), None, t);
        assert!(c.note.contains("DISCONNECT"));
    }

    #[test]
    fn a_blind_pack_stops_on_the_pack_floor() {
        // 12 V lead-acid: stop at 10.8 V, with no cells to watch.
        let mut c = Controller::new(Config {
            pack_floor_v: 10.8,
            ..cfg()
        });
        let t = Instant::now();
        let blind = |v: f64| Snapshot {
            pack_v: v,
            current_a: -3.0,
            temp_c: 25.0,
            ..Default::default()
        };
        assert_eq!(c.step(&blind(12.0), None, t), None);
        assert!(c.load_on);
        assert_eq!(c.step(&blind(10.7), None, t), Some(Reason::PackFloor));
        assert!(!c.load_on);
    }

    #[test]
    fn a_storage_discharge_stops_at_its_soc() {
        let mut c = Controller::new(Config {
            stop_at_soc: Some(50),
            ..cfg()
        });
        let t = Instant::now();
        let mut s = snap(&[3200]);
        s.soc = 80;
        assert_eq!(c.step(&s, None, t), None);
        s.soc = 50;
        assert_eq!(c.step(&s, None, t), Some(Reason::SocTarget));
        assert!(!c.load_on);
    }

    #[test]
    fn gives_up_when_nothing_is_drawn() {
        let mut c = Controller::new(Config {
            stall_timeout: Duration::from_secs(60),
            ..cfg()
        });
        let t = Instant::now();
        let mut s = snap(&[3200]);
        s.current_a = 0.0;
        assert_eq!(c.step(&s, None, t), None);
        assert!(c.load_on);
        assert_eq!(
            c.step(&s, None, t + Duration::from_secs(61)),
            Some(Reason::NoCurrent)
        );
        assert!(!c.load_on);
    }

    #[test]
    fn stops_on_temperature_and_time() {
        let mut hot = snap(&[3200]);
        hot.temp_c = 60.0;
        let mut c = Controller::new(cfg());
        assert_eq!(c.step(&hot, None, Instant::now()), Some(Reason::OverTemp));

        let mut c = Controller::new(Config {
            max_duration: Duration::from_secs(60),
            ..cfg()
        });
        let t = Instant::now();
        c.step(&snap(&[3200]), None, t);
        assert_eq!(
            c.step(&snap(&[3200]), None, t + Duration::from_secs(61)),
            Some(Reason::TimeLimit)
        );
    }
}
