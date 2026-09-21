//! The charge control loop, as the standard stages a proper charger runs:
//! pre-charge, bulk (CC), absorption (CV), and then either a stop, a balance
//! hold, or a float for the chemistries that want one.
//!
//! The supply does the regulating. `cycler` sets a voltage and a current
//! ceiling and lets the supply sit in CC or CV as physics dictates; the only
//! thing the loop adds on top is the cell knowledge a dumb charger lacks,
//! pulling the voltage setpoint down when one cell runs ahead of the pack.

use crate::chemistry::{Chemistry, PackProfile};
use crate::device::Charger;
use crate::pack::{Pack, Snapshot};
use anyhow::{Result, bail};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// The stages a proper charger runs: pre-charge a flat pack, bulk to the
    /// absorb voltage, absorb until the current tails off, then stop. A
    /// chemistry that wants a float gets one.
    Standard,
    /// Standard, then hold at the ceiling on a small current so the balancers
    /// can work, until the lowest cell comes up or the hold times out.
    TopBalance,
    /// Constant current only: stop the moment the pack or the first cell
    /// touches the ceiling, with no absorption. The fastest way to a known
    /// full-ish state before a discharge test, and the pack is left however
    /// balanced it was.
    BulkOnly,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    /// Absorption setpoint: where bulk ends and the current starts tapering.
    pub v_absorb: f64,
    /// Maintenance voltage, for a pack that floats. Never reached by a
    /// chemistry that terminates instead.
    pub v_float: f64,
    /// Sag that sends a floating pack back to bulk.
    pub v_recharge: f64,
    /// Below this the pack is too flat for full current.
    pub v_precharge: f64,
    /// How far above the setpoint counts as the supply misbehaving.
    pub v_hard_margin: f64,
    /// How far under the setpoint still counts as regulating in CV, which is
    /// what makes a low current mean "full" rather than "not connected".
    pub v_cv_slack: f64,
    pub i_max: f64,
    pub i_start: f64,
    /// Smallest current worth commanding.
    pub i_min: f64,
    /// Current fed to a pack that is under `v_precharge`.
    pub i_precharge: f64,
    /// Termination current: absorption is over when the pack stops taking
    /// this much at the absorb voltage. C/20 is the usual figure.
    pub i_term: f64,
    /// How long the current has to stay under `i_term` first, so one low
    /// reading cannot call a pack full.
    pub i_term_for: Duration,
    /// Safety net on the absorption stage.
    pub absorb_max: Duration,
    pub precharge_max: Duration,
    /// Hold at `v_float` after termination instead of stopping. Lead-acid
    /// wants this; sitting a lithium pack at a float voltage only ages it.
    pub maintain_float: bool,
    pub cell_ceiling_mv: u16,
    pub cell_hard_mv: u16,
    pub cell_target_mv: u16,
    /// Below this a cell is too flat for full current.
    pub cell_precharge_mv: u16,
    /// SOC that ends a charge early. Only ever believed from a real gauge.
    pub soc_target: u8,
    /// Stop at this SOC in any mode. Storage charging: fill to 50% and stop.
    /// Separate from `soc_target` so a partial charge does not change what
    /// "full" means to the automatic mode.
    pub stop_at_soc: Option<u8>,
    pub temp_max_c: f64,
    /// Give up if the output is on this long and the pack still reports no
    /// meaningful current: a breaker, a BMS that refuses charge, or a lead
    /// that is not where you think it is. Zero disables the check.
    pub stall_timeout: Duration,
    /// Current below which the pack counts as not charging at all.
    pub stall_current_a: f64,
    /// Longest balance hold, and the longest a float is maintained.
    pub hold_max: Duration,
    pub interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Standard,
            v_absorb: 51.5,
            v_float: 51.0,
            v_recharge: 49.5,
            v_precharge: 37.5,
            v_hard_margin: 0.5,
            v_cv_slack: 0.25,
            i_max: 3.0,
            i_start: 1.0,
            i_min: 0.2,
            i_precharge: 0.5,
            i_term: 0.2,
            i_term_for: Duration::from_secs(120),
            absorb_max: Duration::from_secs(6 * 3600),
            precharge_max: Duration::from_secs(30 * 60),
            maintain_float: false,
            cell_ceiling_mv: 3500,
            cell_hard_mv: 3550,
            cell_target_mv: 3450,
            cell_precharge_mv: 2800,
            soc_target: 100,
            stop_at_soc: None,
            temp_max_c: 45.0,
            stall_timeout: Duration::from_secs(90),
            stall_current_a: 0.05,
            hold_max: Duration::from_secs(48 * 3600),
            interval: Duration::from_secs(20),
        }
    }
}

impl Config {
    /// Every limit derived from what the pack is, which is the only place
    /// they should come from: a chemistry, a series count and a capacity.
    pub fn for_profile(p: &PackProfile) -> Self {
        let cell = p.cell();
        let series = p.series.max(1) as f64;
        let pack_v = |mv: u16| series * mv as f64 / 1000.0;
        let capacity = p.capacity_ah();
        Self {
            v_absorb: p.charge_v(),
            v_float: p.float_v(),
            v_recharge: pack_v(cell.float_mv.saturating_sub(100)),
            v_precharge: pack_v(cell.floor_mv.saturating_sub(200)),
            i_max: p.current_at_c(p.chemistry.default_charge_c()),
            i_start: p.current_at_c(p.chemistry.default_charge_c() / 4.0),
            i_min: (capacity / 100.0).max(0.1),
            i_precharge: (capacity / 20.0).max(0.1),
            i_term: p.current_at_c(p.chemistry.default_termination_c()),
            maintain_float: p.chemistry == Chemistry::LeadAcid,
            cell_ceiling_mv: cell.ceiling_mv,
            cell_hard_mv: cell.ceiling_mv + 50,
            cell_target_mv: cell.balance_mv,
            cell_precharge_mv: cell.floor_mv.saturating_sub(200),
            ..Default::default()
        }
    }
}

/// What may be changed while a charge is running.
///
/// Currents and clocks only. Nothing here can raise a voltage or move a cell
/// limit, because those decide what "full" means and what is safe, and a pack
/// half way up a charge is not the moment to renegotiate either.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tuning {
    pub i_max: f64,
    pub i_term: f64,
    pub absorb_max: Duration,
    pub hold_max: Duration,
    pub stop_at_soc: Option<u8>,
}

impl Tuning {
    pub fn of(cfg: &Config) -> Self {
        Self {
            i_max: cfg.i_max,
            i_term: cfg.i_term,
            absorb_max: cfg.absorb_max,
            hold_max: cfg.hold_max,
            stop_at_soc: cfg.stop_at_soc,
        }
    }

    /// Apply to a config, keeping the values inside what the machine can
    /// still act on: a termination current at or above the charge current
    /// would end the charge the moment it was typed.
    pub fn apply(&self, cfg: &mut Config) {
        cfg.i_max = self.i_max.max(cfg.i_min);
        cfg.i_term = self.i_term.clamp(0.01, cfg.i_max * 0.5);
        cfg.absorb_max = self.absorb_max;
        cfg.hold_max = self.hold_max;
        cfg.stop_at_soc = self.stop_at_soc;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// A flat pack, fed a small current until it is fit for a real one.
    Precharge,
    /// Constant current. The pack decides the voltage.
    Bulk,
    /// Constant voltage at the absorb setpoint, current tapering away.
    Absorb,
    /// Held at the ceiling on a small current for the balancers.
    Balance,
    /// Maintenance: held at the float voltage, taking whatever it needs.
    Float,
    Done(Reason),
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Precharge => "pre-charge",
            Phase::Bulk => "bulk",
            Phase::Absorb => "absorb",
            Phase::Balance => "balance",
            Phase::Float => "float",
            Phase::Done(_) => "done",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Interrupted,
    NoCurrent,
    /// The normal ending: the current fell to the termination figure.
    Terminated,
    SocTarget,
    CeilingReached,
    Balanced,
    AbsorbTimeout,
    HoldTimeout,
    PrechargeFailed,
    OverVoltage,
    OverTemp,
    LostTelemetry,
}

/// The control loop as a pure state machine, so it can be tested without a
/// battery or a power supply attached.
#[derive(Debug, Clone)]
pub struct Controller {
    pub cfg: Config,
    pub set_v: f64,
    pub set_a: f64,
    pub phase: Phase,
    pub output_on: bool,
    pub note: String,
    /// The absorb setpoint after the cell loop has pulled it down for the
    /// highest cell. Never above `v_absorb`.
    limit_v: f64,
    phase_since: Option<Instant>,
    tail_since: Option<Instant>,
    flowing_since: Option<Instant>,
    hard_cuts: u32,
    fails: u32,
}

const HARD_CUTS_MAX: u32 = 5;

impl Controller {
    pub fn new(cfg: Config) -> Self {
        Self {
            set_v: cfg.v_absorb,
            set_a: cfg.i_start.min(cfg.i_max),
            limit_v: cfg.v_absorb,
            phase: Phase::Bulk,
            output_on: false,
            note: "starting".into(),
            phase_since: None,
            tail_since: None,
            flowing_since: None,
            hard_cuts: 0,
            fails: 0,
            cfg,
        }
    }

    /// Change the currents and clocks mid-charge. The stage machine keeps
    /// running: a lower current takes effect on the next command to the
    /// supply, and a changed termination figure is judged from the next
    /// sample, including the tail already counted.
    pub fn retune(&mut self, t: &Tuning) {
        t.apply(&mut self.cfg);
        self.set_a = self.set_a.min(self.cfg.i_max);
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

    fn enter(&mut self, phase: Phase, now: Instant) {
        if self.phase != phase {
            self.phase = phase;
            self.phase_since = Some(now);
            self.tail_since = None;
        }
    }

    fn elapsed(&self, now: Instant) -> Duration {
        self.phase_since
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or_default()
    }

    fn done(&mut self, reason: Reason, note: String) -> Phase {
        self.output_on = false;
        self.phase = Phase::Done(reason);
        self.note = note;
        self.phase
    }

    /// One control decision from one pack reading. `now` is injected so tests
    /// can drive the stage timers without sleeping.
    pub fn step(&mut self, s: &Snapshot, now: Instant) -> Phase {
        self.fails = 0;
        if self.finished().is_some() {
            return self.phase;
        }
        self.phase_since.get_or_insert(now);
        let c = self.cfg.clone();
        let blind = !s.has_cells();
        let hi = s.high_mv();
        let lo = s.low_mv();
        let cells = if blind {
            String::new()
        } else {
            format!(", cells {lo}-{hi} mV")
        };

        if s.temp_c > c.temp_max_c {
            return self.done(Reason::OverTemp, format!("{:.1} C over limit", s.temp_c));
        }

        if let Some(target) = c.stop_at_soc
            && s.soc >= target
        {
            return self.done(
                Reason::SocTarget,
                format!("stopped at {}% soc{cells}", s.soc),
            );
        }

        // On the way up, only the first ceiling touch matters.
        if c.mode == Mode::BulkOnly && self.at_ceiling(s, blind) {
            return self.done(
                Reason::CeilingReached,
                if blind {
                    format!("reached {:.2} V", s.pack_v)
                } else {
                    format!("cell {} reached {hi} mV", s.high_cell())
                },
            );
        }

        // A cell over its hard limit, or a supply overshooting its own
        // setpoint, is not something to regulate around: drop the output,
        // pull the setpoint down, and give up if it keeps happening.
        let over_hard = if blind {
            self.output_on && s.pack_v >= self.set_v + c.v_hard_margin
        } else {
            hi >= c.cell_hard_mv
        };
        if over_hard {
            self.hard_cuts += 1;
            if self.hard_cuts > HARD_CUTS_MAX {
                return self.done(
                    Reason::OverVoltage,
                    format!("over the hard limit {HARD_CUTS_MAX} times, giving up{cells}"),
                );
            }
            self.output_on = false;
            self.limit_v = (self.limit_v - 0.1).max(c.v_absorb * 0.5);
            self.set_v = self.limit_v;
            self.tail_since = None;
            self.note = if blind {
                format!("over {:.2} V, paused, setpoint now {:.2} V", self.set_v + c.v_hard_margin, self.limit_v)
            } else {
                format!("hard limit {hi} mV, paused, setpoint now {:.2} V", self.limit_v)
            };
            return self.phase;
        }

        self.track_cells(s, blind);
        if self.stalled(s, now) {
            return self.done(
                Reason::NoCurrent,
                format!(
                    "no charge current after {:.0}s at {:.2} A set: check the connection",
                    c.stall_timeout.as_secs_f64(),
                    self.set_a
                ),
            );
        }

        // First reading of the charge: a pack that is flat enough gets the
        // gentle stage before the real one.
        if self.phase == Phase::Bulk
            && self.phase_since == Some(now)
            && c.mode == Mode::Standard
            && self.too_flat(s, blind)
        {
            self.enter(Phase::Precharge, now);
        }

        match self.phase {
            Phase::Precharge => self.precharge(s, blind, now),
            Phase::Bulk => self.bulk(s, blind, now, &cells),
            Phase::Absorb => self.absorb(s, blind, now, &cells),
            Phase::Balance => self.balance(s, lo, hi, now),
            Phase::Float => self.float(s, now, &cells),
            Phase::Done(_) => self.phase,
        }
    }

    fn at_ceiling(&self, s: &Snapshot, blind: bool) -> bool {
        if blind {
            s.pack_v >= self.limit_v - 0.02
        } else {
            s.high_mv() >= self.cfg.cell_ceiling_mv || s.pack_v >= self.limit_v - 0.02
        }
    }

    fn too_flat(&self, s: &Snapshot, blind: bool) -> bool {
        if blind {
            s.pack_v > 0.5 && s.pack_v < self.cfg.v_precharge
        } else {
            s.low_mv() < self.cfg.cell_precharge_mv
        }
    }

    /// The cell loop: one cell over the ceiling pulls the whole setpoint
    /// down by its overshoot, and the setpoint creeps back up once it is no
    /// longer in the way. This is the entire difference between charging a
    /// pack and charging a battery.
    fn track_cells(&mut self, s: &Snapshot, blind: bool) {
        if blind {
            return;
        }
        let c = &self.cfg;
        let over = s.high_mv() as i32 - c.cell_ceiling_mv as i32;
        if over > 0 {
            let back_off = (over as f64 / 1000.0).min(0.2);
            self.limit_v = (self.limit_v - back_off).max(c.v_absorb * 0.5);
        } else if over < -20 && self.limit_v < c.v_absorb {
            self.limit_v = (self.limit_v + 0.02).min(c.v_absorb);
        }
    }

    fn stalled(&mut self, s: &Snapshot, now: Instant) -> bool {
        let c = &self.cfg;
        // A pack held at the ceiling or sitting on a float legitimately takes
        // nothing; those stages end on their own terms.
        if matches!(self.phase, Phase::Balance | Phase::Float) || c.stall_timeout.is_zero() {
            return false;
        }
        if !self.output_on {
            return false;
        }
        if s.current_a.abs() >= c.stall_current_a {
            self.flowing_since = None;
            return false;
        }
        let since = *self.flowing_since.get_or_insert(now);
        now.saturating_duration_since(since) >= c.stall_timeout
    }

    fn precharge(&mut self, s: &Snapshot, blind: bool, now: Instant) -> Phase {
        let c = self.cfg.clone();
        if !self.too_flat(s, blind) {
            self.enter(Phase::Bulk, now);
            return self.bulk(s, blind, now, "");
        }
        if self.elapsed(now) >= c.precharge_max {
            return self.done(
                Reason::PrechargeFailed,
                format!(
                    "still under {:.2} V after {:.0} min of pre-charge",
                    c.v_precharge,
                    c.precharge_max.as_secs_f64() / 60.0
                ),
            );
        }
        self.output_on = true;
        self.flowing_since.get_or_insert(now);
        self.set_v = self.limit_v;
        self.set_a = c.i_precharge;
        self.note = format!(
            "pre-charge {:.2} A at {:.2} V, {:.0} min in",
            self.set_a,
            s.pack_v,
            self.elapsed(now).as_secs_f64() / 60.0
        );
        self.phase
    }

    fn bulk(&mut self, s: &Snapshot, blind: bool, now: Instant, cells: &str) -> Phase {
        let c = self.cfg.clone();
        if self.at_ceiling(s, blind) {
            self.enter(Phase::Absorb, now);
            return self.absorb(s, blind, now, cells);
        }
        self.output_on = true;
        self.flowing_since.get_or_insert(now);
        self.set_v = self.limit_v;
        // Soft start rather than slamming the full current into a cold pack.
        if self.set_a < c.i_max {
            self.set_a = (self.set_a + 0.2).min(c.i_max);
        }
        self.note = format!(
            "bulk {:.1} h, {:.2} A at {:.2} V (to {:.2}){cells}",
            self.elapsed(now).as_secs_f64() / 3600.0,
            s.current_a,
            s.pack_v,
            self.limit_v
        );
        self.phase
    }

    fn absorb(&mut self, s: &Snapshot, blind: bool, now: Instant, cells: &str) -> Phase {
        let c = self.cfg.clone();
        self.output_on = true;
        self.flowing_since.get_or_insert(now);
        self.set_v = self.limit_v;
        self.set_a = c.i_max;

        // A gauge that measures rather than guesses is allowed to end it.
        if c.mode == Mode::Standard && !s.soc_estimated && s.has_cells() && s.soc >= c.soc_target {
            return self.finish_absorb(
                Reason::SocTarget,
                format!("full at {}% soc{cells}", s.soc),
                now,
                s,
            );
        }

        // Termination, and the whole reason absorption exists: the current
        // has to have fallen away *while the supply is holding the voltage*.
        // A low current with the voltage nowhere near the setpoint means the
        // charge is not happening, not that the pack is full.
        // "Holding the voltage" is either setpoint: the supply's, or the cell
        // ceiling the loop below it is regulating to, which on an unbalanced
        // pack binds well before the pack reaches the absorb voltage.
        let at_limit = s.pack_v >= self.limit_v - c.v_cv_slack
            || (!blind && s.high_mv() + 10 >= c.cell_ceiling_mv);
        let tail = at_limit && s.current_a.abs() <= c.i_term;
        if tail {
            self.tail_since.get_or_insert(now);
        } else {
            self.tail_since = None;
        }
        let tail_for = self
            .tail_since
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or_default();
        if self.tail_since.is_some() && tail_for >= c.i_term_for {
            return self.finish_absorb(
                Reason::Terminated,
                format!(
                    "terminated: {:.2} A for {:.0}s at {:.2} V{cells}",
                    s.current_a,
                    tail_for.as_secs_f64(),
                    s.pack_v
                ),
                now,
                s,
            );
        }
        if self.elapsed(now) >= c.absorb_max {
            return self.finish_absorb(
                Reason::AbsorbTimeout,
                format!(
                    "absorb ran {:.1} h without terminating{cells}",
                    c.absorb_max.as_secs_f64() / 3600.0
                ),
                now,
                s,
            );
        }
        self.note = format!(
            "absorb {:.1} h, {:.2} V, {:.2} A (term {:.2}, tail {:.0}s){cells}",
            self.elapsed(now).as_secs_f64() / 3600.0,
            self.limit_v,
            s.current_a,
            c.i_term,
            tail_for.as_secs_f64()
        );
        self.phase
    }

    /// What follows a completed absorption: a balance hold, a float, or the
    /// end of the charge.
    fn finish_absorb(&mut self, reason: Reason, note: String, now: Instant, s: &Snapshot) -> Phase {
        if self.cfg.mode == Mode::TopBalance {
            self.enter(Phase::Balance, now);
            return self.balance(s, s.low_mv(), s.high_mv(), now);
        }
        if self.cfg.maintain_float {
            self.enter(Phase::Float, now);
            return self.float(s, now, "");
        }
        self.done(reason, note)
    }

    fn balance(&mut self, s: &Snapshot, lo: u16, hi: u16, now: Instant) -> Phase {
        let c = self.cfg.clone();
        if !s.has_cells() || lo >= c.cell_target_mv {
            return self.done(
                Reason::Balanced,
                format!("low cell reached {lo} mV after balancing"),
            );
        }
        let held = self.elapsed(now);
        if held >= c.hold_max {
            return self.done(
                Reason::HoldTimeout,
                format!(
                    "hold timed out after {:.1} h, low cell {lo} mV",
                    held.as_secs_f64() / 3600.0
                ),
            );
        }
        self.output_on = true;
        self.set_v = self.limit_v;
        self.set_a = c.i_min;
        self.note = format!(
            "balance hold {:.1} h at {:.2} A, cells {lo}-{hi} mV",
            held.as_secs_f64() / 3600.0,
            self.set_a
        );
        self.phase
    }

    fn float(&mut self, s: &Snapshot, now: Instant, cells: &str) -> Phase {
        let c = self.cfg.clone();
        if s.pack_v <= c.v_recharge {
            self.limit_v = c.v_absorb;
            self.set_a = c.i_start.min(c.i_max);
            self.enter(Phase::Bulk, now);
            return self.bulk(s, !s.has_cells(), now, cells);
        }
        let held = self.elapsed(now);
        if held >= c.hold_max {
            return self.done(
                Reason::HoldTimeout,
                format!("floated {:.1} h{cells}", held.as_secs_f64() / 3600.0),
            );
        }
        self.output_on = true;
        self.set_v = c.v_float;
        self.set_a = c.i_max;
        self.note = format!(
            "float {:.2} V, {:.2} A, {:.1} h{cells}",
            c.v_float,
            s.current_a,
            held.as_secs_f64() / 3600.0
        );
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
    if cfg.cell_hard_mv <= cfg.cell_ceiling_mv {
        bail!("the hard cell limit must sit above the ceiling");
    }
    if cfg.v_float > cfg.v_absorb || cfg.v_recharge > cfg.v_float {
        bail!("voltages must be recharge <= float <= absorb");
    }
    if cfg.i_term <= 0.0 || cfg.i_term >= cfg.i_max {
        bail!("the termination current must be above zero and below the charge current");
    }
    let mut ctl = Controller::new(cfg.clone());
    charger.stop()?;
    charger.set(ctl.set_v, ctl.set_a)?;

    loop {
        if crate::interrupt::requested() {
            charger.stop()?;
            return Ok(Reason::Interrupted);
        }
        match pack.read() {
            Ok(s) => {
                let was_on = ctl.output_on;
                let before = (ctl.set_v, ctl.set_a);
                ctl.step(&s, Instant::now());
                if ctl.output_on {
                    if !was_on
                        || (ctl.set_v - before.0).abs() > f64::EPSILON
                        || (ctl.set_a - before.1).abs() > f64::EPSILON
                    {
                        charger.set(ctl.set_v, ctl.set_a)?;
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

    const MINUTE: Duration = Duration::from_secs(60);

    fn cfg(mode: Mode) -> Config {
        Config {
            mode,
            i_term: 0.2,
            i_term_for: Duration::from_secs(120),
            ..Default::default()
        }
    }

    /// A pack reading with cells, at a pack voltage consistent with them.
    fn snap(cells: &[u16], amps: f64) -> Snapshot {
        let pack_v = cells.iter().map(|c| *c as f64 / 1000.0).sum();
        Snapshot {
            cells_mv: cells.to_vec(),
            pack_v,
            current_a: amps,
            temp_c: 25.0,
            ..Default::default()
        }
    }

    fn cells_at(mv: u16, n: usize) -> Vec<u16> {
        vec![mv; n]
    }

    /// A pack with no BMS: a voltage and a current, nothing else.
    fn blind(volts: f64, amps: f64) -> Snapshot {
        Snapshot {
            pack_v: volts,
            current_a: amps,
            temp_c: 25.0,
            ..Default::default()
        }
    }

    /// Feed the same reading for a span of time, one sample a minute.
    fn hold(c: &mut Controller, s: &Snapshot, from: Instant, span: Duration) -> Instant {
        let mut t = from;
        while t < from + span {
            t += MINUTE;
            c.step(s, t);
        }
        t
    }

    #[test]
    fn bulk_holds_the_absorb_setpoint_and_ramps_the_current() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        c.step(&snap(&cells_at(3300, 15), 1.0), t);
        assert_eq!(c.phase, Phase::Bulk);
        assert!(c.output_on);
        assert!((c.set_v - 51.5).abs() < 1e-9, "{}", c.set_v);
        assert!((c.set_a - 1.2).abs() < 1e-9);
        c.step(&snap(&cells_at(3300, 15), 1.2), t + MINUTE);
        assert!((c.set_a - 1.4).abs() < 1e-9);
    }

    #[test]
    fn reaching_the_absorb_voltage_ends_bulk() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        c.step(&blind(48.0, 3.0), t);
        assert_eq!(c.phase, Phase::Bulk);
        c.step(&blind(51.5, 3.0), t + MINUTE);
        assert_eq!(c.phase, Phase::Absorb);
        // The current ceiling stays at maximum: the supply is in CV now and
        // tapers the current itself.
        assert!((c.set_a - c.cfg.i_max).abs() < 1e-9);
        assert!(c.output_on);
    }

    #[test]
    fn absorption_terminates_on_the_tail_current() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        c.step(&blind(51.5, 3.0), t);
        assert_eq!(c.phase, Phase::Absorb);
        let t = hold(&mut c, &blind(51.5, 1.0), t, 10 * MINUTE);
        assert_eq!(c.phase, Phase::Absorb, "still taking current");
        // Under the termination current, but not yet for long enough.
        c.step(&blind(51.5, 0.15), t + MINUTE);
        assert_eq!(c.phase, Phase::Absorb);
        let t = hold(&mut c, &blind(51.5, 0.15), t, 3 * MINUTE);
        assert_eq!(c.phase, Phase::Done(Reason::Terminated));
        assert!(!c.output_on);
        let _ = t;
    }

    #[test]
    fn a_low_current_away_from_the_setpoint_is_not_a_full_pack() {
        // 48 V with a 51.5 V setpoint means the supply is not in CV: the
        // pack is not being charged at all, whatever the ammeter says.
        let mut c = Controller::new(Config {
            stall_timeout: Duration::ZERO,
            ..cfg(Mode::Standard)
        });
        let t = Instant::now();
        c.step(&blind(51.5, 3.0), t);
        assert_eq!(c.phase, Phase::Absorb);
        hold(&mut c, &blind(48.0, 0.0), t, 30 * MINUTE);
        assert_eq!(c.phase, Phase::Absorb, "must not call this terminated");
    }

    #[test]
    fn the_tail_timer_restarts_when_the_pack_takes_current_again() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        c.step(&blind(51.5, 3.0), t);
        let t = hold(&mut c, &blind(51.5, 0.1), t, MINUTE);
        let t = hold(&mut c, &blind(51.5, 1.0), t, MINUTE);
        let t = hold(&mut c, &blind(51.5, 0.1), t, MINUTE);
        assert_eq!(c.phase, Phase::Absorb, "the dwell starts again");
        hold(&mut c, &blind(51.5, 0.1), t, 2 * MINUTE);
        assert_eq!(c.phase, Phase::Done(Reason::Terminated));
    }

    #[test]
    fn a_pack_with_no_gauge_still_terminates() {
        // The whole point: no BMS, no SOC, so the current is the only signal
        // that the pack is full.
        let mut c = Controller::new(Config {
            v_absorb: 14.4,
            v_float: 13.6,
            v_recharge: 13.0,
            v_precharge: 11.0,
            i_max: 10.0,
            i_term: 0.5,
            ..cfg(Mode::Standard)
        });
        let t = Instant::now();
        c.step(&blind(12.6, 8.0), t);
        assert_eq!(c.phase, Phase::Bulk);
        c.step(&blind(14.4, 8.0), t + MINUTE);
        assert_eq!(c.phase, Phase::Absorb);
        let t = hold(&mut c, &blind(14.4, 0.3), t, 5 * MINUTE);
        assert_eq!(c.phase, Phase::Done(Reason::Terminated));
        assert!(!c.output_on);
        let _ = t;
    }

    #[test]
    fn a_gauge_stuck_below_its_target_does_not_hold_the_charge_open() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        let mut s = snap(&cells_at(3433, 15), 3.0);
        s.soc = 97;
        c.step(&s, t);
        assert_eq!(c.phase, Phase::Absorb);
        s.current_a = 0.1;
        hold(&mut c, &s, t, 5 * MINUTE);
        assert_eq!(c.phase, Phase::Done(Reason::Terminated));
    }

    #[test]
    fn a_real_gauge_reading_full_ends_it_sooner() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        let mut s = snap(&cells_at(3433, 15), 2.0);
        s.soc = 100;
        c.step(&s, t);
        assert_eq!(c.phase, Phase::Done(Reason::SocTarget));
    }

    #[test]
    fn an_estimated_gauge_is_not_believed() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        let mut s = snap(&cells_at(3433, 15), 2.0);
        s.soc = 100;
        s.soc_estimated = true;
        c.step(&s, t);
        assert_eq!(c.phase, Phase::Absorb, "a voltage guess cannot end a charge");
    }

    #[test]
    fn a_high_cell_pulls_the_whole_setpoint_down() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        let mut cells = cells_at(3380, 15);
        cells[3] = 3520;
        c.step(&snap(&cells, 2.0), t);
        assert_eq!(c.phase, Phase::Absorb);
        assert!(c.set_v < 51.5, "setpoint {} not backed off", c.set_v);
        assert!(c.output_on, "backing off is not the same as stopping");
        // Once the cell settles, the setpoint creeps back toward absorb.
        let v = c.set_v;
        c.step(&snap(&cells_at(3400, 15), 2.0), t + MINUTE);
        assert!(c.set_v > v);
        assert!(c.set_v <= 51.5);
    }

    #[test]
    fn a_cell_over_the_hard_limit_cuts_the_output() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        let mut cells = cells_at(3400, 15);
        cells[0] = 3560;
        c.step(&snap(&cells, 2.0), t);
        assert!(!c.output_on);
        assert!(c.set_v < 51.5);
        assert!(c.finished().is_none(), "one spike is not a fault");
    }

    #[test]
    fn a_cell_that_keeps_running_away_is_a_fault() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let mut t = Instant::now();
        let mut cells = cells_at(3400, 15);
        cells[0] = 3560;
        for _ in 0..HARD_CUTS_MAX + 1 {
            t += MINUTE;
            c.step(&snap(&cells, 0.0), t);
        }
        assert_eq!(c.finished(), Some(Reason::OverVoltage));
        assert!(!c.output_on);
    }

    #[test]
    fn a_flat_pack_is_pre_charged_first() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        c.step(&snap(&cells_at(2600, 15), 0.4), t);
        assert_eq!(c.phase, Phase::Precharge);
        assert!((c.set_a - c.cfg.i_precharge).abs() < 1e-9);
        assert!(c.output_on);
        // Once the cells recover it becomes an ordinary charge.
        c.step(&snap(&cells_at(3000, 15), 0.5), t + MINUTE);
        assert_eq!(c.phase, Phase::Bulk);
        assert!(c.set_a > c.cfg.i_precharge);
    }

    #[test]
    fn a_pack_that_never_comes_up_fails_pre_charge() {
        let mut c = Controller::new(Config {
            precharge_max: Duration::from_secs(600),
            ..cfg(Mode::Standard)
        });
        let t = Instant::now();
        let s = snap(&cells_at(2600, 15), 0.4);
        c.step(&s, t);
        hold(&mut c, &s, t, 11 * MINUTE);
        assert_eq!(c.finished(), Some(Reason::PrechargeFailed));
        assert!(!c.output_on);
    }

    #[test]
    fn a_full_charge_does_not_pre_charge() {
        let mut c = Controller::new(cfg(Mode::Standard));
        c.step(&snap(&cells_at(3300, 15), 1.0), Instant::now());
        assert_eq!(c.phase, Phase::Bulk);
    }

    #[test]
    fn lead_acid_floats_after_termination_and_recharges_on_sag() {
        let p = PackProfile {
            chemistry: Chemistry::LeadAcid,
            series: 6,
            parallel: 1,
            cell_ah: 100.0,
        };
        let mut c = Controller::new(Config {
            mode: Mode::Standard,
            i_term_for: Duration::from_secs(120),
            hold_max: Duration::from_secs(4 * 3600),
            ..Config::for_profile(&p)
        });
        assert!(c.cfg.maintain_float, "a lead-acid pack floats");
        let t = Instant::now();
        c.step(&blind(14.4, 10.0), t);
        assert_eq!(c.phase, Phase::Absorb);
        let t = hold(&mut c, &blind(14.4, 1.0), t, 5 * MINUTE);
        assert_eq!(c.phase, Phase::Float);
        assert!((c.set_v - c.cfg.v_float).abs() < 1e-9);
        assert!(c.output_on);
        // A sag below the recharge threshold starts the whole thing again.
        let t = hold(&mut c, &blind(12.8, 0.0), t, MINUTE);
        assert_eq!(c.phase, Phase::Bulk);
        assert!((c.set_v - c.cfg.v_absorb).abs() < 1e-9);
        let _ = t;
    }

    #[test]
    fn lithium_stops_instead_of_floating() {
        let c = Config::for_profile(&PackProfile::default());
        assert!(!c.maintain_float);
        let mut ctl = Controller::new(Config {
            mode: Mode::Standard,
            i_term_for: Duration::from_secs(120),
            ..c
        });
        let t = Instant::now();
        ctl.step(&snap(&cells_at(3500, 15), 5.0), t);
        assert_eq!(ctl.phase, Phase::Absorb);
        hold(&mut ctl, &snap(&cells_at(3500, 15), 0.5), t, 5 * MINUTE);
        assert_eq!(ctl.finished(), Some(Reason::Terminated));
        assert!(!ctl.output_on);
    }

    #[test]
    fn top_balance_holds_after_absorption_then_times_out() {
        let mut c = Controller::new(Config {
            hold_max: Duration::from_secs(3600),
            ..cfg(Mode::TopBalance)
        });
        let t = Instant::now();
        let mut cells = cells_at(3300, 15);
        cells[7] = 3500;
        let mut s = snap(&cells, 2.0);
        c.step(&s, t);
        assert_eq!(c.phase, Phase::Absorb);
        s.current_a = 0.1;
        let t = hold(&mut c, &s, t, 5 * MINUTE);
        assert_eq!(c.phase, Phase::Balance);
        assert!(c.output_on);
        assert!((c.set_a - c.cfg.i_min).abs() < 1e-9);
        hold(&mut c, &s, t, 70 * MINUTE);
        assert_eq!(c.finished(), Some(Reason::HoldTimeout));
        assert!(!c.output_on);
    }

    #[test]
    fn top_balance_completes_when_the_low_cell_comes_up() {
        let mut c = Controller::new(cfg(Mode::TopBalance));
        let t = Instant::now();
        let mut cells = cells_at(3300, 15);
        cells[7] = 3500;
        let mut s = snap(&cells, 0.1);
        c.step(&s, t);
        let t = hold(&mut c, &s, t, 5 * MINUTE);
        assert_eq!(c.phase, Phase::Balance);
        s = snap(&cells_at(3460, 15), 0.1);
        c.step(&s, t + MINUTE);
        assert_eq!(c.finished(), Some(Reason::Balanced));
    }

    #[test]
    fn bulk_only_stops_at_the_first_ceiling_touch() {
        let mut c = Controller::new(cfg(Mode::BulkOnly));
        let mut cells = cells_at(3300, 15);
        cells[2] = 3500;
        c.step(&snap(&cells, 3.0), Instant::now());
        assert_eq!(c.finished(), Some(Reason::CeilingReached));
        assert!(!c.output_on);
    }

    #[test]
    fn absorption_gives_up_on_its_own_clock() {
        let mut c = Controller::new(Config {
            absorb_max: Duration::from_secs(3600),
            ..cfg(Mode::Standard)
        });
        let t = Instant::now();
        c.step(&blind(51.5, 3.0), t);
        hold(&mut c, &blind(51.5, 1.0), t, 70 * MINUTE);
        assert_eq!(c.finished(), Some(Reason::AbsorbTimeout));
        assert!(!c.output_on);
    }

    #[test]
    fn a_storage_charge_stops_at_its_soc_in_any_mode() {
        let mut c = Controller::new(Config {
            stop_at_soc: Some(50),
            ..cfg(Mode::Standard)
        });
        let t = Instant::now();
        let mut s = snap(&cells_at(3300, 15), 3.0);
        s.soc = 40;
        c.step(&s, t);
        assert!(c.output_on);
        s.soc = 50;
        assert_eq!(c.step(&s, t + MINUTE), Phase::Done(Reason::SocTarget));
        assert!(!c.output_on);
    }

    #[test]
    fn a_pack_taking_nothing_at_all_is_a_connection_fault() {
        let mut c = Controller::new(Config {
            stall_timeout: Duration::from_secs(60),
            ..cfg(Mode::Standard)
        });
        let t = Instant::now();
        let s = snap(&cells_at(3300, 15), 0.0);
        c.step(&s, t);
        assert!(c.output_on);
        assert_eq!(
            c.step(&s, t + 2 * MINUTE),
            Phase::Done(Reason::NoCurrent)
        );
        assert!(!c.output_on);
    }

    #[test]
    fn current_flowing_keeps_the_watchdog_quiet() {
        let mut c = Controller::new(Config {
            stall_timeout: Duration::from_secs(60),
            ..cfg(Mode::Standard)
        });
        let t = Instant::now();
        let s = snap(&cells_at(3300, 15), 2.0);
        c.step(&s, t);
        assert_ne!(c.step(&s, t + 5 * MINUTE), Phase::Done(Reason::NoCurrent));
        assert!(c.output_on);
    }

    #[test]
    fn over_temperature_stops_everything() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let mut s = snap(&cells_at(3300, 15), 2.0);
        s.temp_c = 50.0;
        c.step(&s, Instant::now());
        assert_eq!(c.finished(), Some(Reason::OverTemp));
        assert!(!c.output_on);
    }

    #[test]
    fn two_read_failures_stop_the_charge() {
        let mut c = Controller::new(cfg(Mode::Standard));
        assert!(!c.on_read_failure());
        assert!(c.on_read_failure());
        assert_eq!(c.finished(), Some(Reason::LostTelemetry));
    }

    #[test]
    fn a_finished_charge_stays_finished() {
        let mut c = Controller::new(cfg(Mode::BulkOnly));
        let t = Instant::now();
        c.step(&snap(&cells_at(3500, 15), 1.0), t);
        assert_eq!(c.finished(), Some(Reason::CeilingReached));
        c.step(&snap(&cells_at(3300, 15), 1.0), t + MINUTE);
        assert_eq!(c.finished(), Some(Reason::CeilingReached));
        assert!(!c.output_on);
    }

    #[test]
    fn a_running_charge_takes_a_new_current() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        c.set_a = 3.0;
        c.step(&blind(51.5, 3.0), t);
        assert_eq!(c.phase, Phase::Absorb);
        let mut tune = Tuning::of(&c.cfg);
        tune.i_max = 1.0;
        c.retune(&tune);
        assert!((c.set_a - 1.0).abs() < 1e-9, "the command drops at once");
        c.step(&blind(51.5, 1.0), t + MINUTE);
        assert!((c.set_a - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_termination_current_cannot_be_raised_into_the_charge_current() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let mut tune = Tuning::of(&c.cfg);
        tune.i_max = 2.0;
        tune.i_term = 5.0;
        c.retune(&tune);
        assert!(c.cfg.i_term <= c.cfg.i_max / 2.0);
        assert!(c.cfg.i_term > 0.0);
    }

    #[test]
    fn retuning_the_stop_current_is_judged_from_the_next_sample() {
        let mut c = Controller::new(cfg(Mode::Standard));
        let t = Instant::now();
        c.step(&blind(51.5, 3.0), t);
        // A pack taking 1 A is not finished at a 0.2 A termination.
        let t = hold(&mut c, &blind(51.5, 1.0), t, 5 * MINUTE);
        assert_eq!(c.phase, Phase::Absorb);
        // Told to stop at 1.5 A instead, the tail it is already sitting in
        // counts, and the dwell still has to pass.
        let mut tune = Tuning::of(&c.cfg);
        tune.i_term = 1.5;
        c.retune(&tune);
        let t = hold(&mut c, &blind(51.5, 1.0), t, 3 * MINUTE);
        assert_eq!(c.phase, Phase::Done(Reason::Terminated));
        let _ = t;
    }

    #[test]
    fn a_profile_sets_every_limit() {
        let c = Config::for_profile(&PackProfile::default());
        assert!((c.v_absorb - 52.5).abs() < 1e-9);
        assert!((c.v_float - 51.0).abs() < 1e-9);
        assert!(c.v_recharge < c.v_float);
        assert!((c.i_term - 2.5).abs() < 1e-9, "C/20 of 50 Ah");
        assert!((c.i_max - 10.0).abs() < 1e-9, "0.2C of 50 Ah");
        let lead = Config::for_profile(&PackProfile {
            chemistry: Chemistry::LeadAcid,
            series: 6,
            parallel: 1,
            cell_ah: 100.0,
        });
        assert!((lead.i_max - 10.0).abs() < 1e-9, "C/10 of 100 Ah");
        assert!((lead.i_term - 2.0).abs() < 1e-9, "2% of 100 Ah");
        assert_eq!(c.cell_ceiling_mv, 3500);
        assert_eq!(c.cell_hard_mv, 3550);
    }
}
