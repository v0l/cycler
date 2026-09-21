mod chart;
mod devices;
mod theme;
mod worker;

use cycler_core::charge::{Config, Mode};
use cycler_core::cycle::Plan;
use cycler_core::chemistry::{Chemistry, PackProfile};
use cycler_core::device::LoadMode;
use cycler_core::discharge;
use cycler_core::device::{CHARGER_BACKENDS, LOAD_BACKENDS};
use cycler_core::pack::{PACK_BACKENDS, Snapshot};
use devices::{Choice, Remembered};
use egui::{Color32, RichText};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use worker::{Command, Session, Update};

const HISTORY: usize = 7200;
/// The chart is a strip under the cards, not the centre of the screen.
const CHART_H: f32 = 264.0;

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
    profile: PackProfile,
    series_detected: bool,
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
    float_v: f64,
    charge_to_soc: bool,
    discharge_to_soc: bool,
    target_soc: f64,
    discharge_mode: LoadMode,
    discharge_a: f64,
    floor_mv: u16,
    rest_min: f64,
    repeat: usize,
}

impl App {
    fn new(log: Option<PathBuf>) -> Self {
        let mut app = Self {
            session: None,
            log,
            remembered: Remembered::default(),
            profile: PackProfile::default(),
            series_detected: false,
            pack_floor_v: PackProfile::default().floor_v(),
            pack_pick: Choice::new("battery", PACK_BACKENDS),
            charger_pick: Choice::new("charger", CHARGER_BACKENDS),
            load_pick: Choice::new("load", LOAD_BACKENDS),
            history: VecDeque::with_capacity(HISTORY),
            trace: Trace::Pack,
            started: Instant::now(),
            last: None,
            error: None,
            mode: Mode::Auto,
            cv: 51.5,
            max_current: 3.0,
            ceiling_mv: 3500,
            target_mv: 3450,
            hold_hours: 48.0,
            float_v: 51.0,
            charge_to_soc: false,
            discharge_to_soc: false,
            target_soc: 50.0,
            discharge_mode: LoadMode::Cc,
            discharge_a: 3.0,
            floor_mv: 3000,
            rest_min: 30.0,
            repeat: 1,
        };
        app.remembered = Remembered::load();
        if let Some(p) = app.remembered.profile {
            app.profile = p;
            app.apply_profile();
        }
        app.remembered.apply([
            &mut app.pack_pick,
            &mut app.charger_pick,
            &mut app.load_pick,
        ]);
        app.connect();
        app
    }

    fn connect(&mut self) {
        self.remembered.profile = Some(self.profile);
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
        self.session = Some(session);
    }

    fn config(&self) -> Config {
        Config {
            mode: self.mode,
            pack_cv: self.cv,
            cell_ceiling_mv: self.ceiling_mv,
            cell_hard_mv: self.ceiling_mv + 50,
            cell_resume_mv: self.ceiling_mv.saturating_sub(40),
            cell_target_mv: self.target_mv,
            float_v: self.float_v,
            stop_at_soc: self.charge_to_soc.then_some(self.target_soc as u8),
            i_max: self.max_current,
            hold_max: Duration::from_secs_f64(self.hold_hours * 3600.0),
            ..Default::default()
        }
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
            pack_floor_v: self.pack_floor_v,
            cell_floor_mv: self.floor_mv,
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
        self.float_v = self.profile.float_v();
        self.ceiling_mv = cell.ceiling_mv;
        self.target_mv = cell.balance_mv;
        self.floor_mv = cell.floor_mv;
        self.pack_floor_v = self.profile.floor_v();
        // A gentle test: fill and empty at a fifth of capacity.
        self.max_current = self.profile.current_at_c(0.2);
        self.discharge_a = self.profile.current_at_c(0.2);
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
                    self.series_detected = true;
                } else if !self.series_detected && s.pack_v > 1.0 {
                    // No BMS: a resting voltage is the only clue to how many
                    // cells are in there, and it is a guess until told
                    // otherwise.
                    self.profile.series = self.profile.chemistry.series_from_voltage(s.pack_v);
                    self.series_detected = true;
                }
            }
            self.error = u.error.clone();
            self.last = Some(u);
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
                        RichText::new("cycler")
                            .size(15.0)
                            .strong()
                            .color(theme::VALUE),
                    );
                    if let Some(p) = &self.log {
                        ui.label(theme::legend(format!("logging {}", p.display())));
                    }
                    if let Some(e) = &self.error {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(e).size(11.5).color(theme::FAULT));
                        });
                    }
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
                let height = ui.available_height();
                ui.horizontal_top(|ui| {
                    let left = 372.0_f32.min(ui.available_width() * 0.42).max(0.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(left, height.max(0.0)),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| self.battery_card(ui),
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width().max(0.0), height.max(0.0)),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.spacing_mut().item_spacing.y = 8.0;
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
                            self.history_card(ui);
                        },
                    );
                });
            });
    }
}

impl App {
    fn battery_card(&mut self, ui: &mut egui::Ui) {
        let snapshot = self.last.as_ref().and_then(|u| u.snapshot.clone());
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
        let ceiling = self.ceiling_mv;
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
                Some(s) => {
                    theme::readouts(
                        ui,
                        &[
                            ("pack", format!("{:.3} V", s.pack_v), theme::TRACE),
                            ("current", format!("{:+.2} A", s.current_a), theme::TRACE),
                            (
                                "power",
                                format!("{:+.1} W", s.pack_v * s.current_a),
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
                        theme::note(ui, s.alarms.join(", "), theme::FAULT);
                    }
                    if !s.has_cells() {
                        theme::note(
                            ui,
                            "No BMS: limits are pack voltage only, and nothing here \
                             knows what the cells are doing.",
                            theme::LEGEND,
                        );
                        return;
                    }
                    ui.add_space(2.0);
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.label(theme::legend("cells"));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(theme::legend(format!("spread {} mV", s.spread_mv())));
                        });
                    });
                    egui::ScrollArea::vertical().show(ui, |ui| cells_table(ui, s, ceiling));
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
        theme::card(
            ui,
            Some(if on { theme::OK } else { theme::ETCH }),
            |ui| {
                ui.label(theme::legend("charger"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(r) = last.as_ref().and_then(|u| u.charger_regulation)
                        && on
                    {
                        ui.label(theme::legend(r.label()));
                    }
                    theme::lamp(ui, if on { "output on" } else { "output off" }, on, disagrees);
                });
            },
            |ui| {
                let (v, a) = last.as_ref().and_then(|u| u.charger).unwrap_or((0.0, 0.0));
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
                        ("out", format!("{v:.2} V"), theme::TRACE),
                        ("current", format!("{a:.2} A"), theme::TRACE),
                        ("power", format!("{:.1} W", v * a), theme::TRACE),
                        (
                            "set",
                            format!(
                                "{:.2} A",
                                last.as_ref().map(|u| u.demand.charger_a).unwrap_or(0.0)
                            ),
                            theme::READOUT,
                        ),
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
        theme::card(
            ui,
            Some(if on { theme::OK } else { theme::ETCH }),
            |ui| {
                ui.label(theme::legend("load"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::lamp(ui, if on { "load on" } else { "load off" }, on, false);
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
                            ("draw", format!("{draw_a:.2} A"), theme::TRACE),
                            ("power", format!("{:.1} W", draw_a * volts), theme::TRACE),
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
                    theme::readouts(
                        ui,
                        &[
                            ("in", format!("{:.2} V", l.volts), theme::TRACE),
                            ("draw", format!("{:.2} A", l.amps), theme::TRACE),
                            ("power", format!("{:.1} W", l.watts), theme::TRACE),
                            ("drawn", format!("{:.3} Ah", l.amp_hours), theme::READOUT),
                            ("energy", format!("{:.1} Wh", l.watt_hours), theme::VALUE),
                            ("temp", format!("{:.0} C", l.temp_c), theme::VALUE),
                            ("run", format!("{:.0} min", l.runtime_s / 60.0), theme::VALUE),
                        ],
                    );
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

    fn history_card(&mut self, ui: &mut egui::Ui) {
        let mut trace = self.trace;
        theme::card(
            ui,
            None,
            |ui| {
                ui.label(theme::legend("history"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.selectable_value(&mut trace, Trace::Cells, theme::legend("cells"));
                    ui.selectable_value(&mut trace, Trace::Pack, theme::legend("pack"));
                });
            },
            |ui| match self.trace {
                Trace::Pack => self.pack_plot(ui),
                Trace::Cells => self.cell_plot(ui),
            },
        );
        self.trace = trace;
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
                ui.add_enabled_ui(!busy, |ui| {
                    if ui.button(theme::value("Connect")).clicked() {
                        reconnect = true;
                    }
                });
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
        let blind = self
            .last
            .as_ref()
            .and_then(|u| u.snapshot.as_ref())
            .map(|s| !s.has_cells())
            .unwrap_or(false);
        let mut apply = false;
        let capacity = self.profile.capacity_ah();
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
                egui::ComboBox::from_id_salt("chemistry")
                    .selected_text(self.profile.chemistry.label())
                    .width(ui.available_width().max(0.0))
                    .show_ui(ui, |ui| {
                        for c in Chemistry::ALL {
                            ui.selectable_value(&mut self.profile.chemistry, c, c.label());
                        }
                    });
                let mut series = self.profile.series as f64;
                let mut parallel = self.profile.parallel as f64;
                if blind || !self.series_detected {
                    field(ui, "series", &mut series, 1.0..=64.0, 1.0, 0);
                    self.profile.series = series as u16;
                } else {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("{}S", self.profile.series))
                                .size(theme::VALUE_SIZE)
                                .color(theme::VALUE),
                        );
                        ui.label(theme::legend("from the bms"));
                    });
                }
                field(ui, "parallel", &mut parallel, 1.0..=32.0, 1.0, 0);
                self.profile.parallel = parallel as u16;
                field(ui, "cell Ah", &mut self.profile.cell_ah, 0.5..=1000.0, 1.0, 1);
                theme::note(
                    ui,
                    format!(
                        "{:.2} V charge, {:.2} V float, {:.2} V floor",
                        self.profile.charge_v(),
                        self.profile.float_v(),
                        self.profile.floor_v()
                    ),
                    theme::LEGEND,
                );
                if ui.button(theme::value("Apply to limits")).clicked() {
                    apply = true;
                }
            },
        );
        if apply {
            self.apply_profile();
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
                egui::ComboBox::from_id_salt("mode")
                    .selected_text(match self.mode {
                        Mode::Auto => "Auto (bulk then float)",
                        Mode::Bulk => "Bulk",
                        Mode::TopBalance => "Top balance",
                        Mode::Unbalanced => "Unbalanced",
                    })
                    .width(ui.available_width().max(0.0))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.mode,
                            Mode::Auto,
                            "Auto (bulk then float)",
                        );
                        ui.selectable_value(&mut self.mode, Mode::Bulk, "Bulk");
                        ui.selectable_value(&mut self.mode, Mode::TopBalance, "Top balance");
                        ui.selectable_value(&mut self.mode, Mode::Unbalanced, "Unbalanced");
                    });
                theme::note(
                    ui,
                    match self.mode {
                        Mode::Auto => {
                            "Bulk to the cell ceiling, then float until the BMS reads 100%."
                        }
                        Mode::Bulk => "Taper at the ceiling, stop at floor current.",
                        Mode::TopBalance => "Bulk, then hold at floor current for the balancers.",
                        Mode::Unbalanced => "Stop the instant any cell touches the ceiling.",
                    },
                    theme::LEGEND,
                );
                ui.add_space(4.0);
                field(ui, "max A", &mut self.max_current, 0.2..=10.0, 0.1, 2);
                field(ui, "CV V", &mut self.cv, 1.0..=150.0, 0.1, 2);
                if self.mode == Mode::Auto {
                    field(ui, "float V", &mut self.float_v, 1.0..=150.0, 0.1, 2);
                }
                if self.blind() {
                    theme::note(
                        ui,
                        "No BMS: the CV setpoint is the ceiling and float is where it holds.",
                        theme::LEGEND,
                    );
                } else {
                    let mut ceiling = self.ceiling_mv as f64;
                    field(ui, "ceiling mV", &mut ceiling, 2000.0..=4300.0, 5.0, 0);
                    self.ceiling_mv = ceiling as u16;
                }
                soc_stop(ui, "stop at soc", &mut self.charge_to_soc, &mut self.target_soc);
                if self.mode == Mode::TopBalance {
                    let mut target = self.target_mv as f64;
                    field(ui, "target mV", &mut target, 2000.0..=4300.0, 5.0, 0);
                    self.target_mv = target as u16;
                    field(ui, "hold h", &mut self.hold_hours, 1.0..=72.0, 1.0, 0);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(theme::value("Charge")).clicked() {
                        self.send(Command::Start(self.plan(false)));
                    }
                    if ui
                        .button(RichText::new("Stop").size(theme::VALUE_SIZE).color(theme::FAULT))
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
                let modes: Vec<LoadMode> = last
                    .as_ref()
                    .map(|u| u.load_modes.clone())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| vec![LoadMode::Cc]);
                egui::ComboBox::from_id_salt("load_mode")
                    .selected_text(self.discharge_mode.label())
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for m in modes {
                            ui.selectable_value(
                                &mut self.discharge_mode,
                                m,
                                format!("{} ({})", m.label(), m.unit()),
                            );
                        }
                    });
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
                ui.horizontal(|ui| {
                    if ui.button(theme::value("Discharge")).clicked() {
                        self.send(Command::Start(Plan::discharge(self.discharge_config())));
                    }
                    if ui
                        .button(
                            RichText::new("Stop")
                                .size(theme::VALUE_SIZE)
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
                if ui.button(theme::value("Run capacity test")).clicked() {
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

    fn pack_plot(&self, ui: &mut egui::Ui) {
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
        let (v_lo, v_hi) = if cells > 0 {
            let n = cells as f64;
            (
                n * self.floor_mv as f64 / 1000.0,
                (n * (self.ceiling_mv + 60) as f64 / 1000.0).max(self.cv + 0.5),
            )
        } else {
            (0.0, 60.0)
        };
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
            CHART_H,
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

    fn cell_plot(&self, ui: &mut egui::Ui) {
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
            CHART_H,
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
        let tint = if *on { theme::READOUT } else { theme::LEGEND };
        if ui
            .selectable_label(*on, RichText::new(label.to_uppercase()).size(10.5).color(tint))
            .clicked()
        {
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

fn cells_table(ui: &mut egui::Ui, s: &Snapshot, ceiling: u16) {
    let (lo, hi) = (s.low_mv(), s.high_mv());
    let width = s.cells_mv.len().saturating_sub(1).to_string().len();
    egui::Grid::new("cell_table")
        .spacing(egui::vec2(8.0, 3.0))
        .show(ui, |ui| {
            for (i, mv) in s.cells_mv.iter().enumerate() {
                ui.label(
                    RichText::new(format!("c{i:0width$}"))
                        .font(theme::mono(11.0))
                        .color(theme::LEGEND),
                );
                let tint = if *mv >= ceiling {
                    theme::FAULT
                } else if *mv >= ceiling.saturating_sub(50) {
                    theme::READOUT
                } else {
                    theme::VALUE
                };
                ui.label(
                    RichText::new(format!("{mv}"))
                        .font(theme::mono(12.0))
                        .color(tint),
                );
                ui.label(
                    RichText::new(format!("+{:<3}", mv - lo))
                        .font(theme::mono(11.0))
                        .color(theme::LEGEND),
                );
                let frac = if hi > lo {
                    (*mv - lo) as f32 / (hi - lo) as f32
                } else {
                    0.04
                };
                theme::bar(ui, frac, tint, 120.0);
                ui.label(if s.balancing.contains(&i) {
                    RichText::new("bal").size(10.5).color(theme::OK)
                } else {
                    RichText::new("")
                });
                ui.end_row();
            }
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
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = match signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("signal handler: {e}");
            return;
        }
    };
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            let tx = SHUTDOWN.lock().ok().and_then(|g| g.clone());
            if let Some(tx) = tx {
                let _ = tx.send(Command::Quit);
                // The worker stops the hardware and exits; give it long enough
                // for a serial round trip before killing the process.
                std::thread::sleep(Duration::from_millis(1500));
            }
            std::process::exit(0);
        }
    });
}

fn main() -> eframe::Result<()> {
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
            Ok(Box::new(App::new(log)))
        }),
    )
}
