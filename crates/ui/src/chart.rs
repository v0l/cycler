use crate::theme;
use egui::{Align2, Color32, Pos2, Rect, Stroke, Ui, Vec2};

/// One trace: a name for the key, a colour, and points already in the units
/// of its own axis.
pub struct Trace<'a> {
    pub name: &'a str,
    pub color: Color32,
    pub points: Vec<[f64; 2]>,
    /// Which axis the values belong to.
    pub right: bool,
}

pub struct Axis {
    pub label: &'static str,
    pub lo: f64,
    pub hi: f64,
    pub decimals: usize,
}

impl Axis {
    fn span(&self) -> f64 {
        if (self.hi - self.lo).abs() < f64::EPSILON {
            1.0
        } else {
            self.hi - self.lo
        }
    }
}

const PAD_L: f32 = 46.0;
const PAD_R: f32 = 46.0;
const PAD_T: f32 = 8.0;
const PAD_B: f32 = 18.0;
const TICKS: usize = 4;

/// A strip chart with its own axes. egui_plot fought every attempt to pin the
/// y range and label it in two units, and a chart this simple is less code
/// drawn directly than configured.
pub fn strip(ui: &mut Ui, height: f32, left: &Axis, right: Option<&Axis>, traces: &[Trace]) {
    let width = ui.available_width().max(80.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let plot = Rect::from_min_max(
        Pos2::new(rect.left() + PAD_L, rect.top() + PAD_T),
        Pos2::new(rect.right() - PAD_R, rect.bottom() - PAD_B),
    );
    if plot.width() < 8.0 || plot.height() < 8.0 {
        return;
    }
    p.rect_filled(plot, 1.0, theme::WELL);

    let (x_lo, x_hi) = traces
        .iter()
        .flat_map(|t| t.points.iter())
        .fold((f64::MAX, f64::MIN), |(lo, hi), pt| {
            (lo.min(pt[0]), hi.max(pt[0]))
        });
    let (x_lo, x_hi) = if x_lo > x_hi {
        (0.0, 1.0)
    } else if (x_hi - x_lo) < 1e-9 {
        (x_lo, x_lo + 1.0)
    } else {
        (x_lo, x_hi)
    };

    let sx = |x: f64| plot.left() + ((x - x_lo) / (x_hi - x_lo)) as f32 * plot.width();
    let sy = |v: f64, a: &Axis| plot.bottom() - ((v - a.lo) / a.span()) as f32 * plot.height();

    let font = egui::FontId::monospace(10.0);
    for i in 0..=TICKS {
        let f = i as f64 / TICKS as f64;
        let y = plot.bottom() - f as f32 * plot.height();
        p.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(1.0, theme::ETCH),
        );
        p.text(
            Pos2::new(plot.left() - 4.0, y),
            Align2::RIGHT_CENTER,
            format!("{:.*}", left.decimals, left.lo + left.span() * f),
            font.clone(),
            theme::LEGEND,
        );
        if let Some(r) = right {
            p.text(
                Pos2::new(plot.right() + 4.0, y),
                Align2::LEFT_CENTER,
                format!("{:.*}", r.decimals, r.lo + r.span() * f),
                font.clone(),
                theme::READOUT,
            );
        }
    }

    // Time axis: a few marks, labelled in minutes since the session began.
    for i in 0..=TICKS {
        let f = i as f64 / TICKS as f64;
        let x = plot.left() + f as f32 * plot.width();
        p.line_segment(
            [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
            Stroke::new(1.0, theme::ETCH.gamma_multiply(0.6)),
        );
        p.text(
            Pos2::new(x, plot.bottom() + 3.0),
            Align2::CENTER_TOP,
            time_label(x_lo + (x_hi - x_lo) * f, x_hi - x_lo),
            font.clone(),
            theme::LEGEND,
        );
    }

    p.text(
        Pos2::new(rect.left() + 2.0, plot.top()),
        Align2::LEFT_TOP,
        left.label,
        font.clone(),
        theme::LEGEND,
    );
    if let Some(r) = right {
        p.text(
            Pos2::new(rect.right() - 2.0, plot.top()),
            Align2::RIGHT_TOP,
            r.label,
            font.clone(),
            theme::READOUT,
        );
    }

    for t in traces {
        let axis = if t.right { right.unwrap_or(left) } else { left };
        let pts: Vec<Pos2> = t
            .points
            .iter()
            .map(|pt| Pos2::new(sx(pt[0]), sy(pt[1], axis).clamp(plot.top(), plot.bottom())))
            .collect();
        if pts.len() > 1 {
            p.add(egui::Shape::line(pts, Stroke::new(1.4, t.color)));
        }
    }

    // Key, top left inside the well, so it never covers the newest samples on
    // the right.
    let mut y = plot.top() + 3.0;
    for t in traces {
        p.text(
            Pos2::new(plot.left() + 6.0, y),
            Align2::LEFT_TOP,
            t.name,
            font.clone(),
            t.color,
        );
        y += 12.0;
    }

    // Hovering reads out the sample under the cursor.
    if let Some(pos) = response.hover_pos()
        && plot.contains(pos)
    {
        let x = x_lo + ((pos.x - plot.left()) / plot.width()) as f64 * (x_hi - x_lo);
        p.line_segment(
            [
                Pos2::new(pos.x, plot.top()),
                Pos2::new(pos.x, plot.bottom()),
            ],
            Stroke::new(1.0, theme::LEGEND.gamma_multiply(0.7)),
        );
        let mut text = format!("{x:.1}m");
        for t in traces {
            if let Some(pt) = nearest(&t.points, x) {
                text.push_str(&format!("  {} {:.2}", t.name, pt));
            }
        }
        p.text(
            Pos2::new(plot.right() - 4.0, plot.bottom() - 4.0),
            Align2::RIGHT_BOTTOM,
            text,
            font,
            theme::VALUE,
        );
    }
}

/// Minutes, at whatever precision the visible span deserves: seconds at the
/// start of a run, whole minutes once it is hours long.
fn time_label(minutes: f64, span: f64) -> String {
    if span < 2.0 {
        format!("{:.0}s", minutes * 60.0)
    } else if span < 20.0 {
        format!("{minutes:.1}m")
    } else if span < 180.0 {
        format!("{minutes:.0}m")
    } else {
        format!("{:.1}h", minutes / 60.0)
    }
}

fn nearest(points: &[[f64; 2]], x: f64) -> Option<f64> {
    points
        .iter()
        .min_by(|a, b| {
            (a[0] - x)
                .abs()
                .partial_cmp(&(b[0] - x).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|p| p[1])
}
