mod chart;
mod devices;
mod theme;
mod worker;

use cycler_core::charge::{Config, Mode, Phase};
use cycler_core::cycle::Plan;
use cycler_core::chemistry::{Chemistry, PackProfile};
use cycler_core::device::{LoadMode, Regulation};
use cycler_core::discharge;
use cycler_core::device::{CHARGER_BACKENDS, LOAD_BACKENDS};
use cycler_core::pack::PACK_BACKENDS;
use devices::{Choice, Remembered};
use egui::{Color32, RichText};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use worker::{Command, Session, Update};

const HISTORY: usize = 7200;
/// Shortest strip worth drawing a trace in.
const CHART_MIN_H: f32 = 150.0;
/// Shortest comb worth drawing cells in.
const COMB_MIN_H: f32 = 132.0;

struct Sample {
    minutes: f64,
    pack_v: f64,
    current_a: f64,
    cells_mv: Vec<u16>,
}

/// What the history strip draws. Pack voltage and current answer "is it
/// working"; the cells answer "which one is the problem", and that is what
/// the table beside it is for, so the cells are the view you ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trace {
    Pack,
    Cells,
}

struct App {
    session: Option<Session>,
    log: Option<PathBuf>,
    remembered: Remembered,
    /// The specs the running session was opened with, so a changed dropdown
    /// can be told apart from a connected device.
    active: Vec<String>,
    profile: PackProfile,
    series_detected: bool,
    /// Test rate as a fraction of capacity: 0.2C fills or empties a pack in
    /// about five hours.
    charge_c: f64,
    discharge_c: f64,
    pack_floor_v: f64,
    pack_pick: Choice,
    charger_pick: Choice,
    load_pick: Choice,
    history: VecDeque<Sample>,
    trace: Trace,
    started: Instant,
    last: Option<Update>,
    error: Option<String>,
    mode: Mode,
    cv: f64,
    max_current: f64,
    ceiling_mv: u16,
    target_mv: u16,
    hold_hours: f64,
    absorb_hours: f64,
    float_v: f64,
    i_term: Option<f64>,
    stop_on_alarm: bool,
    /// Alarms to keep running through, as a comma separated list of
    /// fragments. A pack that flags balancing all the way up would otherwise
    /// never finish a charge.
    alarm_ignore_list: String,
    charge_to_soc: bool,
    discharge_to_soc: bool,
    target_soc: f64,
    discharge_mode: LoadMode,
    discharge_a: f64,
    floor_mv: u16,
    rest_min: f64,
    repeat: usize,
    /// Cell levels as drawn, easing towards the last reading, so a comb of
    /// fifteen bars settles instead of jumping every poll.
    shown_mv: Vec<f32>,
    shown_at: Instant,
    /// The rail as it stood on the last live update, and the rail as it was
    /// left when the run ended.
    running_stage: Option<StageView>,
    ended: Option<StageView>,
}

impl App {
    fn new(log: Option<PathBuf>) -> Self {
        Self::build(log, true)
    }

    fn build(log: Option<PathBuf>, live: bool) -> Self {
        let mut app = Self {
            session: None,
            log,
            remembered: Remembered::default(),
            active: Vec::new(),
            profile: PackProfile::default(),
            series_detected: false,
            charge_c: PackProfile::default().chemistry.default_charge_c(),
            discharge_c: PackProfile::default().chemistry.default_discharge_c(),
            pack_floor_v: PackProfile::default().floor_v(),
            pack_pick: Choice::new("battery", PACK_BACKENDS),
            charger_pick: Choice::new("charger", CHARGER_BACKENDS),
            load_pick: Choice::new("load", LOAD_BACKENDS),
            history: VecDeque::with_capacity(HISTORY),
            trace: Trace::Pack,
            started: Instant::now(),
            last: None,
            error: None,
            mode: Mode::Standard,
            cv: 51.5,
            max_current: 3.0,
            ceiling_mv: 3500,
            target_mv: 3450,
            hold_hours: 48.0,
            absorb_hours: 6.0,
            float_v: 51.0,
            i_term: None,
            stop_on_alarm: true,
            alarm_ignore_list: "balanc".into(),
            charge_to_soc: false,
            discharge_to_soc: false,
            target_soc: 50.0,
            discharge_mode: LoadMode::Cc,
            discharge_a: 3.0,
            floor_mv: 3000,
            rest_min: 30.0,
            repeat: 1,
            shown_mv: Vec::new(),
            shown_at: Instant::now(),
            running_stage: None,
            ended: None,
        };
        app.remembered = Remembered::load();
        if let Some(r) = app.remembered.c_rate.filter(|r| *r > 0.0) {
            app.charge_c = r;
        }
        if let Some(r) = app.remembered.discharge_c_rate.filter(|r| *r > 0.0) {
            app.discharge_c = r;
        }
        if let Some(p) = app.remembered.profile {
            app.profile = p;
            app.apply_profile();
        }
        app.remembered.apply([
            &mut app.pack_pick,
            &mut app.charger_pick,
            &mut app.load_pick,
        ]);
        if live {
            app.connect();
        }
        app
    }

    /// A panel with nothing behind it, for the screenshots in `docs/` and for
    /// working on the layout away from the bench.
    fn demo() -> Self {
        let mut app = Self::build(None, false);
        app.profile = PackProfile {
            chemistry: Chemistry::LiFePo4,
            series: 15,
            parallel: 1,
            cell_ah: 50.0,
            ceiling_mv: None,
        };
        app.apply_profile();
        app.pack_pick.backend = 0;
        app.pack_pick.enabled = true;
        app.max_current = 3.0;
        app.discharge_a = 5.0;
        app.discharge_c = 0.1;
        app.i_term = Some(2.5);
        app.history.clear();
        app.started = Instant::now() - Session::DEMO_HISTORY;
        app.session = Some(Session::demo());
        app.active = app.selected_specs();
        app
    }

    fn connect(&mut self) {
        self.remembered.profile = Some(self.profile);
        self.remembered.c_rate = Some(self.charge_c);
        self.remembered.discharge_c_rate = Some(self.discharge_c);
        self.remembered.remember([&self.pack_pick, &self.charger_pick, &self.load_pick]);
        // Wait for the old worker to release the ports before opening them
        // again, or the two sessions fight over the same serial device.
        if let Some(s) = self.session.take() {
            s.close();
        }
        let Some(pack) = self.pack_pick.spec() else {
            self.error = Some("no battery selected".into());
            return;
        };
        let charger = self.charger_pick.spec().unwrap_or_else(|| "none:".into());
        self.history.clear();
        self.started = Instant::now();
        self.last = None;
        let session = Session::spawn(
            pack,
            charger,
            self.load_pick.spec(),
            self.log.clone(),
            Duration::from_secs(2),
        );
        if let Ok(mut g) = SHUTDOWN.lock() {
            *g = Some(session.tx.clone());
        }
        let _ = session.tx.send(Command::SetProfile(self.profile));
        self.active = self.selected_specs();
        self.session = Some(session);
    }

    /// What the dropdowns currently say, which is not the same as what the
    /// worker has open until Connect is pressed.
    fn selected_specs(&self) -> Vec<String> {
        [&self.pack_pick, &self.charger_pick, &self.load_pick]
            .iter()
            .map(|p| p.spec().unwrap_or_default())
            .collect()
    }

    fn pending_connect(&self) -> bool {
        !self.active.is_empty() && self.selected_specs() != self.active
    }

    fn config(&self) -> Config {
        Config {
            mode: self.mode,
            v_absorb: self.cv,
            cell_ceiling_mv: self.ceiling_mv,
            cell_hard_mv: self.ceiling_mv + 50,
            cell_target_mv: self.target_mv,
            v_float: self.float_v,
            v_recharge: self.float_v - 0.1 * self.profile.series.max(1) as f64,
            i_term: self.stop_current(),
            absorb_max: Duration::from_secs_f64(self.absorb_hours * 3600.0),
            stop_on_alarm: self.stop_on_alarm,
            alarms_ignored: self.alarms_ignored(),
            temp_min_c: self.profile.chemistry.charge_min_c(),
            stop_at_soc: self.charge_to_soc.then_some(self.target_soc as u8),
            i_max: self.max_current,
            i_start: (self.max_current * 0.3).min(self.max_current),
            hold_max: Duration::from_secs_f64(self.hold_hours * 3600.0),
            // Everything not on the card comes from what the pack is, not
            // from a default built for a 15S lithium bench pack.
            ..Config::for_profile(&self.profile)
        }
    }

    fn alarms_ignored(&self) -> Vec<String> {
        self.alarm_ignore_list
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// What the rig is doing, if anything: a plan owns the battery, the
    /// supply and the load until it ends, so nothing else may start one.
    fn busy(&self) -> Option<String> {
        let u = self.last.as_ref()?;
        if !u.running {
            return None;
        }
        Some(if u.plan_steps > 1 {
            format!("a cycle test is running ({})", u.step_label)
        } else if u.step_label.starts_with("charge") {
            "a charge is running".into()
        } else {
            "a discharge is running".into()
        })
    }

    /// Whether a charge is running right now, which is when the settings
    /// that define it stop being settings.
    fn charging(&self) -> bool {
        self.last
            .as_ref()
            .is_some_and(|u| u.running && u.step_label.starts_with("charge"))
    }

    /// The part of the charge config a running charge will still take.
    /// The voltage to convert a setpoint through: what the pack is at now,
    /// or what the profile says it should sit at when nothing is connected.
    fn working_v(&self) -> f64 {
        self.last
            .as_ref()
            .and_then(|u| u.snapshot.as_ref())
            .map(|s| s.pack_v)
            .filter(|v| *v > 1.0)
            .unwrap_or_else(|| {
                self.profile.series.max(1) as f64 * self.profile.cell().nominal_mv as f64 / 1000.0
            })
    }

    /// Carry a discharge setpoint from one mode into another, so switching
    /// from 2 A to power gives the watts that pack draws at 2 A rather than
    /// 2 W. Current is the common currency: every mode converts through it.
    fn convert_setpoint(&self, from: LoadMode, to: LoadMode, value: f64) -> f64 {
        let v = self.working_v();
        if to == LoadMode::Cv {
            return v;
        }
        let amps = match from {
            LoadMode::Cc => value,
            LoadMode::Cp => value / v,
            LoadMode::Cr if value > 0.0 => v / value,
            // Constant voltage says nothing about current, so fall back to
            // the C-rate the pack was set up with.
            _ => self.capacity_ah() * self.discharge_c,
        }
        .max(0.01);
        match to {
            LoadMode::Cc => amps,
            LoadMode::Cp => v * amps,
            LoadMode::Cr => v / amps,
            LoadMode::Cv => v,
        }
    }

    fn discharging(&self) -> bool {
        self.last
            .as_ref()
            .is_some_and(|u| u.running && u.step_label.starts_with("discharge"))
    }

    fn load_tuning(&self) -> discharge::Tuning {
        discharge::Tuning::of(&self.discharge_config())
    }

    fn tuning(&self) -> cycler_core::charge::Tuning {
        cycler_core::charge::Tuning {
            i_max: self.max_current,
            i_term: self.stop_current(),
            absorb_max: Duration::from_secs_f64(self.absorb_hours * 3600.0),
            hold_max: Duration::from_secs_f64(self.hold_hours * 3600.0),
            stop_at_soc: self.charge_to_soc.then_some(self.target_soc as u8),
        }
    }

    /// Why a card has nothing to offer: not connected yet, or connected
    /// without the device that card drives.
    fn missing(&self, what: &str) -> String {
        if self.last.is_none() {
            "Not connected: pick the devices and press Connect.".into()
        } else {
            format!("No {what} is open. Pick one and press Connect.")
        }
    }

    /// Whether a supply is open. Everything on the charge card depends on
    /// it, and so does half of a cycle plan.
    /// Whether anything is reporting cells. Without them there is no comb to
    /// draw and no per-cell trace to plot, and an empty well for each is a
    /// worse answer than not showing them.
    fn has_cells(&self) -> bool {
        self.last
            .as_ref()
            .and_then(|u| u.snapshot.as_ref())
            .is_some_and(|s| s.has_cells())
    }

    fn has_charger(&self) -> bool {
        self.last.as_ref().is_some_and(|u| u.has_charger)
    }

    fn has_load(&self) -> bool {
        self.last.as_ref().is_some_and(|u| u.has_load)
    }

    /// A load that cannot be switched or set from here: the run tells you
    /// when to connect it and counts amp-hours from the pack instead.
    fn load_manual(&self) -> bool {
        self.last.as_ref().is_some_and(|u| u.load_manual)
    }

    /// Whether this pack goes on to a float after terminating. Lead-acid
    /// does; a lithium pack is left alone.
    fn floats(&self) -> bool {
        self.mode == Mode::Standard && self.profile.chemistry == Chemistry::LeadAcid
    }

    /// Where absorption ends. C/20 unless it has been typed over.
    fn stop_current(&self) -> f64 {
        self.i_term.unwrap_or_else(|| {
            (self.capacity_ah() * self.profile.chemistry.default_termination_c()).max(0.1)
        })
    }

    /// The capacity to size test currents from: the BMS's rating when it has
    /// one, otherwise the profile's own arithmetic.
    fn capacity_ah(&self) -> f64 {
        self.last
            .as_ref()
            .and_then(|u| u.snapshot.as_ref())
            .and_then(|s| s.rated_ah)
            .filter(|ah| *ah > 0.0)
            .unwrap_or_else(|| self.profile.capacity_ah())
    }

    /// Whether the connected pack reports cells at all.
    fn blind(&self) -> bool {
        self.last
            .as_ref()
            .and_then(|u| u.snapshot.as_ref())
            .map(|s| !s.has_cells())
            .unwrap_or(false)
    }

    fn discharge_config(&self) -> discharge::Config {
        discharge::Config {
            mode: self.discharge_mode,
            setpoint: self.discharge_a,
            stop_at_soc: self.discharge_to_soc.then_some(self.target_soc as u8),
            // The load's own cutoff is programmed from this, so on a pack
            // that reports cells it has to be the cell floor across the
            // string rather than a limit left over from the profile.
            pack_floor_v: if self.blind() {
                self.pack_floor_v
            } else {
                self.floor_mv as f64 * self.profile.series.max(1) as f64 / 1000.0
            },
            cell_floor_mv: self.floor_mv,
            stop_on_alarm: self.stop_on_alarm,
            alarms_ignored: self.alarms_ignored(),
            temp_min_c: self.profile.chemistry.discharge_min_c(),
            ..Default::default()
        }
    }

    fn plan(&self, full_cycle: bool) -> Plan {
        if full_cycle {
            Plan::capacity(
                self.config(),
                self.discharge_config(),
                Duration::from_secs_f64(self.rest_min * 60.0),
                self.repeat.max(1),
            )
        } else {
            Plan::charge(self.config())
        }
    }

    /// Push the profile into the limits. One place to say what the battery is,
    /// rather than typing the same numbers into five fields.
    fn apply_profile(&mut self) {
        let cell = self.profile.cell();
        self.cv = self.profile.charge_v();
        self.profile.set_charge_v(self.cv);
        self.float_v = self.profile.float_v();
        self.ceiling_mv = cell.ceiling_mv;
        self.target_mv = cell.balance_mv;
        self.floor_mv = cell.floor_mv;
        self.pack_floor_v = self.profile.floor_v();
        let capacity = self.capacity_ah();
        self.max_current = (capacity * self.charge_c).max(0.1);
        self.discharge_a = (capacity * self.discharge_c).max(0.1);
    }

    fn drain(&mut self) {
        let Some(session) = &self.session else { return };
        while let Ok(u) = session.rx.try_recv() {
            if let Some(s) = &u.snapshot {
                let t = u.at.duration_since(self.started).as_secs_f64() / 60.0;
                if self.history.len() == HISTORY {
                    self.history.pop_front();
                }
                self.history.push_back(Sample {
                    minutes: t,
                    pack_v: s.pack_v,
                    current_a: s.current_a,
                    cells_mv: s.cells_mv.clone(),
                });
            }
            if let Some(s) = u.snapshot.as_ref() {
                if s.has_cells() {
                    self.profile.observe_cells(s.cells_mv.len());
                    if !self.series_detected
                        && let Some(c) = Chemistry::from_cell_mv(s.high_mv())
                    {
                        self.profile.chemistry = c;
                    }
                    self.series_detected = true;
                } else if !self.series_detected && s.pack_v > 1.0 {
                    // No BMS: a resting voltage is the only clue to how many
                    // cells are in there, and it is a guess until told
                    // otherwise.
                    self.profile.series = self.profile.chemistry.series_from_voltage(s.pack_v);
                    self.series_detected = true;
                }
            }
            // A run that has ended still has something to say, and the
            // controller stops reporting the moment it lets go. Latch the
            // last live rail and light its final segment, so a charge that
            // finished while you were away still shows what ended it.
            if u.running {
                self.ended = None;
            } else if self.ended.is_none()
                && let Some(mut v) = self.running_stage.take()
            {
                v.plan_at = (!v.plan.is_empty()).then_some(v.plan.len() - 1);
                v.at = (!v.stages.is_empty()).then_some(v.stages.len() - 1);
                if !u.note.is_empty() {
                    v.exit = u.note.clone();
                }
                v.done = true;
                self.ended = Some(v);
            }
            self.error = u.error.clone();
            self.last = Some(u);
            if self.last.as_ref().is_some_and(|u| u.running) {
                self.running_stage = Some(self.stage_view());
            }
        }
    }

    fn send(&self, cmd: Command) {
        if let Some(s) = &self.session {
            let _ = s.tx.send(cmd);
        }
    }
}

fn cell_color(i: usize, n: usize) -> Color32 {
    let (r, g, b) = hsl(i as f32 / n.max(1) as f32 * 360.0, 0.62, 0.60);
    Color32::from_rgb(r, g, b)
}

fn hsl(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

impl eframe::App for App {
    /// Plot bounds are computed from the pack every frame; a remembered zoom
    /// from a previous run silently overrides them.
    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        self.drain();
        let scanning = self.pack_pick.scanning()
            || self.charger_pick.scanning()
            || self.load_pick.scanning();
        ctx.request_repaint_after(if scanning {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(500)
        });

        egui::Panel::top("head")
            .frame(egui::Frame::NONE.fill(theme::WELL).inner_margin(8))
            .show_inside(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("CYCLER")
                            .font(theme::legend_font(15.0))
                            .extra_letter_spacing(3.0)
                            .color(theme::VALUE),
                    );
                    if let Some(p) = &self.log {
                        ui.label(theme::legend(format!("logging {}", p.display())));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(e) = &self.error {
                            ui.label(RichText::new(e).size(11.5).color(theme::FAULT));
                        } else if let Some(s) =
                            self.last.as_ref().and_then(|u| u.snapshot.as_ref())
                            && s.has_cells()
                        {
                            let (lo, hi) = (s.low_mv(), s.high_mv());
                            let tint = if hi >= self.ceiling_mv {
                                theme::FAULT
                            } else {
                                theme::TRACE
                            };
                            ui.label(RichText::new(format!("{hi} mV")).font(theme::figure(13.0)).color(tint));
                            ui.label(theme::legend("highest cell"));
                            ui.add_space(10.0);
                            ui.label(
                                RichText::new(format!("{lo} mV"))
                                    .font(theme::figure(13.0))
                                    .color(theme::TRACE),
                            );
                            ui.label(theme::legend("lowest cell"));
                        }
                    });
                });
            });

        egui::Panel::right("controls")
            .exact_size(300.0)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(theme::CHASSIS).inner_margin(8))
            .show_inside(root, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                self.devices_card(ui);
                self.profile_card(ui);
                self.controls(ui);
                self.discharge_card(ui);
                self.plan_card(ui);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::CHASSIS).inner_margin(8))
            .show_inside(root, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                self.stage_card(ui);
                self.battery_card(ui);
                ui.horizontal_top(|ui| {
                    let w = ((ui.available_width() - 8.0) / 2.0).max(0.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(w, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| self.charger_card(ui),
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width().max(0.0), 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| self.load_card(ui),
                    );
                });
                // Whatever is left over is split between the two views that
                // reward the space: the comb of cells now, and the traces
                // that got them there.
                // Split what is left rather than letting each card claim a
                // minimum: two minimums in a short window overflow the panel
                // and the chart falls off the bottom of the screen.
                // From the cursor to the bottom of the window. The root Ui
                // hands out a stale height once the window has been resized,
                // so the screen is the only thing worth measuring against.
                let spare = (ui.ctx().content_rect().bottom() - 8.0 - ui.cursor().top()).max(80.0);
                let chart_h = if self.has_cells() {
                    let usable = (spare - 8.0 - 2.0 * CARD_CHROME).max(80.0);
                    let comb_h = (usable * 0.44)
                        .clamp(60.0, COMB_MIN_H.max(300.0_f32.min(usable - CHART_MIN_H)));
                    self.cells_card(ui, comb_h);
                    (usable - comb_h).max(90.0)
                } else {
                    (spare - CARD_CHROME).max(CHART_MIN_H)
                };
                self.history_card(ui, chart_h);
            });
    }
}

/// Height a card spends on its own header, rules and margins, so a card told
/// to fill the panel can work out what is left for its contents.
const CARD_CHROME: f32 = 46.0;

/// What the rails are showing: the plan's steps, and the stages of whichever
/// step is running when that step has any of its own.
#[derive(Default, Clone)]
struct StageView {
    plan: Vec<String>,
    plan_at: Option<usize>,
    stages: Vec<String>,
    at: Option<usize>,
    exit: String,
    faulted: bool,
    /// The run is over. The rail stays up, with its last segment lit, until
    /// the next one starts.
    done: bool,
}

impl App {
    fn battery_card(&mut self, ui: &mut egui::Ui) {
        let snapshot = self.last.as_ref().and_then(|u| u.snapshot.clone());
        let last_for_source = (
            self.last
                .as_ref()
                .and_then(|u| u.load)
                .map(|l| l.volts > 0.5),
            self.last
                .as_ref()
                .and_then(|u| u.charger)
                .map(|(v, _)| v > 0.5),
        );
        let name = self
            .last
            .as_ref()
            .map(|u| u.pack_name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "battery".into());
        let rail = snapshot.as_ref().map(|s| {
            if !s.alarms.is_empty() || s.high_mv() >= self.ceiling_mv {
                theme::FAULT
            } else {
                theme::TRACE
            }
        });
        let source = match last_for_source {
            (Some(l), _) if l => "load",
            (_, Some(c)) if c => "charger",
            _ => "instrument",
        };
        let chem = self.profile.chemistry.label();
        let measured = self
            .last
            .as_ref()
            .and_then(|u| u.measured_ah)
            .unwrap_or(0.0);
        theme::card(
            ui,
            rail,
            |ui| {
                ui.label(RichText::new(name).size(12.5).strong().color(theme::VALUE));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let alarms = snapshot.as_ref().map(|s| s.alarms.len()).unwrap_or(0);
                    if alarms > 0 {
                        theme::lamp(ui, &format!("{alarms} alarm"), true, true);
                    }
                    let state = snapshot
                        .as_ref()
                        .map(|s| s.state_label())
                        .unwrap_or("offline");
                    theme::lamp(ui, state, state == "charging" || state == "discharging", false);
                });
            },
            |ui| match &snapshot {
                Some(s) if !s.has_cells() => {
                    // Nothing here comes from the battery: it is whatever the
                    // charger or load can see at the terminals.
                    ui.horizontal(|ui| {
                        theme::hero(ui, "pack", &format!("{:.3}", s.pack_v), "V", theme::TRACE);
                    });
                    theme::readouts(
                        ui,
                        &[
                            ("current", format!("{:+.3} A", s.current_a), theme::TRACE),
                            (
                                "power",
                                format!("{:+.2} W", s.pack_v * s.current_a),
                                theme::TRACE,
                            ),
                            ("soc est", format!("{}%", s.soc), theme::READOUT),
                        ],
                    );
                    theme::note(
                        ui,
                        format!(
                            "No BMS: read by the {}. SOC is estimated from {} voltage and \
                             is only honest at rest; limits are pack voltage only.",
                            source,
                            chem
                        ),
                        theme::LEGEND,
                    );
                }
                Some(s) => {
                    ui.horizontal(|ui| {
                        theme::hero(ui, "pack", &format!("{:.3}", s.pack_v), "V", theme::TRACE);
                    });
                    theme::readouts(
                        ui,
                        &[
                            ("current", format!("{:+.3} A", s.current_a), theme::TRACE),
                            (
                                "power",
                                format!("{:+.2} W", s.pack_v * s.current_a),
                                theme::TRACE,
                            ),
                            ("soc", format!("{}%", s.soc), theme::VALUE),
                            (
                                "soh",
                                s.soh.map(|v| format!("{v:.0}%")).unwrap_or("-".into()),
                                theme::VALUE,
                            ),
                            (
                                "cycles",
                                s.cycles.map(|v| format!("{v:.0}")).unwrap_or("-".into()),
                                theme::VALUE,
                            ),
                            ("temp", format!("{:.1} C", s.temp_c), theme::VALUE),
                            (
                                "rated",
                                s.rated_ah
                                    .map(|v| format!("{v:.0} Ah"))
                                    .unwrap_or("-".into()),
                                theme::LEGEND,
                            ),
                            (
                                "measured",
                                if measured > 0.0 {
                                    format!("{measured:.1} Ah")
                                } else {
                                    "-".into()
                                },
                                if measured > 0.0 {
                                    theme::READOUT
                                } else {
                                    theme::LEGEND
                                },
                            ),
                            (
                                "of rated",
                                match (s.rated_ah, measured > 0.0) {
                                    (Some(r), true) if r > 0.0 => {
                                        format!("{:.0}%", 100.0 * measured / r)
                                    }
                                    _ => "-".into(),
                                },
                                theme::VALUE,
                            ),
                        ],
                    );
                    if !s.alarms.is_empty() {
                        ui.add_space(2.0);
                        let ignored = self.alarms_ignored();
                        for a in &s.alarms {
                            let skipped = ignored.iter().any(|i| {
                                a.to_lowercase().contains(&i.trim().to_lowercase())
                                    && !i.trim().is_empty()
                            });
                            theme::note(
                                ui,
                                if skipped {
                                    format!("{a} (ignored)")
                                } else {
                                    format!("{a} - stops the run")
                                },
                                if skipped { theme::LEGEND } else { theme::FAULT },
                            );
                        }
                    }
                }
                None => {
                    ui.label(theme::legend("waiting for the first cell read"));
                }
            },
        );
    }

    fn charger_card(&mut self, ui: &mut egui::Ui) {
        let last = self.last.clone();
        let wanted = last.as_ref().map(|u| u.demand.charger_on).unwrap_or(false);
        let reported = last.as_ref().and_then(|u| u.charger_output);
        let on = reported.unwrap_or(wanted);
        let disagrees = reported.is_some_and(|r| r != wanted);
        let pack_v = last
            .as_ref()
            .and_then(|u| u.snapshot.as_ref())
            .map(|s| s.pack_v)
            .unwrap_or(0.0);
        let seen = last.as_ref().and_then(|u| u.charger).map(|(v, _)| v);
        let mismatch = on && seen.is_some_and(|v| cycler_core::agree::disagrees(pack_v, v));
        let unseen = on && pack_v > 0.5 && seen.is_some_and(|v| v <= 0.5);
        theme::card(
            ui,
            Some(if mismatch || unseen {
                theme::FAULT
            } else if on {
                theme::OK
            } else {
                theme::ETCH
            }),
            |ui| {
                ui.label(theme::legend("charger"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(r) = last.as_ref().and_then(|u| u.charger_regulation)
                        && on
                    {
                        ui.label(theme::legend(r.label()));
                    }
                    if mismatch {
                        theme::lamp(ui, "wrong battery?", true, true);
                    } else if unseen {
                        theme::lamp(ui, "no battery seen", true, true);
                    }
                    theme::lamp(
                        ui,
                        if on { "output on" } else { "output off" },
                        on,
                        disagrees || mismatch || unseen,
                    );
                });
            },
            |ui| {
                let (v, a) = last.as_ref().and_then(|u| u.charger).unwrap_or((0.0, 0.0));
                if mismatch {
                    theme::note(
                        ui,
                        format!(
                            "supply sees {v:.2} V, battery reads {pack_v:.2} V: \
                             not the same pack"
                        ),
                        theme::FAULT,
                    );
                } else if unseen {
                    theme::note(
                        ui,
                        "output on and no voltage at the terminals: check the leads and fuse.",
                        theme::FAULT,
                    );
                }
                if disagrees {
                    theme::note(
                        ui,
                        format!(
                            "supply says {}, controller wants {}",
                            if on { "on" } else { "off" },
                            if wanted { "on" } else { "off" }
                        ),
                        theme::FAULT,
                    );
                }
                theme::readouts(
                    ui,
                    &[
                        ("out", format!("{v:.3} V"), theme::TRACE),
                        ("current", format!("{a:.3} A"), theme::TRACE),
                        ("power", format!("{:.2} W", v * a), theme::TRACE),
                    ],
                );
                // Which setpoint is doing the work depends on the mode. In CC
                // the current is the target and the voltage is the ceiling it
                // is heading for; in CV that swaps, and calling the current a
                // setpoint reads as a fault when the pack takes half of it.
                let set_v = last.as_ref().map(|u| u.demand.charger_v).unwrap_or(0.0);
                let set_a = last.as_ref().map(|u| u.demand.charger_a).unwrap_or(0.0);
                let holding_v = last
                    .as_ref()
                    .and_then(|u| u.charger_regulation)
                    .map(|r| r == Regulation::Cv)
                    .unwrap_or(false);
                let (v_label, a_label) = if !on {
                    ("set V", "set A")
                } else if holding_v {
                    ("holding", "limit")
                } else {
                    ("ceiling", "holding")
                };
                theme::readouts(
                    ui,
                    &[
                        (v_label, format!("{set_v:.2} V"), theme::READOUT),
                        (a_label, format!("{set_a:.3} A"), theme::READOUT),
                    ],
                );
                ui.label(
                    RichText::new(
                        last.as_ref()
                            .map(|u| u.charger_name.clone())
                            .unwrap_or_default(),
                    )
                    .size(10.5)
                    .color(theme::LEGEND),
                );
                // The note belongs to whichever step is running; a discharge
                // note on the charger card reads like a charger fault.
                if let Some(u) = last.as_ref()
                    && !u.note.is_empty()
                    && u.step_label.starts_with("charge")
                {
                    theme::note(ui, u.note.clone(), theme::READOUT);
                }
            },
        );
    }

    fn load_card(&mut self, ui: &mut egui::Ui) {
        let last = self.last.clone();
        let load = last.as_ref().and_then(|u| u.load);
        let on = load.map(|l| l.on).unwrap_or(false);
        let pack_v_hdr = last
            .as_ref()
            .and_then(|u| u.snapshot.as_ref())
            .map(|s| s.pack_v)
            .unwrap_or(0.0);
        let mismatch = load
            .map(|l| cycler_core::agree::disagrees(pack_v_hdr, l.volts))
            .unwrap_or(false);
        let unseen = on && pack_v_hdr > 0.5 && load.map(|l| l.volts <= 0.5).unwrap_or(false);
        theme::card(
            ui,
            Some(if mismatch {
                theme::FAULT
            } else if on {
                theme::OK
            } else {
                theme::ETCH
            }),
            |ui| {
                ui.label(theme::legend("load"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if mismatch {
                        theme::lamp(ui, "wrong battery?", true, true);
                    } else if unseen {
                        theme::lamp(ui, "no battery seen", true, true);
                    }
                    theme::lamp(
                        ui,
                        if on { "load on" } else { "load off" },
                        on,
                        mismatch || unseen,
                    );
                });
            },
            |ui| match load {
                // A dumb sink reports nothing about itself; the pack's own
                // current is the measurement, so show that instead.
                Some(_) if last.as_ref().is_some_and(|u| u.load_manual) => {
                    // A dumb sink reports nothing about itself, so the pack's
                    // own current is the measurement. Only current leaving the
                    // pack is this load's draw; while it is charging, the
                    // sink is doing nothing.
                    let draw_a = last
                        .as_ref()
                        .and_then(|u| u.snapshot.as_ref())
                        .map(|s| (-s.current_a).max(0.0))
                        .unwrap_or(0.0);
                    let volts = last
                        .as_ref()
                        .and_then(|u| u.snapshot.as_ref())
                        .map(|s| s.pack_v)
                        .unwrap_or(0.0);
                    theme::readouts(
                        ui,
                        &[
                            ("draw", format!("{draw_a:.3} A"), theme::TRACE),
                            ("power", format!("{:.2} W", draw_a * volts), theme::TRACE),
                        ],
                    );
                    theme::note(
                        ui,
                        "Switched by hand: cycler measures and warns, it cannot cut off.",
                        theme::READOUT,
                    );
                    ui.label(
                        RichText::new(last.as_ref().map(|u| u.load_name.clone()).unwrap_or_default())
                            .size(10.5)
                            .color(theme::LEGEND),
                    );
                }
                Some(l) => {
                    let pack_v = last
                        .as_ref()
                        .and_then(|u| u.snapshot.as_ref())
                        .map(|s| s.pack_v)
                        .unwrap_or(0.0);
                    let wrong_battery = l.volts > 0.5
                        && pack_v > 0.5
                        && (pack_v - l.volts).abs() > (pack_v * 0.1).max(2.0);
                    let demand = last.as_ref().map(|u| u.demand);
                    let mode = demand.map(|d| d.load_mode).unwrap_or(LoadMode::Cc);
                    let ohms = match l.ohms {
                        Some(r) if r < 9999.0 => format!("{r:.2} R"),
                        Some(_) => "open".into(),
                        None if l.amps.abs() > 0.001 => format!("{:.2} R", l.volts / l.amps),
                        None => "open".into(),
                    };
                    theme::readouts(
                        ui,
                        &[
                            (
                                "in",
                                format!("{:.3} V", l.volts),
                                if wrong_battery { theme::FAULT } else { theme::TRACE },
                            ),
                            ("draw", format!("{:.3} A", l.amps), theme::TRACE),
                            ("power", format!("{:.2} W", l.watts), theme::TRACE),
                            ("resistance", ohms, theme::TRACE),
                            ("drawn", format!("{:.4} Ah", l.amp_hours), theme::READOUT),
                            ("energy", format!("{:.2} Wh", l.watt_hours), theme::VALUE),
                            ("temp", format!("{:.0} C", l.temp_c), theme::VALUE),
                            ("run", hms(l.runtime_s), theme::VALUE),
                        ],
                    );
                    // What the load was told, beside what it says it is
                    // holding. A value sent in the wrong mode is accepted and
                    // ignored, and this is the only place that shows it.
                    if let Some(d) = demand.filter(|d| d.load_on) {
                        let held = format!("{:.3} {}", l.setpoint, mode.unit());
                        let disagrees =
                            (l.setpoint - d.load_value).abs() > (d.load_value * 0.02).max(0.01);
                        theme::readouts(
                            ui,
                            &[
                                ("mode", mode.label().to_string(), theme::READOUT),
                                (
                                    "set",
                                    format!("{:.3} {}", d.load_value, mode.unit()),
                                    theme::READOUT,
                                ),
                                (
                                    "cutoff",
                                    format!("{:.2} V", d.load_cutoff_v),
                                    theme::READOUT,
                                ),
                                (
                                    "holding",
                                    held,
                                    if disagrees { theme::FAULT } else { theme::TRACE },
                                ),
                            ],
                        );
                        if disagrees {
                            theme::note(
                                ui,
                                format!(
                                    "Load says it is holding {:.3} {}, not the {:.3} it was \
                                     told. The value went in while it was in another mode.",
                                    l.setpoint,
                                    mode.unit(),
                                    d.load_value
                                ),
                                theme::FAULT,
                            );
                        }
                    }
                    if wrong_battery {
                        theme::note(
                            ui,
                            format!(
                                "Load sees {:.2} V, pack is {pack_v:.2} V. Different battery, \
                                 or the leads are on something else.",
                                l.volts
                            ),
                            theme::FAULT,
                        );
                    }
                    ui.label(
                        RichText::new(last.as_ref().map(|u| u.load_name.clone()).unwrap_or_default())
                            .size(10.5)
                            .color(theme::LEGEND),
                    );
                }
                None => {
                    ui.label(theme::legend("not connected"));
                }
            },
        );
    }

    fn history_card(&mut self, ui: &mut egui::Ui, height: f32) {
        let cells = self.has_cells();
        if !cells {
            self.trace = Trace::Pack;
        }
        let mut trace = self.trace;
        theme::card(
            ui,
            None,
            |ui| {
                ui.label(theme::legend("history"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if cells {
                        ui.selectable_value(&mut trace, Trace::Cells, theme::legend("cells"));
                    }
                    ui.selectable_value(&mut trace, Trace::Pack, theme::legend("pack"));
                });
            },
            |ui| match self.trace {
                Trace::Pack => self.pack_plot(ui, height),
                Trace::Cells => self.cell_plot(ui, (height - 16.0).max(CHART_MIN_H)),
            },
        );
        self.trace = trace;
    }

    /// The stage rail. A charge is a sequence, so it is drawn as one, with
    /// the thing that ends the running stage spelled out under it rather than
    /// left for you to remember.
    fn stage_card(&mut self, ui: &mut egui::Ui) {
        let v = self.stage_view();
        let live = v.plan_at.is_some() || v.at.is_some();
        let tint = if v.faulted {
            theme::FAULT
        } else if v.done {
            theme::OK
        } else {
            theme::READOUT
        };
        let cycles = self
            .last
            .as_ref()
            .filter(|u| u.running && u.plan_repeat > 1)
            .map(|u| format!("cycle {} of {}", u.cycle + 1, u.plan_repeat));
        theme::card(
            ui,
            live.then_some(tint),
            |ui| {
                ui.label(theme::legend(match (live, v.done) {
                    (_, true) => "last run",
                    (true, _) => "running",
                    _ => "idle",
                }));
                if let Some(c) = &cycles {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(theme::legend(c));
                    });
                }
            },
            |ui| {
                // A cycle test is a sequence of steps, and a charge inside it
                // is a sequence of its own. Two rails, the plan over the
                // stage, so neither has to stand for the other.
                if v.plan.len() > 1 {
                    let names: Vec<&str> = v.plan.iter().map(|s| s.as_str()).collect();
                    theme::stage_rail(ui, &names, v.plan_at, tint, 26.0, !v.stages.is_empty());
                }
                if !v.stages.is_empty() {
                    if v.plan.len() > 1 {
                        ui.add_space(3.0);
                    }
                    let names: Vec<&str> = v.stages.iter().map(|s| s.as_str()).collect();
                    let h = if v.plan.len() > 1 { 20.0 } else { 26.0 };
                    theme::stage_rail(ui, &names, v.at, tint, h, false);
                }
                theme::exit_note(ui, &v.exit, tint);
            },
        );
    }

    fn stage_view(&self) -> StageView {
        let charge_stages = |mode: Mode, floats: bool| -> Vec<String> {
            let mut v = vec!["pre-charge".to_string(), "bulk".into()];
            match mode {
                Mode::BulkOnly => {}
                Mode::TopBalance => {
                    v.push("absorb".into());
                    v.push("balance".into());
                }
                Mode::Standard => v.push("absorb".into()),
            }
            v.push(if floats && mode != Mode::BulkOnly {
                "float".into()
            } else {
                "done".into()
            });
            v
        };
        let Some(u) = self.last.as_ref().filter(|u| u.running) else {
            return self.ended.clone().unwrap_or(StageView {
                stages: charge_stages(self.mode, self.floats()),
                ..StageView::default()
            });
        };
        let plan = u.plan_labels.clone();
        let plan_at = (!plan.is_empty()).then_some(u.step_index.min(plan.len() - 1));
        if let Some((mode, phase)) = u.charge_stage {
            let stages = charge_stages(mode, self.floats());
            let want = phase.label();
            let at = match phase {
                Phase::Done(_) => Some(stages.len() - 1),
                _ => stages.iter().position(|s| s == want),
            };
            let exit = match phase {
                Phase::Precharge => {
                    "feeding a small current until the pack is fit for a full one".to_string()
                }
                Phase::Bulk => format!(
                    "constant {:.2} A until the pack reaches {:.2} V or a cell reaches {} mV",
                    self.max_current, self.cv, self.target_mv
                ),
                Phase::Absorb => format!(
                    "holding {:.2} V, ends when the current falls to {:.2} A and stays there",
                    self.cv,
                    self.stop_current()
                ),
                Phase::Balance => format!(
                    "held at {} mV for the balancers, up to {:.0} h",
                    self.ceiling_mv, self.hold_hours
                ),
                Phase::Float => format!("maintaining {:.2} V", self.float_v),
                Phase::Done(_) => u.note.clone(),
            };
            return StageView {
                plan,
                plan_at,
                stages,
                at,
                exit,
                faulted: !u.note.is_empty() && matches!(phase, Phase::Done(_)),
                done: false,
            };
        }
        // A discharge and a rest have no stages of their own, so the plan
        // rail is the whole story and a second rail would only invent one.
        let exit = if u.step_label.starts_with("discharge") {
            let blind = self
                .last
                .as_ref()
                .and_then(|u| u.snapshot.as_ref())
                .is_none_or(|s| !s.has_cells());
            if blind {
                format!(
                    "{} {:.2} {} until the pack reaches {:.2} V",
                    self.discharge_mode.label(),
                    self.discharge_a,
                    self.discharge_mode.unit(),
                    self.pack_floor_v
                )
            } else {
                format!(
                    "{} {:.2} {} until the first cell reaches {} mV",
                    self.discharge_mode.label(),
                    self.discharge_a,
                    self.discharge_mode.unit(),
                    self.floor_mv
                )
            }
        } else if u.step_label.starts_with("rest") {
            "letting the pack settle before it is measured again".into()
        } else {
            u.note.clone()
        };
        StageView {
            plan,
            plan_at,
            stages: Vec::new(),
            at: None,
            exit,
            faulted: false,
            done: false,
        }
    }

    /// The comb. Every cell as a level in the window the pack is working in,
    /// with the limits you set ruled across it, because the pack is only as
    /// good as the cell nearest one of those rules.
    fn cells_card(&mut self, ui: &mut egui::Ui, height: f32) {
        let snapshot = self.last.as_ref().and_then(|u| u.snapshot.clone());
        let cells: Vec<theme::Cell> = snapshot
            .as_ref()
            .map(|s| {
                s.cells_mv
                    .iter()
                    .enumerate()
                    .map(|(i, mv)| theme::Cell {
                        mv: *mv,
                        balancing: s.balancing.contains(&i),
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.ease_cells(ui.ctx(), &cells);
        let spread = snapshot.as_ref().map(|s| s.spread_mv()).unwrap_or(0);
        let balancing = cells.iter().filter(|c| c.balancing).count();
        theme::card(
            ui,
            None,
            |ui| {
                ui.label(theme::legend("cells"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{spread} mV"))
                            .font(theme::figure(theme::VALUE_SIZE))
                            .color(if spread > 100 {
                                theme::READOUT
                            } else {
                                theme::VALUE
                            }),
                    );
                    ui.label(theme::legend("spread"));
                    if balancing > 0 {
                        ui.add_space(10.0);
                        ui.label(theme::legend(format!("{balancing} balancing")));
                    }
                });
            },
            |ui| {
                theme::comb(
                    ui,
                    &cells,
                    &self.shown_mv,
                    self.ceiling_mv,
                    self.target_mv,
                    self.floor_mv,
                    height,
                );
            },
        );
    }

    /// Bars ease to the new reading rather than snapping to it, so a poll
    /// that moves one cell reads as that cell moving.
    fn ease_cells(&mut self, ctx: &egui::Context, cells: &[theme::Cell]) {
        let now = Instant::now();
        let dt = (now - self.shown_at).as_secs_f32().min(0.1);
        self.shown_at = now;
        self.shown_mv.resize(cells.len(), 0.0);
        let k = 1.0 - (-dt / 0.06).exp();
        let mut moving = false;
        for (shown, cell) in self.shown_mv.iter_mut().zip(cells) {
            let target = cell.mv as f32;
            if *shown == 0.0 {
                *shown = target;
                continue;
            }
            if (target - *shown).abs() > 0.25 {
                *shown += (target - *shown) * k;
                moving = true;
            } else {
                *shown = target;
            }
        }
        if moving {
            ctx.request_repaint();
        }
    }

    fn devices_card(&mut self, ui: &mut egui::Ui) {
        for pick in [
            &mut self.pack_pick,
            &mut self.charger_pick,
            &mut self.load_pick,
        ] {
            pick.poll();
        }
        let mut reconnect = false;
        let mut rescan = false;
        let mut header_rescan = false;
        let scanning_any = self.pack_pick.scanning()
            || self.charger_pick.scanning()
            || self.load_pick.scanning();
        theme::card(
            ui,
            None,
            |ui| {
                ui.label(theme::legend("devices"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("rescan").clicked() {
                        header_rescan = true;
                    }
                });
            },
            |ui| {
                for pick in [
                    &mut self.pack_pick,
                    &mut self.charger_pick,
                    &mut self.load_pick,
                ] {
                    ui.horizontal(|ui| {
                        ui.label(theme::legend(pick.role));
                        if pick.role != "battery" {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.checkbox(&mut pick.enabled, "");
                                },
                            );
                        }
                    });
                    let w = ui.available_width().max(0.0);
                    let label = elide(pick.backend().map(|b| b.label).unwrap_or("none"), 36);
                    egui::ComboBox::from_id_salt(("backend", pick.role))
                        .selected_text(RichText::new(label).size(11.5))
                        .width(w)
                        .show_ui(ui, |ui| {
                            for (i, b) in pick.backends.iter().enumerate() {
                                if ui
                                    .selectable_value(
                                        &mut pick.backend,
                                        i,
                                        RichText::new(elide(b.label, 40)).size(11.5),
                                    )
                                    .changed()
                                {
                                    rescan = true;
                                }
                            }
                        });
                    if pick.scanning() {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(12.0));
                            ui.label(theme::legend("scanning"));
                        });
                        ui.add_space(4.0);
                        continue;
                    }
                    if !pick.has_ports() {
                        ui.add_space(4.0);
                        continue;
                    }
                    let target = pick
                        .selected()
                        .map(|c| elide(&c.label, 36))
                        .unwrap_or_else(|| "nothing found".into());
                    egui::ComboBox::from_id_salt(("target", pick.role))
                        .selected_text(RichText::new(target).size(11.0))
                        .width(w)
                        .show_ui(ui, |ui| {
                            for (i, c) in pick.candidates.iter().enumerate() {
                                let text = elide(&c.label, 40);
                                ui.selectable_value(
                                    &mut pick.target,
                                    i,
                                    RichText::new(if c.matches_ids {
                                        format!("* {text}")
                                    } else {
                                        text
                                    })
                                    .size(11.0),
                                )
                                .on_hover_text(&c.target);
                            }
                        });
                    ui.add_space(4.0);
                }
                // Connecting mid-scan would open whatever the half-finished
                // list happens to hold, or start a second scan to find a
                // default target.
                let busy = scanning_any;
                let pending = self.pending_connect();
                ui.add_enabled_ui(!busy, |ui| {
                    let text = if pending {
                        RichText::new("Connect")
                            .font(theme::legend_font(theme::VALUE_SIZE + 0.5))
                            .color(theme::READOUT)
                            .strong()
                    } else {
                        theme::action("Connect")
                    };
                    if ui.button(text).clicked() {
                        reconnect = true;
                    }
                });
                if pending {
                    // Changing a dropdown does nothing until the session is
                    // reopened, and a stale session looks exactly like a
                    // device that will not start.
                    theme::note(
                        ui,
                        "Selection changed: press Connect to use it.",
                        theme::READOUT,
                    );
                }
            },
        );
        if rescan || header_rescan {
            for pick in [
                &mut self.pack_pick,
                &mut self.charger_pick,
                &mut self.load_pick,
            ] {
                pick.rescan();
            }
        }
        if reconnect {
            self.connect();
        }
    }

    fn profile_card(&mut self, ui: &mut egui::Ui) {
        let before = self.profile;
        let mut apply = false;
        let capacity = self.capacity_ah();
        let snapshot = self.last.as_ref().and_then(|u| u.snapshot.clone());
        let from_bms = snapshot.as_ref().map(|s| s.has_cells()).unwrap_or(false);
        let rated_ah = snapshot.as_ref().and_then(|s| s.rated_ah).filter(|a| *a > 0.0);
        theme::card(
            ui,
            None,
            |ui| {
                ui.label(theme::legend("battery"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(theme::legend(format!("{capacity:.0} Ah")));
                });
            },
            |ui| {
                // A BMS reports its cells and often its rating; it never
                // reports chemistry. So that is the only thing left to choose
                // when one is connected.
                egui::ComboBox::from_id_salt("chemistry")
                    .selected_text(self.profile.chemistry.label())
                    .width(ui.available_width().max(0.0))
                    .show_ui(ui, |ui| {
                        for c in Chemistry::ALL {
                            ui.selectable_value(&mut self.profile.chemistry, c, c.label());
                        }
                    });
                // A rate typed for one chemistry means nothing for the next:
                // C/5 is gentle on lithium and abuse on lead-acid.
                if self.profile.chemistry != before.chemistry {
                    self.charge_c = self.profile.chemistry.default_charge_c();
                    self.discharge_c = self.profile.chemistry.default_discharge_c();
                }
                if from_bms {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{}S", self.profile.series))
                                .size(theme::VALUE_SIZE)
                                .color(theme::VALUE),
                        );
                        if let Some(ah) = rated_ah {
                            ui.label(
                                RichText::new(format!("{ah:.0} Ah"))
                                    .size(theme::VALUE_SIZE)
                                    .color(theme::VALUE),
                            );
                        }
                        ui.label(theme::legend("from the bms"));
                    });
                    if rated_ah.is_none() {
                        let mut parallel = self.profile.parallel as f64;
                        field(ui, "parallel", &mut parallel, 1.0..=32.0, 1.0, 0);
                        self.profile.parallel = parallel as u16;
                        field(ui, "cell Ah", &mut self.profile.cell_ah, 0.5..=1000.0, 1.0, 1);
                    }
                } else {
                    let mut series = self.profile.series as f64;
                    field(ui, "series", &mut series, 1.0..=64.0, 1.0, 0);
                    self.profile.series = series as u16;
                    let mut parallel = self.profile.parallel as f64;
                    field(ui, "parallel", &mut parallel, 1.0..=32.0, 1.0, 0);
                    self.profile.parallel = parallel as u16;
                    field(ui, "cell Ah", &mut self.profile.cell_ah, 0.5..=1000.0, 1.0, 1);
                }
                field(ui, "charge C", &mut self.charge_c, 0.01..=3.0, 0.05, 2);
                field(ui, "discharge C", &mut self.discharge_c, 0.01..=3.0, 0.05, 2);
                if self.profile.chemistry == Chemistry::LeadAcid {
                    theme::note(
                        ui,
                        "Lead-acid capacity is quoted at the 20 hour rate, so C/20 out is \
                         what the rating on the label means. Above C/10 in it gasses.",
                        theme::LEGEND,
                    );
                }
                theme::note(
                    ui,
                    format!(
                        "{:.2} V charge, {:.2} V float, {:.2} V floor, {:.2} A in, {:.2} A out",
                        self.profile.charge_v(),
                        self.profile.float_v(),
                        self.profile.floor_v(),
                        (capacity * self.charge_c).max(0.1),
                        (capacity * self.discharge_c).max(0.1)
                    ),
                    theme::LEGEND,
                );
                if ui.button(theme::action("Apply to limits")).clicked() {
                    apply = true;
                }
            },
        );
        if apply {
            self.apply_profile();
        }
        // A blind pack works out its SOC from the profile, so it needs to know
        // the moment the profile changes.
        if self.profile != before {
            self.send(Command::SetProfile(self.profile));
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        // The side panel says what was asked for; the cards across the top say
        // what the hardware reports. Keep the two apart so a disagreement is
        // visible rather than averaged away.
        let wanted = self
            .last
            .as_ref()
            .map(|u| u.running && u.step_label.starts_with("charge"))
            .unwrap_or(false);
        theme::card(
            ui,
            Some(if wanted { theme::OK } else { theme::ETCH }),
            |ui| {
                ui.label(theme::legend("charge"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::lamp(ui, if wanted { "charging" } else { "idle" }, wanted, false);
                });
            },
            |ui| {
                if !self.has_charger() {
                    theme::note(ui, self.missing("charger"), theme::FAULT);
                    return;
                }
                // What the stages mean, and what the cells are held to, is
                // settled when the run starts. Changing either half way up a
                // charge changes what the machine already decided.
                let locked = self.charging();
                ui.add_enabled_ui(!locked, |ui| {
                    egui::ComboBox::from_id_salt("mode")
                        .selected_text(match self.mode {
                            Mode::Standard => "Standard",
                            Mode::TopBalance => "Top balance",
                            Mode::BulkOnly => "Bulk only",
                        })
                        .width(ui.available_width().max(0.0))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.mode, Mode::Standard, "Standard");
                            ui.selectable_value(&mut self.mode, Mode::TopBalance, "Top balance");
                            ui.selectable_value(&mut self.mode, Mode::BulkOnly, "Bulk only");
                        });
                });
                theme::note(
                    ui,
                    match self.mode {
                        Mode::Standard => {
                            "Pre-charge if flat, bulk, absorb, then stop (or float lead-acid)."
                        }
                        Mode::TopBalance => "Absorb, then hold at the ceiling for the balancers.",
                        Mode::BulkOnly => "Constant current only: stop at the ceiling, no absorb.",
                    },
                    theme::LEGEND,
                );
                ui.add_space(4.0);
                let before = self.tuning();
                field(ui, "max A", &mut self.max_current, 0.2..=10.0, 0.1, 2);
                let cv_before = self.cv;
                ui.add_enabled_ui(!locked, |ui| {
                    field(ui, "CV V", &mut self.cv, 1.0..=150.0, 0.1, 2);
                    if self.mode == Mode::Standard {
                        field(ui, "float V", &mut self.float_v, 1.0..=150.0, 0.1, 2);
                    }
                });
                // The setpoint is what full means on a pack with no gauge,
                // so a change to it has to reach the estimate as well as the
                // supply, or stopping at a state of charge aims at a figure
                // the panel never shows.
                if (self.cv - cv_before).abs() > f64::EPSILON {
                    self.profile.set_charge_v(self.cv);
                    self.ceiling_mv = self.profile.cell().ceiling_mv;
                    self.send(Command::SetProfile(self.profile));
                }
                let mut term = self.stop_current();
                field(ui, "stop A", &mut term, 0.05..=20.0, 0.05, 2);
                if (term - self.stop_current()).abs() > f64::EPSILON {
                    self.i_term = Some(term);
                }
                theme::note(
                    ui,
                    "Absorption ends when the pack stops taking this much: C/20 by default.",
                    theme::LEGEND,
                );
                if self.blind() {
                    theme::note(
                        ui,
                        "No BMS: the CV setpoint is the ceiling, and the stop current ends it.",
                        theme::LEGEND,
                    );
                } else {
                    ui.add_enabled_ui(!locked, |ui| {
                        let mut ceiling = self.ceiling_mv as f64;
                        field(ui, "ceiling mV", &mut ceiling, 2000.0..=4300.0, 5.0, 0);
                        self.ceiling_mv = ceiling as u16;
                    });
                }
                soc_stop(ui, "stop at soc", &mut self.charge_to_soc, &mut self.target_soc);
                if theme::toggle(ui, "stop on bms alarm", self.stop_on_alarm).clicked() {
                    self.stop_on_alarm = !self.stop_on_alarm;
                }
                if self.stop_on_alarm {
                    ui.horizontal(|ui| {
                        ui.label(theme::legend("except"));
                        ui.add_sized(
                            [150.0, 18.0],
                            egui::TextEdit::singleline(&mut self.alarm_ignore_list)
                                .hint_text("balanc, charging"),
                        );
                    });
                } else {
                    theme::note(
                        ui,
                        "The pack's own protection is the only instrument wired to every \
                         cell. Charging through it is your call.",
                        theme::FAULT,
                    );
                }
                if self.mode != Mode::BulkOnly {
                    field(ui, "absorb h", &mut self.absorb_hours, 0.5..=24.0, 0.5, 1);
                    theme::note(
                        ui,
                        "Longest absorption before it gives up waiting for the stop current.",
                        theme::LEGEND,
                    );
                }
                if self.mode == Mode::TopBalance {
                    ui.add_enabled_ui(!locked, |ui| {
                        let mut target = self.target_mv as f64;
                        field(ui, "target mV", &mut target, 2000.0..=4300.0, 5.0, 0);
                        self.target_mv = target as u16;
                    });
                    field(ui, "hold h", &mut self.hold_hours, 1.0..=72.0, 1.0, 0);
                } else if self.floats() {
                    field(ui, "float h", &mut self.hold_hours, 1.0..=72.0, 1.0, 0);
                    theme::note(
                        ui,
                        "Lead-acid: held at the float voltage this long after terminating.",
                        theme::LEGEND,
                    );
                }
                if locked {
                    theme::note(
                        ui,
                        "Charging: currents and clocks are live, the voltages and the mode \
                         are settled until it stops.",
                        theme::LEGEND,
                    );
                }
                // Everything above that a running charge will still obey.
                let now = self.tuning();
                if locked && now != before {
                    self.send(Command::Tune(now));
                }
                ui.add_space(6.0);
                let pending = self.pending_connect();
                if pending {
                    theme::note(
                        ui,
                        "Devices changed: press Connect before starting.",
                        theme::READOUT,
                    );
                }
                let busy = self.busy().filter(|_| !self.charging());
                if let Some(what) = &busy {
                    theme::note(ui, format!("{what}: stop it to charge."), theme::LEGEND);
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !pending && busy.is_none(),
                            egui::Button::new(theme::action("Charge")),
                        )
                        .clicked()
                    {
                        self.send(Command::Start(self.plan(false)));
                    }
                    if ui
                        .button(RichText::new("Stop").font(theme::legend_font(theme::VALUE_SIZE + 0.5)).color(theme::FAULT))
                        .clicked()
                    {
                        self.send(Command::Stop);
                    }
                });
            },
        );
    }

    fn discharge_card(&mut self, ui: &mut egui::Ui) {
        let last = self.last.clone();
        let wanted = last
            .as_ref()
            .map(|u| u.running && u.step_label.starts_with("discharge"))
            .unwrap_or(false);
        let manual = last.as_ref().is_some_and(|u| u.load_manual);
        theme::card(
            ui,
            Some(if wanted { theme::OK } else { theme::ETCH }),
            |ui| {
                ui.label(theme::legend("discharge"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = match (wanted, manual) {
                        (true, true) => "connect load",
                        (true, false) => "discharging",
                        (false, _) => "idle",
                    };
                    theme::lamp(ui, label, wanted, false);
                });
            },
            |ui| {
                if !self.has_load() {
                    theme::note(ui, self.missing("load"), theme::FAULT);
                    return;
                }
                // A resistor bank draws what it draws, cannot be switched
                // from here, and stops when you disconnect it. There is
                // nothing on this card it would obey.
                if manual {
                    theme::note(
                        ui,
                        format!(
                            "{} cannot be driven from here: switch it by hand and watch \
                             the battery card.",
                            last.as_ref().map(|u| u.load_name.clone()).unwrap_or_default()
                        ),
                        theme::LEGEND,
                    );
                    return;
                }
                let modes: Vec<LoadMode> = last
                    .as_ref()
                    .map(|u| u.load_modes.clone())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| vec![LoadMode::Cc]);
                // What the load holds constant cannot change part way
                // through: the amp-hours would be two tests added together.
                // Everything under it can.
                let running = self.discharging();
                let before = self.load_tuning();
                let mut picked = self.discharge_mode;
                ui.add_enabled_ui(!running, |ui| {
                    egui::ComboBox::from_id_salt("load_mode")
                        .selected_text(self.discharge_mode.label())
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for m in modes {
                                ui.selectable_value(
                                    &mut picked,
                                    m,
                                    format!("{} ({})", m.label(), m.unit()),
                                );
                            }
                        });
                });
                if picked != self.discharge_mode {
                    self.discharge_a =
                        self.convert_setpoint(self.discharge_mode, picked, self.discharge_a);
                    self.discharge_mode = picked;
                }
                let (range, decimals) = match self.discharge_mode {
                    LoadMode::Cc => (0.1..=30.0, 2),
                    LoadMode::Cv => (1.0..=150.0, 1),
                    LoadMode::Cr => (0.1..=1000.0, 1),
                    LoadMode::Cp => (1.0..=300.0, 0),
                };
                field(
                    ui,
                    self.discharge_mode.unit(),
                    &mut self.discharge_a,
                    range,
                    0.1,
                    decimals,
                );
                if self.blind() {
                    field(ui, "floor V", &mut self.pack_floor_v, 1.0..=150.0, 0.1, 2);
                } else {
                    let mut floor = self.floor_mv as f64;
                    field(ui, "floor mV", &mut floor, 1500.0..=3300.0, 10.0, 0);
                    self.floor_mv = floor as u16;
                }
                soc_stop(
                    ui,
                    "stop at soc",
                    &mut self.discharge_to_soc,
                    &mut self.target_soc,
                );
                let now = self.load_tuning();
                if running && now != before {
                    self.send(Command::TuneLoad(now));
                }
                theme::note(
                    ui,
                    match self.discharge_mode {
                        LoadMode::Cc => "Constant current until the first cell reaches its floor.",
                        LoadMode::Cv => "Constant voltage: draw falls as the pack sags.",
                        LoadMode::Cr => "Constant resistance: like a fixed load bank.",
                        LoadMode::Cp => "Constant power: current rises as the pack sags.",
                    },
                    theme::LEGEND,
                );
                ui.add_space(4.0);
                let pending = self.pending_connect();
                let busy = self.busy().filter(|_| !wanted);
                if let Some(what) = &busy {
                    theme::note(ui, format!("{what}: stop it to discharge."), theme::LEGEND);
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !pending && busy.is_none(),
                            egui::Button::new(theme::action("Discharge")),
                        )
                        .clicked()
                    {
                        self.send(Command::Start(Plan::discharge(self.discharge_config())));
                    }
                    if ui
                        .button(
                            RichText::new("Stop")
                                .font(theme::legend_font(theme::VALUE_SIZE + 0.5))
                                .color(theme::FAULT),
                        )
                        .clicked()
                    {
                        self.send(Command::Stop);
                    }
                });
                if let Some(u) = last.as_ref()
                    && u.running
                    && u.plan_steps == 1
                    && u.step_label.starts_with("discharge")
                {
                    theme::note(ui, u.note.clone(), theme::READOUT);
                }
                if let Some(ah) = last.as_ref().and_then(|u| u.measured_ah) {
                    ui.label(
                        RichText::new(format!("last discharge {ah:.3} Ah"))
                            .size(11.5)
                            .color(theme::READOUT),
                    );
                }
            },
        );
    }

    fn plan_card(&mut self, ui: &mut egui::Ui) {
        let last = self.last.clone();
        // A single charge runs through the same runner, so a plan is only
        // "running" here when it actually has steps beyond that.
        let running = last
            .as_ref()
            .map(|u| u.running && u.plan_steps > 1)
            .unwrap_or(false);
        theme::card(
            ui,
            Some(if running { theme::OK } else { theme::ETCH }),
            |ui| {
                ui.label(theme::legend("cycle plan"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::lamp(ui, if running { "running" } else { "idle" }, running, false);
                });
            },
            |ui| {
                // A capacity test needs both ends: something to fill the
                // pack and something to empty it while counting.
                // Both ends have to be drivable: a plan that cannot switch
                // the load cannot time a discharge, and a plan that cannot
                // switch the supply cannot charge between them.
                let drivable_load = self.has_load() && !self.load_manual();
                if !self.has_charger() || !drivable_load {
                    let what = match (self.has_charger(), drivable_load) {
                        (false, false) => "charger or load",
                        (false, true) => "charger",
                        _ => "load",
                    };
                    let why = if self.has_load() && self.load_manual() {
                        "A cycle test has to switch the load itself. This one is manual."
                            .to_string()
                    } else {
                        self.missing(what)
                    };
                    theme::note(ui, why, theme::FAULT);
                    return;
                }
                field(ui, "rest min", &mut self.rest_min, 0.0..=600.0, 5.0, 0);
                let mut repeat = self.repeat as f64;
                field(ui, "cycles", &mut repeat, 1.0..=20.0, 1.0, 0);
                self.repeat = repeat as usize;
                theme::note(
                    ui,
                    "Charge, rest, discharge counting Ah, rest.",
                    theme::LEGEND,
                );
                ui.add_space(4.0);
                let busy = self.busy().filter(|_| !running);
                if let Some(what) = &busy {
                    theme::note(ui, format!("{what}: stop it to run a test."), theme::LEGEND);
                }
                if ui
                    .add_enabled(
                        busy.is_none(),
                        egui::Button::new(theme::action("Run capacity test")),
                    )
                    .clicked()
                {
                    self.send(Command::Start(self.plan(true)));
                }
                if let Some(u) = last.as_ref()
                    && u.plan_steps > 1
                    && (u.running || !u.results.is_empty())
                {
                    ui.add_space(4.0);
                    ui.separator();
                    if u.running {
                        ui.label(
                            RichText::new(format!(
                                "cycle {} / {}: {}",
                                u.cycle + 1,
                                u.plan_repeat.max(1),
                                u.step_label
                            ))
                            .size(11.5)
                            .color(theme::READOUT),
                        );
                        theme::note(ui, u.note.clone(), theme::LEGEND);
                    }
                    for r in u.results.iter().rev().take(6) {
                        ui.label(
                            RichText::new(match r.amp_hours {
                                Some(ah) => format!(
                                    "c{} {} -> {} {:.3} Ah",
                                    r.cycle, r.label, r.outcome, ah
                                ),
                                None => format!("c{} {} -> {}", r.cycle, r.label, r.outcome),
                            })
                            .size(10.5)
                            .color(theme::VALUE),
                        );
                    }
                }
            },
        );
    }

    fn pack_plot(&self, ui: &mut egui::Ui, height: f32) {
        // Scale to the pack, not to the data: auto-scaling a resting battery
        // turns 20 mV of noise into a mountain range. The window runs from the
        // discharge floor to just past the charge ceiling.
        let cells = self
            .history
            .back()
            .map(|s| s.cells_mv.len())
            .or_else(|| {
                self.last
                    .as_ref()
                    .and_then(|u| u.snapshot.as_ref())
                    .map(|s| s.cells_mv.len())
            })
            .unwrap_or(0);
        // A pack with no BMS reports no cells, but the profile still knows
        // how many are in there, and a window from zero to sixty volts turns
        // a 4S charge into a flat line across the bottom.
        let n = if cells > 0 {
            cells
        } else {
            self.profile.series.max(1) as usize
        } as f64;
        let (v_lo, v_hi) = (
            (n * self.floor_mv as f64 / 1000.0).min(self.pack_floor_v),
            (n * (self.ceiling_mv + 60) as f64 / 1000.0).max(self.cv + 0.5),
        );
        let a_max = self.max_current.max(self.discharge_a).max(1.0) * 1.15;

        let volts = chart::Trace {
            name: "pack V",
            color: theme::TRACE,
            points: self.history.iter().map(|s| [s.minutes, s.pack_v]).collect(),
            right: false,
        };
        let amps = chart::Trace {
            name: "current A",
            color: theme::READOUT,
            points: self
                .history
                .iter()
                .map(|s| [s.minutes, s.current_a])
                .collect(),
            right: true,
        };
        chart::strip(
            ui,
            height,
            &chart::Axis {
                label: "V",
                lo: v_lo,
                hi: v_hi,
                decimals: 1,
            },
            Some(&chart::Axis {
                label: "A",
                lo: -a_max,
                hi: a_max,
                decimals: 1,
            }),
            &[volts, amps],
        );
    }

    fn cell_plot(&self, ui: &mut egui::Ui, height: f32) {
        let n = self
            .history
            .back()
            .map(|s| s.cells_mv.len())
            .unwrap_or_default();
        let (mut lo, mut hi) = (f64::MAX, f64::MIN);
        for s in &self.history {
            for mv in &s.cells_mv {
                lo = lo.min(*mv as f64);
                hi = hi.max(*mv as f64);
            }
        }
        if lo > hi {
            (lo, hi) = (3000.0, 3600.0);
        }
        if hi - lo < 20.0 {
            let mid = (hi + lo) / 2.0;
            (lo, hi) = (mid - 10.0, mid + 10.0);
        }
        let pad = (hi - lo) * 0.08;
        let width = n.saturating_sub(1).to_string().len();
        let traces: Vec<chart::Trace> = (0..n)
            .map(|i| chart::Trace {
                name: "",
                color: cell_color(i, n),
                points: self
                    .history
                    .iter()
                    .filter_map(|s| s.cells_mv.get(i).map(|mv| [s.minutes, *mv as f64]))
                    .collect(),
                right: false,
            })
            .collect();
        chart::strip(
            ui,
            height,
            &chart::Axis {
                label: "mV",
                lo: lo - pad,
                hi: hi + pad,
                decimals: 0,
            },
            None,
            &traces,
        );
        // The colours are the table's order; name them under the chart rather
        // than stacking fifteen keys on top of the traces.
        ui.horizontal_wrapped(|ui| {
            for i in 0..n {
                ui.label(
                    RichText::new(format!("c{i:0width$}"))
                        .font(theme::mono(10.0))
                        .color(cell_color(i, n)),
                );
            }
        });
    }
}

/// Keep the ends of a long device path, which is where the useful part is,
/// so a serial id cannot push the panel wider than it should be.
fn elide(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1) / 2;
    let head: String = text.chars().take(keep).collect();
    let tail: String = text.chars().skip(n - keep).collect();
    format!("{head}\u{2026}{tail}")
}

/// A stop-at-SOC control: off by default, because a partial charge is
/// something you ask for (storage at 50%), not a default.
///
/// The value only appears once it is switched on. A greyed-out number beside
/// every other setting reads as a setting, and this one does nothing until
/// you ask for it.
fn soc_stop(ui: &mut egui::Ui, label: &str, on: &mut bool, value: &mut f64) {
    ui.horizontal(|ui| {
        if theme::toggle(ui, label, *on).clicked() {
            *on = !*on;
        }
        if *on {
            ui.add_sized(
                [58.0, 18.0],
                egui::DragValue::new(value)
                    .range(5.0..=100.0)
                    .fixed_decimals(0)
                    .suffix("%"),
            );
        }
    });
}

/// A runtime as the instrument's own screen writes it, because "45 min" and
/// "46 min" are the same number to anyone comparing the two.
fn hms(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s / 3600 {
        0 => format!("{}:{:02}", s / 60, s % 60),
        h => format!("{h}:{:02}:{:02}", (s / 60) % 60, s % 60),
    }
}

/// One labelled numeric control. A slider collapses to nothing in a narrow
/// card; a drag field keeps its size and can be typed into.
fn field(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f64,
    range: std::ops::RangeInclusive<f64>,
    step: f64,
    decimals: usize,
) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [78.0, 18.0],
            egui::DragValue::new(value)
                .range(range)
                .speed(step)
                .fixed_decimals(decimals),
        );
        ui.label(theme::legend(label));
    });
}

/// The supply is switched off from `Drop`, which a signal does not run, so the
/// shutdown path is explicit: tell the live worker to quit, wait for it to let
/// go of the hardware, then exit.
///
/// The sender is replaced on every reconnect. Holding only the first one (as a
/// `OnceLock` does) means Ctrl-C signals a worker that died hours ago and the
/// charger keeps delivering.
static SHUTDOWN: std::sync::Mutex<Option<std::sync::mpsc::Sender<Command>>> =
    std::sync::Mutex::new(None);

fn stop_hardware_on_signal() {
    cycler_core::interrupt::install();
    std::thread::spawn(move || {
        while !cycler_core::interrupt::requested() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let tx = SHUTDOWN.lock().ok().and_then(|g| g.clone());
        if let Some(tx) = tx {
            let _ = tx.send(Command::Quit);
            // The worker stops the hardware and exits; give it long enough
            // for a serial round trip before killing the process.
            std::thread::sleep(Duration::from_millis(1500));
        }
        std::process::exit(0);
    });
}

fn main() -> eframe::Result<()> {
    let demo = std::env::args().any(|a| a == "--demo");
    let log = std::env::var_os("CYCLER_LOG").map(PathBuf::from);
    stop_hardware_on_signal();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([980.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "cycler",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(if demo { App::demo() } else { App::new(log) }))
        }),
    )
}
