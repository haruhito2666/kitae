//! RP-C2 chart kit integration tests — geometry, signal-driven redraws,
//! degenerate inputs, and a churn property run.

use kitae_ui::charts::{AreaChart, BarChart, LineChart, Sparkline};
use kitae_ui::prelude::*;
use kitae_ui::testing::Node;

#[component]
fn LineDemo() -> View {
    let data = use_signal(|| vec![(0.0, 0.0), (1.0, 10.0), (2.0, 5.0)]);
    view! {
        div { LineChart(data, 400.0, 200.0) }
    }
}

#[component]
fn LineOf(data: Signal<Vec<(f64, f64)>>) -> View {
    view! {
        div { LineChart(data, 400.0, 200.0) }
    }
}

#[component]
fn AreaDemo() -> View {
    let data = use_signal(|| vec![(0.0, 2.0), (1.0, 6.0), (2.0, 4.0)]);
    view! {
        div { AreaChart(data, 300.0, 150.0) }
    }
}

#[component]
fn Bars() -> View {
    let vals = use_signal(|| {
        vec![
            ("a".to_string(), 5.0),
            ("b".to_string(), -3.0),
            ("c".to_string(), 10.0),
        ]
    });
    view! {
        div { BarChart(vals, 300.0, 150.0) }
    }
}

#[component]
fn Spark() -> View {
    let vals = use_signal(|| vec![3.0, 8.0, 2.0, 9.0, 5.0]);
    view! {
        div { Sparkline(vals, 120.0, 32.0) }
    }
}

#[component]
fn Empties() -> View {
    let l = use_signal(|| Vec::<(f64, f64)>::new());
    let a = use_signal(|| Vec::<(f64, f64)>::new());
    let b = use_signal(|| Vec::<(String, f64)>::new());
    let s = use_signal(|| Vec::<f64>::new());
    view! {
        div {
            LineChart(l, 100.0, 80.0)
            AreaChart(a, 100.0, 80.0)
            BarChart(b, 100.0, 80.0)
            Sparkline(s, 100.0, 80.0)
        }
    }
}

#[component]
fn Singles() -> View {
    let l = use_signal(|| vec![(7.0, 42.0)]);
    let a = use_signal(|| vec![(7.0, 42.0)]);
    let b = use_signal(|| vec![("x".to_string(), 42.0)]);
    let s = use_signal(|| vec![42.0]);
    view! {
        div {
            LineChart(l, 100.0, 80.0)
            AreaChart(a, 100.0, 80.0)
            BarChart(b, 100.0, 80.0)
            Sparkline(s, 100.0, 80.0)
        }
    }
}

#[test]
fn line_chart_renders_geometry_and_labels() {
    let app = TestApp::new(|| LineDemo());
    assert_eq!(app.query("svg").len(), 1);
    let polys = app.query("polyline");
    let poly = &polys[0];
    let points = poly.attr("points").unwrap();
    assert_eq!(points.split_whitespace().count(), 3, "one point per sample");
    for pair in points.split_whitespace() {
        for v in pair.split(',') {
            let v: f64 = v.parse().expect("finite coordinate");
            assert!(v.is_finite());
        }
    }
    // y labels at min/mid/max (0, 5, 10) + x labels at first/last (0, 2)
    let texts: Vec<String> = app.query("text").iter().map(|t| t.text()).collect();
    assert_eq!(texts.len(), 5);
    for label in ["0", "5", "10", "2"] {
        assert!(
            texts.iter().any(|t| t == label),
            "missing {label} in {texts:?}"
        );
    }
    assert!(poly.attr("stroke").is_some());
    assert_eq!(poly.attr("fill"), Some("none"));
}

#[test]
fn line_chart_redraws_when_signal_changes() {
    let data = use_signal(|| vec![(0.0, 1.0), (1.0, 2.0)]);
    let mut app = TestApp::new(move || LineOf(data));
    let polys = app.query("polyline");
    let before = polys[0].attr("points").unwrap().to_string();
    assert!(
        app.text().contains("1.5"),
        "mid gridline label from extent 1..2"
    );
    // Note: an extent-only change (1..2 → 5..9) redraws identical normalized
    // geometry by design — auto-scaling. So change the *shape* too.
    data.set(vec![(0.0, 5.0), (1.0, 9.0), (2.0, 6.0)]);
    app.refresh();
    let polys = app.query("polyline");
    let after = polys[0].attr("points").unwrap().to_string();
    assert_ne!(before, after, "the polyline follows the signal");
    assert_eq!(after.split_whitespace().count(), 3, "one point per sample");
    assert!(
        app.text().contains("7"),
        "mid gridline label from extent 5..9"
    );
}

#[test]
fn area_chart_closes_path_over_zero_base() {
    let app = TestApp::new(|| AreaDemo());
    let paths = app.query("path");
    let path = &paths[0];
    let d = path.attr("d").unwrap();
    assert!(d.starts_with("M "), "path starts with a moveto");
    assert!(d.trim_end().ends_with("Z"), "the fill polygon closes");
    assert!(
        path.attr("fill").unwrap().starts_with("rgba"),
        "translucent fill, not a solid block"
    );
    assert_eq!(
        app.query("polyline").len(),
        1,
        "the stroke line rides on top"
    );
    assert!(
        app.text().contains("3"),
        "mid gridline label from zero-based extent 0..6"
    );
    assert!(app.text().contains("0"), "the axis starts at zero");
}

#[test]
fn bar_chart_scales_bars_and_labels_negatives() {
    let app = TestApp::new(|| Bars());
    let rects = app.query("rect");
    assert_eq!(rects.len(), 3);
    let h = |r: &Node| r.attr("height").unwrap().parse::<f64>().unwrap();
    let (ha, hb, hc) = (h(&rects[0]), h(&rects[1]), h(&rects[2]));
    // Coordinates round to 1 decimal, so ratios hold only to ~0.01.
    assert!(
        (hc / ha - 2.0).abs() < 0.01,
        "10 vs 5 over the shared -3..10 range"
    );
    assert!(
        (hb / ha - 0.6).abs() < 0.01,
        "|-3| vs 5 over the shared range"
    );
    assert_eq!(
        app.query("line").len(),
        1,
        "a zero line exists for negative bars"
    );
    let text = app.text();
    for label in ["a", "b", "c"] {
        assert!(text.contains(label), "missing bar label {label}");
    }
}

#[test]
fn sparkline_shows_shape_and_last_point() {
    let app = TestApp::new(|| Spark());
    let svgs = app.query("svg");
    let svg = &svgs[0];
    assert!(svg.attr("class").unwrap().contains("sparkline"));
    let polys = app.query("polyline");
    let points = polys[0].attr("points").unwrap();
    assert_eq!(points.split_whitespace().count(), 5);
    assert_eq!(app.query("circle").len(), 1, "last-sample dot");
    assert!(app.query("text").is_empty(), "sparkline has no axis chrome");
}

#[test]
fn degenerate_data_never_produces_nan() {
    {
        let app = TestApp::new(|| Empties());
        assert_eq!(app.query("svg").len(), 4);
        assert!(!app.html().contains("NaN"));
    }
    kitae_ui::testing::reset();
    let app = TestApp::new(|| Singles());
    assert_eq!(app.query("svg").len(), 4);
    let html = app.html();
    assert!(
        !html.contains("NaN"),
        "single-point extents must clamp, not divide by zero: {html}"
    );
    assert_eq!(
        app.query("polyline").len(),
        1,
        "the line draws its single point; the sparkline needs two"
    );
}

#[test]
fn charts_stay_well_formed_under_churn() {
    let data = use_signal(|| vec![(0.0, 0.0), (1.0, 1.0)]);
    let mut app = TestApp::new(move || LineOf(data));
    let mut seed: u64 = 0x5EED;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..60u32 {
        let n = (rnd() % 9) as usize;
        let pts: Vec<(f64, f64)> = (0..n)
            .map(|i| {
                let y = ((rnd() % 200) as i64 - 100) as f64 / 10.0;
                (i as f64, y)
            })
            .collect();
        data.set(pts);
        app.refresh();
        let html = app.html();
        assert!(!html.contains("NaN"), "round {round}: {html}");
    }
}
