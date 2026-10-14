//! The chart starter kit (UI_LIBRARY_PLAN RP-C2): [`LineChart`],
//! [`AreaChart`], [`BarChart`], [`Sparkline`] — minimal, dependency-free SVG
//! charts driven by signals. Each component reads its data signal once per
//! render; when the signal changes the host re-renders and the geometry
//! recomputes — that is the whole "driven by signals" story, nothing
//! imperative.
//!
//! Charts emit a plain `svg` subtree (viewBox + shapes), so any host that
//! renders kitae elements can paint them — the browser engine paints inline
//! SVG through its R6 replaced-element path.
//!
//! Starter scope, deliberately: y gridlines at min/mid/max with numeric
//! labels, x labels at the first/last point, one accent color, red bars for
//! negatives with a zero line. No hover tooltips (the engine dispatches only
//! `click`/`input`), no legends, no date axes. Empty or single-point data
//! renders an empty (or degenerate) chart — never `NaN` coordinates.
//!
//! ```
//! use kitae_ui::charts::Sparkline;
//! use kitae_ui::prelude::*;
//!
//! #[component]
//! fn Dash() -> View {
//!     let cpu = use_signal(|| vec![3.0, 7.0, 4.0, 9.0, 6.0]);
//!     view! {
//!         div {
//!             Sparkline(cpu, 120.0, 32.0)
//!             button(on:click = move |_| cpu.update(|v| v.push(8.0))) { "sample" }
//!         }
//!     }
//! }
//!
//! let mut app = TestApp::new(|| Dash());
//! let before = app.query("polyline")[0].attr("points").unwrap().to_string();
//! assert!(app.click_text("sample"));
//! let after = app.query("polyline")[0].attr("points").unwrap().to_string();
//! assert_ne!(before, after, "the sparkline redraws from the signal");
//! ```

use crate::component;
use crate::components::theme_class;
use crate::signals::Signal;
use crate::vnode::{VNode, View};

// ── palette & padding ───────────────────────────────────────────────────────

const ACCENT: &str = "#2563eb";
const ACCENT_FILL: &str = "rgba(37, 99, 235, 0.15)";
const NEGATIVE: &str = "#dc2626";
const GRID: &str = "#e5e7eb";
const AXIS: &str = "#6b7280";
const PAD_L: f64 = 34.0;
const PAD_R: f64 = 8.0;
const PAD_T: f64 = 8.0;
const PAD_B: f64 = 20.0;

// ── geometry ────────────────────────────────────────────────────────────────

/// Maps data extents to svg coordinates for the padded plot area. `None`
/// means "no drawable data" — callers render an empty svg instead of NaNs.
struct Scale {
    xmin: f64,
    xmax: f64,
    ymin: f64,
    ymax: f64,
    w: f64,
    h: f64,
}

impl Scale {
    /// `zero_base` pins the y extent to include 0 (honest bars/areas);
    /// line charts use the raw data extent instead.
    fn new(series: &[(f64, f64)], w: f64, h: f64, zero_base: bool) -> Option<Scale> {
        let (x0, y0) = *series.first()?;
        let mut s = Scale {
            xmin: x0,
            xmax: x0,
            ymin: y0,
            ymax: y0,
            w,
            h,
        };
        for (x, y) in series {
            s.xmin = s.xmin.min(*x);
            s.xmax = s.xmax.max(*x);
            s.ymin = s.ymin.min(*y);
            s.ymax = s.ymax.max(*y);
        }
        if zero_base {
            s.ymin = s.ymin.min(0.0);
            s.ymax = s.ymax.max(0.0);
        }
        // A degenerate extent would divide by zero; widen it by one unit so
        // the single point sits at the center of its axis instead.
        if s.xmin == s.xmax {
            s.xmin -= 1.0;
            s.xmax += 1.0;
        }
        if s.ymin == s.ymax {
            s.ymin -= 1.0;
            s.ymax += 1.0;
        }
        Some(s)
    }

    fn x(&self, v: f64) -> f64 {
        PAD_L + (v - self.xmin) / (self.xmax - self.xmin) * (self.w - PAD_L - PAD_R)
    }

    fn y(&self, v: f64) -> f64 {
        PAD_T + (1.0 - (v - self.ymin) / (self.ymax - self.ymin)) * (self.h - PAD_T - PAD_B)
    }
}

/// Integers print without a decimal point; everything else with one.
fn fmt_num(v: f64) -> String {
    let r = v.round();
    if (v - r).abs() < 1e-9 {
        format!("{}", r as i64)
    } else {
        format!("{v:.1}")
    }
}

fn fmt_coord(v: f64) -> String {
    format!("{v:.1}")
}

fn polyline(points: &str, color: &str) -> VNode {
    crate::__rt::elem("polyline")
        .attr("points", points)
        .attr("fill", "none")
        .attr("stroke", color)
        .attr("stroke-width", 2)
        .attr("stroke-linejoin", "round")
        .attr("stroke-linecap", "round")
        .build()
}

fn points_string(s: &Scale, pts: &[(f64, f64)]) -> String {
    pts.iter()
        .map(|(x, y)| format!("{},{}", fmt_coord(s.x(*x)), fmt_coord(s.y(*y))))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Three horizontal gridlines with min/mid/max y labels at the left edge.
fn grid_and_y_labels(s: &Scale) -> Vec<VNode> {
    let mut out = Vec::new();
    for i in 0..=2u32 {
        let v = s.ymin + (s.ymax - s.ymin) * f64::from(i) / 2.0;
        let y = fmt_coord(s.y(v));
        out.push(
            crate::__rt::elem("line")
                .attr("x1", PAD_L)
                .attr("x2", s.w - PAD_R)
                .attr("y1", &y)
                .attr("y2", &y)
                .attr("stroke", GRID)
                .attr("stroke-width", 1)
                .build(),
        );
        out.push(
            crate::__rt::elem("text")
                .attr("x", PAD_L - 4.0)
                .attr("y", fmt_coord(s.y(v) + 3.0))
                .attr("text-anchor", "end")
                .attr("font-size", 10)
                .attr("fill", AXIS)
                .children(vec![crate::__rt::text(fmt_num(v))])
                .build(),
        );
    }
    out
}

fn x_label(s: &Scale, v: f64, left: bool) -> VNode {
    crate::__rt::elem("text")
        .attr("x", if left { PAD_L } else { s.w - PAD_R })
        .attr("y", s.h - 4.0)
        .attr("text-anchor", if left { "start" } else { "end" })
        .attr("font-size", 10)
        .attr("fill", AXIS)
        .children(vec![crate::__rt::text(fmt_num(v))])
        .build()
}

fn svg_root(kind: &str, width: f64, height: f64, kids: Vec<VNode>) -> View {
    let theme = theme_class();
    let mut class = format!("kit-chart kit-chart-{kind}");
    if !theme.is_empty() {
        class.push(' ');
        class.push_str(&theme);
    }
    let svg = crate::__rt::elem("svg")
        .attr("class", class)
        .attr("width", width)
        .attr("height", height)
        .attr("viewBox", format!("0 0 {width} {height}"))
        .attr("role", "img")
        .children(kids)
        .build();
    View::from_nodes(vec![svg])
}

// ── the charts ──────────────────────────────────────────────────────────────

/// A stroked line through `(x, y)` pairs, with gridlines and min/mid/max
/// axis labels.
#[component]
pub fn LineChart(series: Signal<Vec<(f64, f64)>>, width: f64, height: f64) -> View {
    let pts = series.get();
    let mut kids: Vec<VNode> = Vec::new();
    if let Some(s) = Scale::new(&pts, width, height, false) {
        kids.extend(grid_and_y_labels(&s));
        kids.push(polyline(&points_string(&s, &pts), ACCENT));
        if let (Some((x0, _)), Some((xn, _))) = (pts.first(), pts.last()) {
            kids.push(x_label(&s, *x0, true));
            kids.push(x_label(&s, *xn, false));
        }
    }
    svg_root("line", width, height, kids)
}

/// A filled polygon under the line, pinned to a zero baseline (all-positive
/// data still starts its axis at 0 — an area chart that truncates its base
/// exaggerates trends).
#[component]
pub fn AreaChart(series: Signal<Vec<(f64, f64)>>, width: f64, height: f64) -> View {
    let pts = series.get();
    let mut kids: Vec<VNode> = Vec::new();
    if let Some(s) = Scale::new(&pts, width, height, true) {
        kids.extend(grid_and_y_labels(&s));
        if pts.len() >= 2 {
            let mut d = String::from("M ");
            for (i, (x, y)) in pts.iter().enumerate() {
                if i > 0 {
                    d.push_str("L ");
                }
                d.push_str(&fmt_coord(s.x(*x)));
                d.push(' ');
                d.push_str(&fmt_coord(s.y(*y)));
                d.push(' ');
            }
            let base = fmt_coord(s.y(0.0));
            d.push_str(&format!(
                "L {} {base} L {} {base} Z",
                fmt_coord(s.x(pts[pts.len() - 1].0)),
                fmt_coord(s.x(pts[0].0)),
            ));
            kids.push(
                crate::__rt::elem("path")
                    .attr("d", d.trim())
                    .attr("fill", ACCENT_FILL)
                    .build(),
            );
            kids.push(polyline(&points_string(&s, &pts), ACCENT));
        }
        if let (Some((x0, _)), Some((xn, _))) = (pts.first(), pts.last()) {
            kids.push(x_label(&s, *x0, true));
            kids.push(x_label(&s, *xn, false));
        }
    }
    svg_root("area", width, height, kids)
}

/// Category bars from `(label, value)` pairs. The y axis always includes 0;
/// negative bars are red and a zero line is drawn when any bar is negative.
#[component]
pub fn BarChart(bars: Signal<Vec<(String, f64)>>, width: f64, height: f64) -> View {
    let data = bars.get();
    let mut kids: Vec<VNode> = Vec::new();
    if !data.is_empty() {
        let mut ymin = 0.0f64;
        let mut ymax = 0.0f64;
        for (_, v) in &data {
            ymin = ymin.min(*v);
            ymax = ymax.max(*v);
        }
        if ymin == ymax {
            ymin -= 1.0;
            ymax += 1.0;
        }
        let inner_h = height - PAD_T - PAD_B;
        let slot = (width - PAD_L - PAD_R) / data.len() as f64;
        let y_of = |v: f64| PAD_T + (1.0 - (v - ymin) / (ymax - ymin)) * inner_h;
        if ymin < 0.0 {
            let y0 = fmt_coord(y_of(0.0));
            kids.push(
                crate::__rt::elem("line")
                    .attr("x1", PAD_L)
                    .attr("x2", width - PAD_R)
                    .attr("y1", &y0)
                    .attr("y2", &y0)
                    .attr("stroke", AXIS)
                    .attr("stroke-width", 1)
                    .build(),
            );
        }
        for (i, (label, v)) in data.iter().enumerate() {
            let top = y_of(*v);
            let base = y_of(0.0);
            kids.push(
                crate::__rt::elem("rect")
                    .attr("x", fmt_coord(PAD_L + i as f64 * slot + slot * 0.15))
                    .attr("y", fmt_coord(top.min(base)))
                    .attr("width", fmt_coord(slot * 0.7))
                    .attr("height", fmt_coord((base - top).abs()))
                    .attr("fill", if *v < 0.0 { NEGATIVE } else { ACCENT })
                    .attr("rx", 2)
                    .build(),
            );
            kids.push(
                crate::__rt::elem("text")
                    .attr("x", fmt_coord(PAD_L + (i as f64 + 0.5) * slot))
                    .attr("y", height - 4.0)
                    .attr("text-anchor", "middle")
                    .attr("font-size", 10)
                    .attr("fill", AXIS)
                    .children(vec![crate::__rt::text(label.clone())])
                    .build(),
            );
        }
    }
    svg_root("bar", width, height, kids)
}

/// A chrome-free line for inline metrics: evenly spaced values, no axes, a
/// dot on the last sample. Needs two values to draw anything.
#[component]
pub fn Sparkline(values: Signal<Vec<f64>>, width: f64, height: f64) -> View {
    let vs = values.get();
    let mut kids: Vec<VNode> = Vec::new();
    if vs.len() >= 2 {
        let mut ymin = vs[0];
        let mut ymax = vs[0];
        for v in &vs {
            ymin = ymin.min(*v);
            ymax = ymax.max(*v);
        }
        if ymin == ymax {
            ymin -= 1.0;
            ymax += 1.0;
        }
        let pad = 3.0;
        let inner_h = height - pad * 2.0;
        let n = (vs.len() - 1) as f64;
        let pts: Vec<String> = vs
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let x = i as f64 / n * width;
                let y = pad + (1.0 - (v - ymin) / (ymax - ymin)) * inner_h;
                format!("{},{}", fmt_coord(x), fmt_coord(y))
            })
            .collect();
        kids.push(polyline(&pts.join(" "), ACCENT));
        let (cx, cy) = pts.last().unwrap().split_once(',').unwrap();
        kids.push(
            crate::__rt::elem("circle")
                .attr("cx", cx)
                .attr("cy", cy)
                .attr("r", 2.5)
                .attr("fill", ACCENT)
                .build(),
        );
    }
    svg_root("sparkline", width, height, kids)
}
