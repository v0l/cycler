use crate::charge;
use crate::device::{LoadMode, LoadState};
use crate::discharge;
use crate::pack::Snapshot;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Step {
    Charge(charge::Config),
    Rest(Duration),
    Discharge(discharge::Config),
}

impl Step {
    pub fn label(&self) -> String {
        match self {
            Step::Charge(c) => format!("charge {:?}", c.mode),
            Step::Rest(d) => format!("rest {}", minutes(*d)),
            Step::Discharge(d) => {
            format!("discharge {} {:.2} {}", d.mode.label(), d.setpoint, d.mode.unit())
        }
        }
    }
}

fn minutes(d: Duration) -> String {
    let m = d.as_secs_f64() / 60.0;
    if m >= 90.0 {
        format!("{:.1} h", m / 60.0)
    } else {
        format!("{m:.0} m")
    }
}

/// A test to run: the steps in order, repeated. One `repeat` is one cycle.
#[derive(Debug, Clone)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub repeat: usize,
}

impl Plan {
    /// A single charge, which is what the Start button does.
    pub fn charge(cfg: charge::Config) -> Self {
        Self {
            steps: vec![Step::Charge(cfg)],
            repeat: 1,
        }
    }

    /// A single discharge, for emptying a pack by hand while still stopping on
    /// the first cell to reach its floor.
    pub fn discharge(cfg: discharge::Config) -> Self {
        Self {
            steps: vec![Step::Discharge(cfg)],
            repeat: 1,
        }
    }

    /// The capacity test: fill, let it settle, empty it counting amp-hours,
    /// settle again. Repeat to see whether capacity moves between cycles.
    pub fn capacity(
        charge: charge::Config,
        discharge: discharge::Config,
        rest: Duration,
        repeat: usize,
    ) -> Self {
        Self {
            steps: vec![
                Step::Charge(charge),
                Step::Rest(rest),
                Step::Discharge(discharge),
                Step::Rest(rest),
            ],
            repeat,
        }
    }

    pub fn total_steps(&self) -> usize {
        self.steps.len() * self.repeat
    }
}

#[derive(Debug, Clone)]
pub struct StepResult {
    pub cycle: usize,
    pub step: usize,
    pub label: String,
    pub outcome: String,
    /// Amp-hours moved, for the steps that move any.
    pub amp_hours: Option<f64>,
    pub duration: Duration,
}

/// What the runner wants the hardware to do right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Demand {
    pub charger_on: bool,
    pub charger_v: f64,
    pub charger_a: f64,
    pub load_on: bool,
    pub load_mode: LoadMode,
    pub load_value: f64,
}

impl Default for Demand {
    fn default() -> Self {
        Self {
            charger_on: false,
            charger_v: 0.0,
            charger_a: 0.0,
            load_on: false,
            load_mode: LoadMode::Cc,
            load_value: 0.0,
        }
    }
}

enum Active {
    Charge(charge::Controller),
    Rest { until: Instant },
    Discharge(discharge::Controller),
}

pub struct Runner {
    plan: Plan,
    manual_load: bool,
    cycle: usize,
    step: usize,
    active: Option<Active>,
    step_started: Option<Instant>,
    pub results: Vec<StepResult>,
    pub note: String,
    done: bool,
}

impl Runner {
    pub fn new(plan: Plan) -> Self {
        Self {
            plan,
            manual_load: false,
            cycle: 0,
            step: 0,
            active: None,
            step_started: None,
            results: Vec::new(),
            note: "starting".into(),
            done: false,
        }
    }

    /// Tell the runner the load cannot be switched from here.
    pub fn set_manual_load(&mut self, manual: bool) {
        self.manual_load = manual;
    }

    pub fn done(&self) -> bool {
        self.done
    }

    pub fn cycle(&self) -> usize {
        self.cycle
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Index of the running step within the plan, for progress display.
    pub fn step_index(&self) -> usize {
        self.step
    }

    pub fn current_label(&self) -> String {
        self.plan
            .steps
            .get(self.step)
            .map(|s| s.label())
            .unwrap_or_else(|| "done".into())
    }

    /// Amp-hours taken by the most recent discharge, which is the capacity
    /// number the whole exercise exists to produce.
    pub fn measured_ah(&self) -> Option<f64> {
        self.results
            .iter()
            .rev()
            .find(|r| r.label.starts_with("discharge"))
            .and_then(|r| r.amp_hours)
    }

    pub fn step_sample(
        &mut self,
        s: &Snapshot,
        load: Option<LoadState>,
        now: Instant,
    ) -> Demand {
        if self.done {
            return Demand::default();
        }
        let Some(step) = self.plan.steps.get(self.step).cloned() else {
            self.finish();
            return Demand::default();
        };
        if self.active.is_none() {
            self.step_started = Some(now);
            self.active = Some(match &step {
                Step::Charge(c) => Active::Charge(charge::Controller::new(c.clone())),
                Step::Rest(d) => Active::Rest { until: now + *d },
                Step::Discharge(d) => {
                    let mut c = discharge::Controller::new(d.clone());
                    c.manual = self.manual_load;
                    Active::Discharge(c)
                }
            });
        }

        let mut demand = Demand::default();
        let mut finished: Option<(String, Option<f64>)> = None;
        match self.active.as_mut().expect("active step") {
            Active::Charge(c) => {
                c.step(s, now);
                demand.charger_on = c.output_on;
                demand.charger_v = c.set_v;
                demand.charger_a = c.set_a;
                self.note = c.note.clone();
                if let Some(r) = c.finished() {
                    finished = Some((format!("{r:?}"), None));
                }
            }
            Active::Rest { until } => {
                let left = until.saturating_duration_since(now);
                self.note = format!("resting, {} left", minutes(left));
                if left.is_zero() {
                    finished = Some(("Rested".into(), None));
                }
            }
            Active::Discharge(d) => {
                d.step(s, load, now);
                demand.load_on = d.load_on;
                demand.load_mode = d.cfg.mode;
                demand.load_value = d.cfg.setpoint;
                self.note = d.note.clone();
                if let Some(r) = d.finished() {
                    finished = Some((format!("{r:?}"), Some(d.amp_hours)));
                }
            }
        }

        if let Some((outcome, amp_hours)) = finished {
            self.results.push(StepResult {
                cycle: self.cycle + 1,
                step: self.step + 1,
                label: step.label(),
                outcome,
                amp_hours,
                duration: self
                    .step_started
                    .map(|t| now.saturating_duration_since(t))
                    .unwrap_or_default(),
            });
            self.active = None;
            self.step += 1;
            if self.step >= self.plan.steps.len() {
                self.step = 0;
                self.cycle += 1;
                if self.cycle >= self.plan.repeat {
                    self.finish();
                }
            }
            return Demand::default();
        }
        demand
    }

    fn finish(&mut self) {
        self.done = true;
        self.active = None;
        self.note = match self.measured_ah() {
            Some(ah) => format!("plan complete, last discharge {ah:.3} Ah"),
            None => "plan complete".into(),
        };
    }
}

/// Drive a plan against real hardware until it finishes. The UI has its own
/// loop so it can stay responsive; this is the batch version.
pub fn run(
    pack: &mut dyn crate::pack::Pack,
    charger: Option<&mut dyn crate::device::Charger>,
    load: Option<&mut dyn crate::device::Discharger>,
    plan: Plan,
    interval: Duration,
    mut on_sample: impl FnMut(&Snapshot, &Runner, Demand),
) -> anyhow::Result<Vec<StepResult>> {
    let mut runner = Runner::new(plan);
    let mut charger = charger;
    let mut load = load;
    runner.set_manual_load(load.as_ref().map(|l| !l.controllable()).unwrap_or(false));
    let mut have = Demand::default();
    let mut fails = 0u32;

    let stop_all = |charger: &mut Option<&mut dyn crate::device::Charger>,
                    load: &mut Option<&mut dyn crate::device::Discharger>| {
        if let Some(c) = charger.as_mut() {
            let _ = c.stop();
        }
        if let Some(l) = load.as_mut() {
            let _ = l.stop();
        }
    };

    // A blind pack has no opinion about itself, so whatever instrument is
    // connected to it becomes its voltmeter and ammeter.
    let blind = pack.blind();
    loop {
        if crate::interrupt::requested() {
            stop_all(&mut charger, &mut load);
            eprintln!("interrupted: output off");
            return Ok(runner.results);
        }
        if blind {
            let from_load = load
                .as_mut()
                .and_then(|l| l.state().ok())
                .filter(|l| l.on && l.volts > 0.0)
                .map(|l| (l.volts, -l.amps));
            let reading = from_load.or_else(|| {
                charger
                    .as_mut()
                    .and_then(|c| c.measure().ok())
                    .map(|s| (s.volts, s.amps))
            });
            if let Some((v, a)) = reading {
                pack.observe(v, a);
            }
        }
        let snapshot = match pack.read() {
            Ok(s) => {
                fails = 0;
                s
            }
            Err(e) => {
                fails += 1;
                eprintln!("BMS read failed ({fails}): {e:#}");
                if fails >= 2 {
                    stop_all(&mut charger, &mut load);
                    anyhow::bail!("lost BMS telemetry");
                }
                std::thread::sleep(interval);
                continue;
            }
        };
        let load_state = load.as_mut().and_then(|l| l.state().ok());
        let want = runner.step_sample(&snapshot, load_state, Instant::now());

        if let Some(c) = charger.as_mut() {
            if want.charger_on
                && (want.charger_a != have.charger_a || want.charger_v != have.charger_v)
            {
                c.set(want.charger_v, want.charger_a)?;
            }
            if want.charger_on != have.charger_on {
                if want.charger_on {
                    c.start()?;
                } else {
                    c.stop()?;
                }
            }
        }
        if let Some(l) = load.as_mut()
            && l.controllable()
        {
            if want.load_on
                && (want.load_value != have.load_value || want.load_mode != have.load_mode)
            {
                l.set_mode(want.load_mode, want.load_value)?;
            }
            if want.load_on != have.load_on {
                if want.load_on {
                    l.start()?;
                } else {
                    l.stop()?;
                }
            }
        }
        have = want;
        on_sample(&snapshot, &runner, want);

        if runner.done() {
            stop_all(&mut charger, &mut load);
            return Ok(runner.results);
        }
        std::thread::sleep(interval);
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
            ..Default::default()
        }
    }

    fn plan() -> Plan {
        Plan::capacity(
            charge::Config {
                mode: charge::Mode::Unbalanced,
                cell_ceiling_mv: 3500,
                ..Default::default()
            },
            discharge::Config {
                cell_floor_mv: 3000,
                pack_floor_v: 1.0,
                ..Default::default()
            },
            Duration::from_secs(60),
            2,
        )
    }

    #[test]
    fn walks_charge_rest_discharge_rest_and_repeats() {
        let mut r = Runner::new(plan());
        let t = Instant::now();

        // Charge ends the moment a cell touches the ceiling.
        r.step_sample(&snap(&[3300]), None, t);
        assert!(r.current_label().starts_with("charge"));
        r.step_sample(&snap(&[3500]), None, t);
        assert!(r.current_label().starts_with("rest"));

        // Rest runs on the clock.
        r.step_sample(&snap(&[3400]), None, t);
        r.step_sample(&snap(&[3400]), None, t + Duration::from_secs(61));
        assert!(r.current_label().starts_with("discharge"));

        // Discharge ends on the cell floor and records what it took.
        let load = |ah: f64| {
            Some(LoadState {
                amp_hours: ah,
                ..Default::default()
            })
        };
        r.step_sample(&snap(&[3400]), load(0.0), t);
        let d = r.step_sample(&snap(&[3400]), load(9.5), t);
        assert!(d.load_on);
        r.step_sample(&snap(&[2999]), load(9.5), t);
        assert_eq!(r.measured_ah(), Some(9.5));

        // Second cycle starts after the trailing rest.
        r.step_sample(&snap(&[3200]), None, t);
        r.step_sample(&snap(&[3200]), None, t + Duration::from_secs(61));
        assert_eq!(r.cycle(), 1);
        assert!(r.current_label().starts_with("charge"));
        assert!(!r.done());
    }

    #[test]
    fn finishes_after_the_last_repeat() {
        let mut r = Runner::new(Plan {
            steps: vec![Step::Rest(Duration::ZERO)],
            repeat: 2,
        });
        let t = Instant::now();
        r.step_sample(&snap(&[3300]), None, t);
        assert!(!r.done());
        r.step_sample(&snap(&[3300]), None, t);
        assert!(r.done());
        assert_eq!(r.results.len(), 2);
        assert_eq!(r.results[1].cycle, 2);
    }

    #[test]
    fn a_single_charge_is_just_a_one_step_plan() {
        let p = Plan::charge(charge::Config::default());
        assert_eq!(p.total_steps(), 1);
    }
}
