use egui::{Color32, FontId, Pos2, Rect, RichText, Stroke, Ui};

/// Deepest surface, the case itself.
pub const CHASSIS: Color32 = Color32::from_rgb(0x17, 0x19, 0x1D);
/// Raised control panels.
pub const PANEL: Color32 = Color32::from_rgb(0x21, 0x24, 0x2A);
/// Recessed wells: card headers, plot grounds.
pub const WELL: Color32 = Color32::from_rgb(0x14, 0x16, 0x19);
/// Engraved rules and borders.
pub const ETCH: Color32 = Color32::from_rgb(0x33, 0x38, 0x41);
/// Silkscreened label text.
pub const LEGEND: Color32 = Color32::from_rgb(0x8B, 0x92, 0x9C);
/// Brighter legend, for values.
pub const VALUE: Color32 = Color32::from_rgb(0xD5, 0xDB, 0xE3);
/// Amber: what you set.
pub const READOUT: Color32 = Color32::from_rgb(0xF5, 0xA6, 0x3B);
/// Cyan: what the instrument measured.
pub const TRACE: Color32 = Color32::from_rgb(0x5C, 0xD0, 0xE8);
/// Fault state.
pub const FAULT: Color32 = Color32::from_rgb(0xE2, 0x6D, 0x5A);
/// Doing its job.
pub const OK: Color32 = Color32::from_rgb(0x5C, 0xB0, 0x7A);

pub const LEGEND_SIZE: f32 = 11.5;
pub const VALUE_SIZE: f32 = 13.0;
const RAIL_W: f32 = 3.0;

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = CHASSIS;
    v.window_fill = CHASSIS;
    v.extreme_bg_color = WELL;
    v.faint_bg_color = PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, ETCH);
    v.widgets.inactive.bg_fill = PANEL;
    v.override_text_color = Some(VALUE);
    ctx.set_visuals(v);
    // Readings and plot ticks must never break mid-number, so nothing wraps
    // by default; prose asks for wrapping where it needs it.
    let mut style = (*ctx.global_style()).clone();
    style.wrap_mode = Some(egui::TextWrapMode::Extend);
    ctx.set_global_style(style);
}

/// A silkscreened caption: small, uppercase, widely tracked.
pub fn legend(text: impl Into<String>) -> RichText {
    RichText::new(text.into().to_uppercase())
        .size(LEGEND_SIZE)
        .extra_letter_spacing(1.6)
        .color(LEGEND)
}

pub fn value(text: impl Into<String>) -> RichText {
    RichText::new(text).size(VALUE_SIZE).color(VALUE)
}

/// Width a single reading is given. Fixed, so three of them in a card line up
/// with three in the card beside it, and a long value clips rather than
/// shoving its neighbour sideways.
pub const READOUT_W: f32 = 104.0;

/// A captioned reading: the legend above, the number under it.
pub fn readout(ui: &mut Ui, label: &str, v: String, tint: Color32) {
    ui.allocate_ui_with_layout(
        egui::vec2(READOUT_W, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(legend(label));
            ui.label(RichText::new(v).size(17.0).strong().color(tint));
        },
    );
}

/// A row of readings, wrapped so a narrow card stacks them instead of letting
/// them overrun each other.
pub fn readouts(ui: &mut Ui, items: &[(&str, String, Color32)]) {
    const GAP: f32 = 18.0;
    let per_row = ((ui.available_width() / (READOUT_W + GAP)).floor() as usize).max(1);
    for chunk in items.chunks(per_row) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            for (label, v, tint) in chunk {
                readout(ui, label, v.clone(), *tint);
            }
        });
        ui.add_space(4.0);
    }
}

/// A block of related lines: a recessed header on its own ground, the body
/// under it, and a rail down the left carrying the card's state.
pub fn card<R>(
    ui: &mut Ui,
    rail: Option<Color32>,
    header: impl FnOnce(&mut Ui),
    body: impl FnOnce(&mut Ui) -> R,
) -> egui::InnerResponse<R> {
    let outer = egui::Frame::NONE
        .fill(PANEL)
        .stroke(Stroke::new(1.0, ETCH))
        .corner_radius(2);
    let framed = outer.show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let head = egui::Frame::NONE.fill(WELL).inner_margin(egui::Margin {
            left: 10,
            right: 10,
            top: 4,
            bottom: 4,
        });
        let h = head.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| header(ui));
        });
        let r = h.response.rect;
        ui.painter().line_segment(
            [
                Pos2::new(r.left(), r.bottom()),
                Pos2::new(r.right(), r.bottom()),
            ],
            Stroke::new(1.0, ETCH),
        );
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 10,
                right: 10,
                top: 6,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.set_width(ui.available_width());
                body(ui)
            })
            .inner
    });
    if let Some(c) = rail {
        let r = framed.response.rect;
        ui.painter().rect_filled(
            Rect::from_min_max(r.left_top(), Pos2::new(r.left() + RAIL_W, r.bottom())),
            0.0,
            c,
        );
    }
    framed
}

/// One cell row painted as a bar in its well, so fifteen of them read as a
/// column of levels rather than fifteen progress widgets.
pub fn bar(ui: &mut Ui, frac: f32, tint: Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 9.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 1.0, WELL);
    let mut fill = rect.shrink(1.0);
    fill.set_width((fill.width() * frac.clamp(0.0, 1.0)).max(1.0));
    p.rect_filled(fill, 1.0, tint);
    p.rect_stroke(
        rect,
        1.0,
        Stroke::new(1.0, ETCH),
        egui::StrokeKind::Inside,
    );
}

/// A state lamp drawn as a pressed key: recessed and grey when off, lit and
/// outlined when on. One shape for every output in the rig, so "is it
/// delivering" reads the same on each card.
pub fn lamp(ui: &mut Ui, text: &str, on: bool, fault: bool) {
    let tint = if fault {
        FAULT
    } else if on {
        OK
    } else {
        LEGEND
    };
    let font = egui::FontId::proportional(LEGEND_SIZE);
    let galley = ui.painter().layout_no_wrap(
        text.to_uppercase(),
        font,
        tint,
    );
    let size = galley.size() + egui::vec2(14.0, 6.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(
        rect,
        2.0,
        if on {
            tint.gamma_multiply(0.18)
        } else {
            WELL
        },
    );
    p.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, if on { tint } else { ETCH }),
        egui::StrokeKind::Inside,
    );
    p.galley(
        rect.center() - galley.size() / 2.0,
        galley,
        tint,
    );
}

/// A lamp you can press: the same key shape, so a setting that is on or off
/// reads as a switch rather than as a caption.
pub fn toggle(ui: &mut Ui, text: &str, on: bool) -> egui::Response {
    let font = egui::FontId::proportional(LEGEND_SIZE);
    let tint = if on { READOUT } else { LEGEND };
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_uppercase(), font, tint);
    let size = galley.size() + egui::vec2(26.0, 7.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let lit = on || response.hovered();
    let p = ui.painter();
    p.rect_filled(
        rect,
        2.0,
        if on {
            tint.gamma_multiply(0.18)
        } else if response.hovered() {
            ETCH.gamma_multiply(0.6)
        } else {
            WELL
        },
    );
    p.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, if lit { tint } else { ETCH }),
        egui::StrokeKind::Inside,
    );
    let box_side = 8.0;
    let tick = Rect::from_center_size(
        Pos2::new(rect.left() + 11.0, rect.center().y),
        egui::vec2(box_side, box_side),
    );
    p.rect_stroke(
        tick,
        1.0,
        Stroke::new(1.0, if lit { tint } else { LEGEND }),
        egui::StrokeKind::Inside,
    );
    if on {
        p.rect_filled(tick.shrink(2.0), 0.0, tint);
    }
    p.galley(
        Pos2::new(tick.right() + 6.0, rect.center().y - galley.size().y / 2.0),
        galley,
        tint,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    response
}

/// A wrapped caption, for the sentences in the side panel that are prose
/// rather than readings.
pub fn note(ui: &mut Ui, text: impl Into<String>, color: Color32) {
    let width = ui.available_width();
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
    ui.add_sized(
        [width, 0.0],
        egui::Label::new(RichText::new(text).size(11.0).color(color)).wrap(),
    );
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}
