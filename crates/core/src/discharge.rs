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
    /// Coldest the pack may be discharged at. Colder than it may be charged:
    /// taking current out of a cold cell costs capacity, not anode.
    pub temp_min_c: f64,
    /// Stop when the BMS raises an alarm of its own.
    pub stop_on_alarm: bool,
    /// Alarms to keep going through, as case-insensitive substrings.
    pub alarms_ignored: Vec<String>,
    /// Give up if the load is on this long and the pack reports no current
    /// leaving it: a load over its voltage rating, a breaker, an
    /// uncontrolled load nobody switched on, or an electronic load whose own
    /// supply is unplugged. Zero disables the check.
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
            temp_min_c: -20.0,
            stop_on_alarm: true,
            alarms_ignored: vec!["balanc".into()],
            stall_timeout: Duration::from_secs(90),
            stall_current_a: 0.05,
            max_duration: Duration::from_secs(24 * 3600),
            interval: Duration::from_secs(5),
        }
    }
}

/// The settings a discharge will obey while it is already running. The mode
/// is not among them: changing what the load holds constant part way through
/// a capacity test makes the amp-hours measure two different tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    pub setpoint: f64,
    pub cell_floor_mv: u16,
    pub pack_floor_v: f64,
    pub stop_at_soc: Option<u8>,
}

impl Tuning {
    pub fn of(cfg: &Config) -> Self {
        Self {
            setpoint: cfg.setpoint,
            cell_floor_mv: cfg.cell_floor_mv,
            pack_floor_v: cfg.pack_floor_v,
            stop_at_soc: cfg.stop_at_soc,
        }
    }

    pub fn apply(&self, cfg: &mut Config) {
        cfg.setpoint = self.setpoint;
        cfg.cell_floor_mv = self.cell_floor_mv;
        cfg.pack_floor_v = self.pack_floor_v;
        cfg.stop_at_soc = self.stop_at_soc;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    NoCurrent,
    /// The load's output went off without being told: its own cutoff, or a
    /// hand on its panel.
    LoadOff,
    SocTarget,
    CellFloor,
    PackFloor,
    OverTemp,
    /// Too cold to be drawing current from.
    UnderTemp,
    /// The pack's own protection is complaining.
    PackAlarm,
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
    saw_load_on: bool,
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
            saw_load_on: false,
            started: None,
            flowing_since: None,
            last: None,
            finished: None,
        }
    }

    pub fn finished(&self) -> Option<Reason> {
        self.finished
    }

    pub fn retune(&mut self, t: &Tuning) {
        t.apply(&mut self.cfg);
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
            self.saw_load_on |= l.on;
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
        let alarm = self
            .cfg
            .stop_on_alarm
            .then(|| s.blocking_alarm(&self.cfg.alarms_ignored))
            .flatten();
        // The load was sinking and now says its output is off, so the
        // discharge is already over: its own cutoff fired, or someone
        // pressed the button. Waiting for the stall timer would blame the
        // bench for it, and the pack rebounds above the floor meanwhile.
        let quit = self.load_on
            && self.saw_load_on
            && !self.manual
            && load.is_some_and(|l| !l.on);
        let stop = if alarm.is_some() {
            Some(Reason::PackAlarm)
        } else if quit {
            Some(Reason::LoadOff)
        } else if stalled {
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
        } else if s.temp_c < self.cfg.temp_min_c {
            Some(Reason::UnderTemp)
        } else if now.saturating_duration_since(started) >= self.cfg.max_duration {
            Some(Reason::TimeLimit)
        } else {
            None
        };

        match stop {
            Some(r) => {
                self.load_on = false;
                self.finished = Some(r);
                let why = match (&alarm, r) {
                    (Some(a), _) => format!("{r:?} {a}"),
                    // The instrument answered every command and sank
                    // nothing, so the fault is on the bench, and the one
                    // that looks least like a fault is a load with no
                    // supply of its own.
                    (None, Reason::NoCurrent) => {
                        "NoCurrent (leads, rating, or the load's own supply)".into()
                    }
                    (None, Reason::LoadOff) => {
                        "LoadOff (its own cutoff, or switched off at the panel)".into()
                    }
                    (None, _) => format!("{r:?}"),
                };
                self.note = if !s.has_cells() {
                    format!(
                        "stopped on {why}: {:.2} V, {:.3} Ah taken",
                        s.pack_v, self.amp_hours
                    )
                } else if self.manual {
                    format!(
                        "DISCONNECT THE LOAD: {why}, cell {} at {lo} mV, {:.3} Ah taken",
                        s.low_cell(),
                        self.amp_hours
                    )
                } else {
                    format!(
                        "stopped on {why}: cell {} at {lo} mV, {:.3} Ah taken",
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
    fn a_running_discharge_takes_a_new_floor_and_stops_on_it() {
        let mut c = Controller::new(cfg());
        let t = Instant::now();
        assert_eq!(c.step(&snap(&[3200, 3300, 3250]), None, t), None);
        c.retune(&Tuning {
            setpoint: 1.0,
            cell_floor_mv: 3250,
            pack_floor_v: 1.0,
            stop_at_soc: None,
        });
        assert_eq!(c.cfg.setpoint, 1.0);
        assert_eq!(
            c.step(&snap(&[3200, 3300, 3250]), None, t + Duration::from_secs(5)),
            Some(Reason::CellFloor)
        );
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
    fn a_load_that_cuts_off_on_its_own_ends_the_discharge() {
        let mut c = Controller::new(cfg());
        let t = Instant::now();
        let load = |on: bool| {
            Some(LoadState {
                on,
                amp_hours: 1.5,
                ..Default::default()
            })
        };
        // Off on the first poll is the load not started yet, not a cutoff.
        assert_eq!(c.step(&snap(&[3200]), load(false), t), None);
        assert!(c.load_on);
        assert_eq!(c.step(&snap(&[3200]), load(true), t), None);
        // Its own cutoff fires and the pack rebounds above the floor.
        assert_eq!(
            c.step(&snap(&[3200]), load(false), t),
            Some(Reason::LoadOff)
        );
        assert!(!c.load_on);
        assert!(c.note.contains("LoadOff"), "{}", c.note);
    }

    #[test]
    fn a_manual_load_never_reports_itself_on_and_keeps_running() {
        let mut c = Controller::new(cfg());
        c.manual = true;
        let t = Instant::now();
        let off = Some(LoadState::default());
        assert_eq!(c.step(&snap(&[3200]), off, t), None);
        assert_eq!(c.step(&snap(&[3200]), off, t), None);
        assert!(c.load_on);
    }

    #[test]
    fn a_pack_below_its_cold_limit_stops() {
        let mut cold = snap(&[3200]);
        cold.temp_c = -25.0;
        let mut c = Controller::new(cfg());
        assert_eq!(
            c.step(&cold, None, Instant::now()),
            Some(Reason::UnderTemp)
        );
        assert!(!c.load_on);
    }

    #[test]
    fn an_alarm_from_the_pack_stops_the_discharge() {
        let mut s = snap(&[3200]);
        s.alarms = vec!["Discharge over current".into()];
        let mut c = Controller::new(cfg());
        assert_eq!(
            c.step(&s, None, Instant::now()),
            Some(Reason::PackAlarm)
        );
        assert!(!c.load_on);
        assert!(c.note.contains("Discharge over current"), "{}", c.note);
    }

    #[test]
    fn an_ignored_alarm_does_not_stop_the_discharge() {
        let mut s = snap(&[3200]);
        s.alarms = vec!["Balancing".into()];
        let mut c = Controller::new(cfg());
        assert_eq!(c.step(&s, None, Instant::now()), None);
        assert!(c.load_on);
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
