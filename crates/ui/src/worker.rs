use cycler_core::cycle::{Demand, Plan, Runner, StepResult};
use cycler_core::device::LoadState;
use cycler_core::log::{CsvLog, Row};
use cycler_core::pack::Snapshot;
use cycler_core::{Charger, Discharger, Pack, open_charger, open_discharger, open_pack};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

pub enum Command {
    /// What the battery is, for a pack that cannot say so itself.
    SetProfile(cycler_core::chemistry::PackProfile),
    Start(Plan),
    /// Change the currents and clocks of a charge that is already running.
    Tune(cycler_core::charge::Tuning),
    Stop,
    Quit,
}

#[derive(Clone)]
pub struct Update {
    pub at: Instant,
    pub snapshot: Option<Snapshot>,
    pub demand: Demand,
    pub note: String,
    pub running: bool,
    /// How many steps the running plan has. A plain charge is a one-step plan
    /// internally, and should not look like a cycle test in the UI.
    pub plan_steps: usize,
    pub plan_repeat: usize,
    pub cycle: usize,
    pub step_label: String,
    pub step_index: usize,
    pub results: Vec<StepResult>,
    pub measured_ah: Option<f64>,
    pub error: Option<String>,
    pub pack_name: String,
    pub charger_name: String,
    /// Whether a supply is actually open. Without one there is nothing to
    /// charge with, and the controls that pretend otherwise are a lie.
    pub has_charger: bool,
    pub charger: Option<(f64, f64)>,
    /// What the supply says it is doing, not what we asked it to do.
    pub charger_output: Option<bool>,
    pub charger_regulation: Option<cycler_core::device::Regulation>,
    pub load_name: String,
    pub has_load: bool,
    pub load: Option<LoadState>,
    /// A load that cannot be switched from here: a resistor bank, a bulb.
    pub load_manual: bool,
    /// An instrument that does not agree with the battery, or sees no
    /// battery at all while it is switched on.
    pub mismatch: Option<cycler_core::agree::Mismatch>,
    pub load_modes: Vec<cycler_core::device::LoadMode>,
}

pub struct Session {
    pub tx: Sender<Command>,
    pub rx: Receiver<Update>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Stop the hardware and wait for the worker to let go of the ports. A
    /// reconnect that does not wait races the old session for the same serial
    /// device, and whichever loses comes back as "no battery".
    pub fn close(mut self) {
        let _ = self.tx.send(Command::Quit);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Session {
    pub fn spawn(
        pack_spec: String,
        charger_spec: String,
        load_spec: Option<String>,
        log: Option<PathBuf>,
        poll: Duration,
    ) -> Self {
        let (tx, cmd_rx) = channel::<Command>();
        let (up_tx, rx) = channel::<Update>();
        let handle = std::thread::spawn(move || {
            run(pack_spec, charger_spec, load_spec, log, poll, cmd_rx, up_tx)
        });
        Self {
            tx,
            rx,
            handle: Some(handle),
        }
    }
}

struct Devices {
    pack: Option<Box<dyn Pack>>,
    charger: Option<Box<dyn Charger>>,
    load: Option<Box<dyn Discharger>>,
    pack_name: String,
    charger_name: String,
    load_name: String,
    errors: Vec<String>,
}

fn open(pack_spec: &str, charger_spec: &str, load_spec: Option<&str>) -> Devices {
    let mut d = Devices {
        pack: None,
        charger: None,
        load: None,
        pack_name: String::new(),
        charger_name: String::new(),
        load_name: String::new(),
        errors: Vec::new(),
    };
    match open_pack(pack_spec) {
        Ok(p) => {
            d.pack_name = p.name();
            d.pack = Some(p);
        }
        Err(e) => d.errors.push(format!("battery: {e:#}")),
    }
    match open_charger(charger_spec) {
        Ok(mut c) => {
            // A supply left delivering by a killed process is the one state
            // that must never survive a reconnect.
            let mut on = c.output_on().ok().flatten();
            if on.is_none() {
                std::thread::sleep(Duration::from_millis(300));
                on = c.output_on().ok().flatten();
            }
            if on == Some(true) {
                let _ = c.stop();
                d.errors
                    .push("charger was left on; output forced off".into());
            }
            d.charger_name = c.name();
            d.charger = Some(c);
        }
        Err(e) => d.errors.push(format!("charger: {e:#}")),
    }
    if let Some(spec) = load_spec {
        match open_discharger(spec) {
            Ok(l) => {
                d.load_name = l.name();
                d.load = Some(l);
            }
            Err(e) => d.errors.push(format!("load: {e:#}")),
        }
    }
    d
}

/// Send only what changed: the OWON takes a quarter second per command and
/// the DL24 is polled, so re-sending a setpoint every tick costs samples.
/// How far the load's own voltage reading may differ from the pack's before
/// they are clearly not connected to the same battery.
fn mismatched(pack_v: f64, load_v: f64) -> bool {
    cycler_core::agree::disagrees(pack_v, load_v)
}

fn apply(d: &mut Devices, want: Demand, have: Demand, pack_v: f64) -> Result<(), String> {
    if let Some(c) = d.charger.as_mut() {
        if want.charger_on
            && (want.charger_a != have.charger_a || want.charger_v != have.charger_v)
        {
            let _ = c.set(want.charger_v, want.charger_a);
        }
        if want.charger_on != have.charger_on {
            let _ = if want.charger_on { c.start() } else { c.stop() };
        }
    }
    if let Some(l) = d.load.as_mut() {
        if !l.controllable() {
            return Ok(());
        }
        if want.load_on
            && (want.load_value != have.load_value || want.load_mode != have.load_mode)
        {
            let _ = l.set_mode(want.load_mode, want.load_value);
        }
        if want.load_on && !have.load_on {
            // The load measures its own terminals. If that does not match the
            // pack, it is wired to something else, and discharging it would
            // be a test of the wrong battery at best.
            let load_v = l.state().map(|s| s.volts).unwrap_or(0.0);
            if mismatched(pack_v, load_v) {
                return Err(format!(
                    "load sees {load_v:.2} V but the pack is {pack_v:.2} V: \
                     check what the load is connected to"
                ));
            }
        }
        if want.load_on != have.load_on {
            let r = if want.load_on { l.start() } else { l.stop() };
            if let Err(e) = r {
                return Err(format!("{e:#}"));
            }
        }
    }
    Ok(())
}

fn stop_all(d: &mut Devices) {
    if let Some(c) = d.charger.as_mut() {
        let _ = c.stop();
    }
    if let Some(l) = d.load.as_mut() {
        let _ = l.stop();
    }
}

fn run(
    pack_spec: String,
    charger_spec: String,
    load_spec: Option<String>,
    log_path: Option<PathBuf>,
    poll: Duration,
    cmd_rx: Receiver<Command>,
    up_tx: Sender<Update>,
) {
    let mut dev = open(&pack_spec, &charger_spec, load_spec.as_deref());
    let mut runner: Option<Runner> = None;
    let mut demand = Demand::default();
    let mut fails = 0u32;
    // A supply that has just been switched on, and a load that polls slowly,
    // both read nothing for a moment. Three polls in a row is a fault; one
    // is a device catching up.
    let mut disagreeing = 0u32;
    let mut refused: Option<String> = None;
    let mut log = log_path.and_then(|p| match CsvLog::create(&p) {
        Ok(l) => Some(l),
        Err(e) => {
            dev.errors.push(format!("log: {e:#}"));
            None
        }
    });

    let mut wait = Duration::from_millis(0);
    loop {
        // Drain commands, sleeping out the poll interval here rather than at
        // the end of the loop: a Quit then takes effect immediately instead of
        // up to one poll later, which is what makes reconnects deterministic.
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let cmd = if left.is_zero() {
                cmd_rx.try_recv().map_err(|e| match e {
                    TryRecvError::Empty => RecvTimeoutError::Timeout,
                    TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
                })
            } else {
                cmd_rx.recv_timeout(left)
            };
            match cmd {
                // One run at a time. Two plans over one battery would fight
                // over the same supply and load, and the second would inherit
                // a rig the first left mid-stage.
                Ok(Command::Start(_)) if runner.is_some() => {
                    refused = Some("already running: stop it before starting another".into());
                }
                Ok(Command::Start(plan)) => {
                    stop_all(&mut dev);
                    // Clamp the supply in hardware before anything is asked
                    // of it, while there is still someone to tell.
                    if let Some((v, a)) = plan.steps.iter().find_map(|s| match s {
                        cycler_core::cycle::Step::Charge(c) => Some(c.hardware_limits()),
                        _ => None,
                    }) && let Some(c) = dev.charger.as_mut()
                    {
                        match c.arm(v, a) {
                            Ok(true) => {}
                            Ok(false) => {
                                refused = Some(
                                    "this supply has no hardware limits to arm".into(),
                                )
                            }
                            Err(e) => {
                                refused = Some(format!("could not arm the supply: {e:#}"));
                                continue;
                            }
                        }
                    }
                    demand = Demand::default();
                    let mut r = Runner::new(plan);
                    r.set_manual_load(
                        dev.load.as_ref().map(|l| !l.controllable()).unwrap_or(false),
                    );
                    runner = Some(r);
                }
                Ok(Command::SetProfile(p)) => {
                    if let Some(pack) = dev.pack.as_mut() {
                        pack.set_profile(p);
                    }
                }
                Ok(Command::Tune(t)) => {
                    if let Some(r) = runner.as_mut() {
                        r.retune_charge(&t);
                    }
                }
                Ok(Command::Stop) => {
                    stop_all(&mut dev);
                    demand = Demand::default();
                    runner = None;
                }
                Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => {
                    stop_all(&mut dev);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                }
            }
        }
        wait = poll;

        if cycler_core::interrupt::requested() {
            stop_all(&mut dev);
            return;
        }
        let load_state = dev.load.as_mut().and_then(|l| l.state().ok());
        let mut update = Update {
            at: Instant::now(),
            snapshot: None,
            demand,
            note: String::new(),
            running: runner.is_some(),
            plan_steps: runner.as_ref().map(|r| r.plan().steps.len()).unwrap_or(0),
            plan_repeat: runner.as_ref().map(|r| r.plan().repeat).unwrap_or(0),
            cycle: 0,
            step_label: String::new(),
            step_index: 0,
            results: Vec::new(),
            measured_ah: None,
            error: refused
                .take()
                .or_else(|| (!dev.errors.is_empty()).then(|| dev.errors.join("; "))),
            pack_name: dev.pack_name.clone(),
            charger_name: dev.charger_name.clone(),
            has_charger: dev.charger.is_some(),
            charger: dev
                .charger
                .as_mut()
                .and_then(|c| c.measure().ok())
                .map(|s| (s.volts, s.amps)),
            charger_output: dev.charger.as_mut().and_then(|c| c.output_on().ok().flatten()),
            charger_regulation: dev.charger.as_mut().and_then(|c| c.regulation().ok().flatten()),
            load_name: dev.load_name.clone(),
            has_load: dev.load.is_some(),
            load: load_state,
            mismatch: None,
            load_manual: dev.load.as_ref().map(|l| !l.controllable()).unwrap_or(false),
            load_modes: dev.load.as_ref().map(|l| l.modes().to_vec()).unwrap_or_default(),
        };

        // Nothing is running, so nothing may be delivering. This catches a
        // supply left on by a killed session, or switched on at the panel,
        // every poll rather than only at startup.
        if runner.is_none() {
            if update.charger_output == Some(true) {
                if let Some(c) = dev.charger.as_mut() {
                    let _ = c.stop();
                }
                update.charger_output = Some(false);
                update.note = "charger was on with no plan running: forced off".into();
                update.error = Some(update.note.clone());
            }
            if update.load.map(|l| l.on).unwrap_or(false)
                && dev.load.as_ref().map(|l| l.controllable()).unwrap_or(false)
            {
                if let Some(l) = dev.load.as_mut() {
                    let _ = l.stop();
                }
                update.note = "load was on with no plan running: forced off".into();
                update.error = Some(update.note.clone());
            }
        }

        // Same idea in the UI loop: the charger or load is the blind pack's
        // only instrument.
        if dev.pack.as_ref().map(|p| p.blind()).unwrap_or(false) {
            let from_load = load_state.map(|l| (l.volts, l.amps, l.on));
            let from_charger = update
                .charger
                .map(|(v, a)| (v, a, update.charger_output.unwrap_or(false)));
            if let Some(p) = dev.pack.as_mut() {
                match cycler_core::pack::blind_reading(from_charger, from_load) {
                    Some((v, a)) => p.observe(v, a),
                    None => p.lost(),
                }
            }
        }

        match dev.pack.as_mut().map(|p| p.read()) {
            Some(Ok(s)) => {
                fails = 0;
                // A blind pack is read through these same instruments, so it
                // agrees with them by construction: nothing to cross-check.
                if !dev.pack.as_ref().map(|p| p.blind()).unwrap_or(false) {
                    update.mismatch = cycler_core::agree::check(
                        s.pack_v,
                        &[
                            cycler_core::agree::Instrument {
                                who: "the charger",
                                volts: update.charger.map(|(v, _)| v),
                                live: update.charger_output.unwrap_or(false)
                                    || demand.charger_on,
                            },
                            cycler_core::agree::Instrument {
                                who: "the load",
                                volts: load_state.map(|l| l.volts),
                                live: load_state.map(|l| l.on).unwrap_or(false)
                                    || demand.load_on,
                            },
                        ],
                    );
                }
                disagreeing = if update.mismatch.is_some() {
                    disagreeing + 1
                } else {
                    0
                };
                if disagreeing < 3 {
                    update.mismatch = None;
                }
                if let Some(m) = update.mismatch.clone() {
                    let message = m.message();
                    if runner.is_some() {
                        stop_all(&mut dev);
                        demand = Demand::default();
                        runner = None;
                        update.running = false;
                        update.note = format!("stopped: {message}");
                        disagreeing = 0;
                    }
                    update.error = Some(message);
                    update.snapshot = Some(s);
                    let _ = up_tx.send(update);
                    continue;
                }
                if let Some(r) = runner.as_mut() {
                    let want = r.step_sample(&s, load_state, Instant::now());
                    if let Err(e) = apply(&mut dev, want, demand, s.pack_v) {
                        stop_all(&mut dev);
                        demand = Demand::default();
                        update.error = Some(e.clone());
                        update.note = format!("stopped: {e}");
                        update.running = false;
                        runner = None;
                        update.snapshot = Some(s);
                        let _ = up_tx.send(update);
                        continue;
                    }
                    demand = want;
                    update.demand = want;
                    update.note = r.note.clone();
                    update.cycle = r.cycle();
                    update.step_label = r.current_label();
                    update.step_index = r.step_index();
                    update.results = r.results.clone();
                    update.measured_ah = r.measured_ah();
                    if r.done() {
                        stop_all(&mut dev);
                        demand = Demand::default();
                        update.running = false;
                        runner = None;
                    }
                }
                update.snapshot = Some(s);
            }
            Some(Err(e)) => {
                fails += 1;
                update.error = Some(format!("{e:#}"));
                if fails >= 2 && runner.is_some() {
                    stop_all(&mut dev);
                    demand = Demand::default();
                    runner = None;
                    update.running = false;
                    update.note = "stopped: lost BMS telemetry".into();
                }
            }
            None => update.error = Some("no battery".into()),
        }

        if let (Some(l), Some(s)) = (log.as_mut(), update.snapshot.as_ref())
            && let Err(e) = l.write(
                s,
                &Row {
                    set_a: update.demand.charger_a,
                    output_on: update.demand.charger_on,
                    note: &update.note,
                    load: update.load,
                },
            )
        {
            eprintln!("log write: {e:#}");
        }

        if up_tx.send(update).is_err() {
            stop_all(&mut dev);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mismatched;

    #[test]
    fn a_load_on_another_battery_is_spotted() {
        // 15 V on the load while the pack is 51 V: different battery.
        assert!(mismatched(51.2, 15.0));
        // Lead drop and meter error on the same battery: fine.
        assert!(!mismatched(51.2, 51.0));
        assert!(!mismatched(12.6, 12.4));
        // Nothing connected yet says nothing either way.
        assert!(!mismatched(51.2, 0.0));
        assert!(!mismatched(0.0, 12.0));
    }
}
