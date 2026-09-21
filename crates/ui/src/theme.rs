use egui::{Color32, FontFamily, FontId, Pos2, Rect, RichText, Stroke, Ui};
use std::sync::Arc;

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
/// Ground the cell comb is drawn in.
pub const BAND: Color32 = Color32::from_rgb(0x2A, 0x2E, 0x36);

pub const LEGEND_SIZE: f32 = 12.0;
pub const VALUE_SIZE: f32 = 13.0;
const RAIL_W: f32 = 3.0;

fn legend_family() -> FontFamily {
    FontFamily::Name("legend".into())
}

pub fn legend_font(size: f32) -> FontId {
    FontId::new(size, legend_family())
}

/// A figure you read from across the bench: Plex Mono semibold, tabular, so a
/// changing digit never shifts the ones beside it.
pub fn figure(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("figure".into()))
}

pub fn fonts(ctx: &egui::Context) {
    let mut f = egui::FontDefinitions::default();
    for (name, bytes) in [
        (
            "plex-mono",
            &include_bytes!("../assets/IBMPlexMono-Regular.ttf")[..],
        ),
        (
            "plex-mono-semibold",
            &include_bytes!("../assets/IBMPlexMono-SemiBold.ttf")[..],
        ),
        (
            "plex-condensed",
            &include_bytes!("../assets/IBMPlexSansCondensed-Regular.ttf")[..],
        ),
        (
            "plex-condensed-semibold",
            &include_bytes!("../assets/IBMPlexSansCondensed-SemiBold.ttf")[..],
        ),
    ] {
        f.font_data
            .insert(name.to_owned(), Arc::new(egui::FontData::from_static(bytes)));
    }
    f.families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "plex-condensed".into());
    f.families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "plex-mono".into());
    let fallback: Vec<String> = f
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for (family, face) in [
        ("legend", "plex-condensed-semibold"),
        ("figure", "plex-mono-semibold"),
    ] {
        let mut stack = vec![face.to_owned()];
        stack.extend(fallback.iter().cloned());
        f.families.insert(FontFamily::Name(family.into()), stack);
    }
    ctx.set_fonts(f);
}

pub fn apply(ctx: &egui::Context) {
    fonts(ctx);
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
        .font(legend_font(LEGEND_SIZE))
        .extra_letter_spacing(1.4)
        .color(LEGEND)
}

/// What a button says. Words, not a reading, so they are set in the panel
/// face rather than in the figures one.
pub fn action(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .font(legend_font(VALUE_SIZE + 0.5))
        .color(VALUE)
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
            ui.label(RichText::new(v).font(figure(17.0)).color(tint));
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
    let font = legend_font(LEGEND_SIZE);
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
    let font = legend_font(LEGEND_SIZE);
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
    ui.allocate_ui_with_layout(
        egui::vec2(width, 0.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.add(egui::Label::new(RichText::new(text).size(11.5).color(color)).wrap());
        },
    );
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

/// The one figure on the panel you are meant to read from across the bench:
/// the number large, its unit small beside it, its legend underneath.
pub fn hero(ui: &mut Ui, label: &str, v: &str, unit: &str, tint: Color32) {
    let figure_g = ui
        .painter()
        .layout_no_wrap(v.to_owned(), figure(42.0), tint);
    let unit_g = ui.painter().layout_no_wrap(
        unit.to_uppercase(),
        legend_font(14.0),
        tint.gamma_multiply(0.75),
    );
    let label_g = ui
        .painter()
        .layout_no_wrap(label.to_uppercase(), legend_font(LEGEND_SIZE), LEGEND);
    let w = figure_g.size().x + 5.0 + unit_g.size().x;
    let h = figure_g.size().y + 1.0 + label_g.size().y;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    let base = rect.top() + figure_g.size().y;
    let p = ui.painter();
    p.galley(rect.left_top(), figure_g.clone(), tint);
    p.galley(
        Pos2::new(
            rect.left() + figure_g.size().x + 5.0,
            base - unit_g.size().y - figure_g.size().y * 0.14,
        ),
        unit_g,
        tint,
    );
    p.galley(Pos2::new(rect.left(), base + 1.0), label_g, LEGEND);
}

/// Where a charge is in its sequence. The stages are a real order, so they
/// are drawn as one: the ones behind you dim, the one running lit, the ones
/// ahead outlined, and under it the condition that ends the one running.
pub fn stage_rail(
    ui: &mut Ui,
    stages: &[&str],
    at: Option<usize>,
    now: Color32,
    height: f32,
    muted: bool,
) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, height), egui::Sense::hover());
    let p = ui.painter();
    let n = stages.len().max(1);
    let gap = 5.0;
    let seg_w = ((rect.width() - gap * (n as f32 - 1.0)) / n as f32).max(8.0);
    for (i, name) in stages.iter().enumerate() {
        let x = rect.left() + i as f32 * (seg_w + gap);
        let seg = Rect::from_min_size(Pos2::new(x, rect.top()), egui::vec2(seg_w, rect.height()));
        let (fill, edge, ink) = match at {
            Some(a) if i == a => (
                if muted {
                    WELL
                } else {
                    now.gamma_multiply(0.20)
                },
                now,
                now,
            ),
            Some(a) if i < a => (WELL, ETCH, OK.gamma_multiply(0.85)),
            _ => (WELL, ETCH, LEGEND.gamma_multiply(0.65)),
        };
        p.rect_filled(seg, 2.0, fill);
        p.rect_stroke(seg, 2.0, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
        let size = if height < 22.0 {
            LEGEND_SIZE - 1.5
        } else {
            LEGEND_SIZE
        };
        let galley = p.layout_no_wrap(name.to_uppercase(), legend_font(size), ink);
        p.galley(seg.center() - galley.size() / 2.0, galley, ink);
    }
}

/// What ends the stage that is running, said in words, so it is not a thing
/// you have to remember from the settings panel.
pub fn exit_note(ui: &mut Ui, text: &str, tint: Color32) {
    if text.is_empty() {
        return;
    }
    ui.add_space(3.0);
    ui.label(RichText::new(text).font(legend_font(12.0)).color(tint));
}

pub struct Cell {
    pub mv: u16,
    pub balancing: bool,
}

/// The cell comb: every cell drawn as a level in the millivolt window the
/// pack is actually working in, with the commanded limits ruled across it in
/// amber. The two cells that govern the run, the highest and the lowest, are
/// the only ones that carry a number, because they are the only two any
/// decision is made on.
pub fn comb(
    ui: &mut Ui,
    cells: &[Cell],
    shown_mv: &[f32],
    ceiling_mv: u16,
    target_mv: u16,
    floor_mv: u16,
    height: f32,
) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, height), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 2.0, WELL);
    if cells.is_empty() {
        let g = p.layout_no_wrap(
            "NO CELLS REPORTED".into(),
            legend_font(LEGEND_SIZE),
            LEGEND,
        );
        p.galley(rect.center() - g.size() / 2.0, g, LEGEND);
        return;
    }

    const GUTTER: f32 = 58.0;
    const FOOT: f32 = 28.0;
    const HEAD: f32 = 16.0;
    let plot = Rect::from_min_max(
        Pos2::new(rect.left() + 8.0, rect.top() + 8.0 + HEAD),
        Pos2::new(rect.right() - GUTTER, rect.bottom() - 6.0 - FOOT),
    );
    if plot.width() < 20.0 || plot.height() < 20.0 {
        return;
    }

    let lo = cells.iter().map(|c| c.mv).min().unwrap_or(0);
    let hi = cells.iter().map(|c| c.mv).max().unwrap_or(0);
    let spread = hi.saturating_sub(lo);
    // Always keep the commanded ceiling in frame, because the headroom to it
    // is the whole question; zoom the bottom to wherever the pack is, or 55
    // mV of spread on a 3.0 V window is a flat line.
    let pad = (spread as f32 * 0.6).max(25.0);
    let band_hi = (ceiling_mv as f32 + pad * 0.5).max(hi as f32 + pad * 0.5);
    let band_lo = (lo as f32 - pad).min(band_hi - 60.0);
    let y_of = |mv: f32| plot.bottom() - (mv - band_lo) / (band_hi - band_lo) * plot.height();

    // Every rule across the comb is a figure you set, so every rule is amber.
    // The bars are cyan because the pack decided them. Reading the gap is the
    // whole job of the panel.
    for (mv, name, weight) in [
        (ceiling_mv, "ceiling", 0.85),
        (target_mv, "target", 0.5),
        (floor_mv, "floor", 0.5),
    ] {
        let y = y_of(mv as f32);
        if !(plot.top() - 1.0..=plot.bottom() + 1.0).contains(&y) {
            continue;
        }
        p.add(egui::Shape::dashed_line(
            &[Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(1.0, READOUT.gamma_multiply(weight * 0.7)),
            5.0,
            4.0,
        ));
        let g = p.layout_no_wrap(format!("{mv}"), figure(11.0), READOUT.gamma_multiply(weight));
        p.galley(Pos2::new(plot.right() + 6.0, y - g.size().y / 2.0), g, READOUT);
        let g = p.layout_no_wrap(
            name.to_uppercase(),
            legend_font(9.5),
            LEGEND.gamma_multiply(0.8),
        );
        p.galley(Pos2::new(plot.right() + 6.0, y + 5.0), g, LEGEND);
    }

    let n = cells.len();
    let col = plot.width() / n as f32;
    let tooth_w = (col - 6.0).clamp(2.0, 42.0);
    let label_cells = col >= 18.0;
    let label_mv = col >= 34.0;
    let width = n.saturating_sub(1).to_string().len();
    for (i, c) in cells.iter().enumerate() {
        let mv = shown_mv.get(i).copied().unwrap_or(c.mv as f32);
        let cx = plot.left() + col * (i as f32 + 0.5);
        let y = y_of(mv).clamp(plot.top() + 1.0, plot.bottom());
        let governs = c.mv == hi || c.mv == lo;
        let tint = if c.mv >= ceiling_mv { FAULT } else { TRACE };
        let half = tooth_w / 2.0;
        p.line_segment(
            [
                Pos2::new(cx, plot.top()),
                Pos2::new(cx, plot.bottom()),
            ],
            Stroke::new(1.0, BAND.gamma_multiply(0.55)),
        );
        // A stem for weight and a tooth at the level. The level is the
        // reading; a bar filled from an arbitrary floor would put the eye on
        // the area instead, and the area means nothing.
        p.rect_filled(
            Rect::from_min_max(
                Pos2::new(cx - half, y),
                Pos2::new(cx + half, plot.bottom()),
            ),
            0.0,
            tint.gamma_multiply(if governs { 0.16 } else { 0.09 }),
        );
        p.rect_filled(
            Rect::from_min_max(
                Pos2::new(cx - half, y),
                Pos2::new(cx + half, y + if governs { 4.0 } else { 3.0 }),
            ),
            0.0,
            tint.gamma_multiply(if governs { 1.0 } else { 0.62 }),
        );
        if c.balancing {
            p.rect_filled(
                Rect::from_center_size(Pos2::new(cx, y - 6.0), egui::vec2(4.0, 4.0)),
                0.0,
                READOUT,
            );
        }
        if governs && n > 1 {
            let tag = if c.mv == hi { "HIGH" } else { "LOW" };
            let g = p.layout_no_wrap(format!("{tag} {}", c.mv), legend_font(10.5), tint);
            let x = (cx - g.size().x / 2.0).clamp(plot.left(), plot.right() - g.size().x);
            p.galley(Pos2::new(x, (y - 18.0).max(rect.top() + 2.0)), g, tint);
        }
        if label_cells || governs {
            let ink = if governs { tint } else { LEGEND.gamma_multiply(0.75) };
            let g = p.layout_no_wrap(format!("c{i:0width$}"), mono(9.5), ink);
            p.galley(Pos2::new(cx - g.size().x / 2.0, plot.bottom() + 3.0), g, ink);
            if label_mv {
                let g = p.layout_no_wrap(
                    format!("{}", c.mv),
                    figure(10.0),
                    if governs { tint } else { VALUE.gamma_multiply(0.8) },
                );
                p.galley(
                    Pos2::new(cx - g.size().x / 2.0, plot.bottom() + 14.0),
                    g,
                    VALUE,
                );
            }
        }
    }
}
