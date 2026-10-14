//! The official component kit (UI_LIBRARY_PLAN RP-C1): Button, Modal,
//! Drawer, Tabs, Table, Toast, Tooltip, DatePicker, VirtualList — built only
//! on the Track A primitives, dogfooding them exactly the way user code will.
//!
//! Three documented engine-shape deviations, all consequences of kitae_ui
//! being synchronous and the engine dispatching only `click` and `input`:
//!
//! - **Tooltip toggles on click**, not hover (no hover events exist).
//! - **VirtualList and Table paginate** instead of scroll-virtualizing (no
//!   scroll events exist) — a page-sized window of rows plus a pager.
//! - **Toasts dismiss on click**; auto-dismiss timers need a timer hook and
//!   are not in v1.
//!
//! # A button that counts clicks
//!
//! ```
//! use kitae_ui::components::*;
//! use kitae_ui::prelude::*;
//! use std::rc::Rc;
//!
//! #[component]
//! fn Demo() -> View {
//!     let n = use_signal(|| 0);
//!     view! {
//!         div {
//!             Button("Save".into(), Rc::new(move |_| n.update(|v| *v += 1)), "primary".into(), false)
//!             p { "n={n}" }
//!         }
//!     }
//! }
//!
//! let mut app = TestApp::new(|| Demo());
//! assert!(app.click_text("Save"));
//! assert!(app.text().contains("n=1"));
//! ```

use crate::component;
use crate::signals::{use_signal, Signal};
use crate::view;
use crate::vnode::{Children, Event, Handler, VNode, View};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

// ── theming ─────────────────────────────────────────────────────────────────

/// Wrap a subtree with a theme name (`"light"`, `"dark"`, …). Every kit
/// component reads it through [`theme_class`] and appends the resulting
/// `kit-theme-<name>` token to its root class, so theme-scoped CSS (an app's
/// own stylesheet) can restyle anything inside.
#[component]
pub fn ThemeProvider(theme: String, children: Children) -> View {
    crate::hooks::provide_context(format!("kit-theme-{theme}"));
    view! {
        div(class: "kit-theme-root") { {children} }
    }
}

/// The active theme class (`kit-theme-<name>`), or `""` when no
/// [`ThemeProvider`] is above — components append it to their class list.
/// Non-panicking on purpose: an unthemed page is not an error.
pub fn theme_class() -> String {
    crate::hooks::try_use_context::<String>().unwrap_or_default()
}

// ── Button ──────────────────────────────────────────────────────────────────

/// A button. `variant` is `"primary"`, `"secondary"`, `"danger"` or
/// `"ghost"`. When `disabled`, the element carries the `disabled` attribute
/// (the engine treats it as presence-based) and **no click handler at all**,
/// so a disabled button cannot be activated through any path.
#[component]
pub fn Button(label: String, on_click: Handler, variant: String, disabled: bool) -> View {
    let theme = theme_class();
    let class = format!("kit-btn kit-btn-{variant} {theme}");
    let style = button_style(&variant, disabled);
    if disabled {
        view! {
            button(class: class, style: style, disabled: "true") { {label} }
        }
    } else {
        view! {
            button(class: class, style: style, on:click = move |e| on_click(e)) { {label} }
        }
    }
}

fn button_style(variant: &str, disabled: bool) -> String {
    let (bg, fg, border) = match variant {
        "primary" => ("#2f6fed", "#ffffff", "#2f6fed"),
        "danger" => ("#d64545", "#ffffff", "#d64545"),
        "ghost" => ("transparent", "#1f2933", "transparent"),
        _ => ("#ffffff", "#1f2933", "#c9d2dd"),
    };
    let opacity = if disabled { "0.55" } else { "1" };
    format!(
        "background: {bg}; color: {fg}; border: 1px solid {border}; border-radius: 6px; padding: 6px 14px; font-size: 14px; opacity: {opacity};"
    )
}

// ── Modal ───────────────────────────────────────────────────────────────────

/// A centered dialog over a dimmed backdrop. Renders nothing while `open` is
/// false. The backdrop and the × close it; the panel itself carries a
/// no-op click handler that *consumes* clicks — the engine bubbles a click
/// to the nearest ancestor with a handler, so without it every click inside
/// the dialog would close it.
#[component]
pub fn Modal(open: bool, title: String, on_close: Handler, children: Children) -> View {
    if !open {
        return View::default();
    }
    let close_x = on_close.clone();
    view! {
        div(class: "kit-modal-backdrop",
            style: "position: fixed; left: 0; top: 0; right: 0; bottom: 0; background: rgba(15, 23, 42, 0.45); z-index: 1000; display: flex; align-items: center; justify-content: center;",
            on:click = move |e| on_close(e)) {
            div(class: "kit-modal-panel",
                style: "background: #ffffff; border-radius: 10px; box-shadow: 0 18px 50px rgba(15, 23, 42, 0.35); width: 480px;",
                on:click = move |e| swallow(e)) {
                div(class: "kit-modal-head",
                    style: "display: flex; align-items: center; justify-content: space-between; padding: 14px 18px; border-bottom: 1px solid #e5eaf0;") {
                    strong(class: "kit-modal-title", style: "font-size: 16px;") { {title} }
                    button(class: "kit-modal-close",
                        style: "background: transparent; border: none; font-size: 18px; padding: 0 6px; color: #6b7280;",
                        on:click = move |e| close_x(e)) { "×" }
                }
                div(class: "kit-modal-body", style: "padding: 18px;") { {children} }
            }
        }
    }
}

/// The panel's bubble-swallowing click handler (see [`Modal`]).
fn swallow(_: Event) {}

// ── Drawer ──────────────────────────────────────────────────────────────────

/// A side panel over a dimmed backdrop; `side` is `"left"` or `"right"`.
/// Same close behavior as [`Modal`] (backdrop / ×, panel swallows clicks).
#[component]
pub fn Drawer(
    open: bool,
    side: String,
    title: String,
    on_close: Handler,
    children: Children,
) -> View {
    if !open {
        return View::default();
    }
    let close_x = on_close.clone();
    let anchor = if side == "right" {
        "right: 0;"
    } else {
        "left: 0;"
    };
    let panel_class = format!("kit-drawer kit-drawer-{side}");
    let panel_style = format!(
        "position: fixed; top: 0; bottom: 0; {anchor} width: 320px; background: #ffffff; z-index: 1001; box-shadow: 0 0 40px rgba(15, 23, 42, 0.25); display: flex; flex-direction: column;"
    );
    view! {
        div(class: "kit-drawer-backdrop",
            style: "position: fixed; left: 0; top: 0; right: 0; bottom: 0; background: rgba(15, 23, 42, 0.35); z-index: 1000;",
            on:click = move |e| on_close(e)) {
            div(class: panel_class, style: panel_style, on:click = move |e| swallow(e)) {
                div(class: "kit-drawer-head",
                    style: "display: flex; align-items: center; justify-content: space-between; padding: 14px 16px; border-bottom: 1px solid #e5eaf0;") {
                    strong(class: "kit-drawer-title", style: "font-size: 15px;") { {title} }
                    button(class: "kit-drawer-close",
                        style: "background: transparent; border: none; font-size: 18px; padding: 0 6px; color: #6b7280;",
                        on:click = move |e| close_x(e)) { "×" }
                }
                div(class: "kit-drawer-body", style: "padding: 16px;") { {children} }
            }
        }
    }
}

// ── Tabs ────────────────────────────────────────────────────────────────────

/// A tab strip plus panel. `tabs` is `(label, content)` pairs; `active` is
/// the selected index (clamped to the last tab) — drive it with your own
/// signal, or let the strip's clicks drive it.
const TAB_ACTIVE: &str = "background: #eef4ff; color: #2f6fed; border: none; border-bottom: 2px solid #2f6fed; margin-bottom: -2px; padding: 8px 14px; font-size: 14px;";
const TAB_IDLE: &str = "background: transparent; color: #4b5563; border: none; border-bottom: 2px solid transparent; margin-bottom: -2px; padding: 8px 14px; font-size: 14px;";

#[component]
pub fn Tabs(tabs: Vec<(String, View)>, active: Signal<usize>) -> View {
    let theme = theme_class();
    let selected = active.get().min(tabs.len().saturating_sub(1));
    let panel = if tabs.is_empty() {
        View::default()
    } else {
        tabs[selected].1.clone()
    };
    view! {
        div(class: {format!("kit-tabs {theme}")}) {
            div(class: "kit-tabs-strip",
                style: "display: flex; gap: 2px; border-bottom: 2px solid #e5eaf0;") {
                for tabs.iter().enumerate() -> (i, tab) {
                    button(class: format!("kit-tab{}", if active.get() == i { " kit-tab-active" } else { "" }),
                        style: format!("{}", if active.get() == i { TAB_ACTIVE } else { TAB_IDLE }),
                        on:click = move |_| active.set(i)) {
                        {tab.0.clone()}
                    }
                }
            }
            div(class: "kit-tabs-panel", style: "padding: 12px 2px;") { {panel} }
        }
    }
}

// ── Toasts ──────────────────────────────────────────────────────────────────

/// One toast notification.
#[derive(Clone, Debug)]
pub struct ToastMsg {
    /// Stable identity, assigned by [`push_toast`]. It is the reconciliation
    /// key AND the dismissal target, so removing one toast never re-keys its
    /// neighbors or makes their dismiss handlers fire on the wrong toast.
    pub id: u64,
    pub text: String,
    /// `"info"`, `"success"`, `"error"`, … — picks the accent color.
    pub kind: String,
}

/// Monotonic source of toast identities.
static TOAST_SEQ: AtomicU64 = AtomicU64::new(0);

/// Append a toast to a [`ToastHost`]'s signal (click a toast to dismiss it).
pub fn push_toast(toasts: &Signal<Vec<ToastMsg>>, kind: &str, text: impl Into<String>) {
    let id = TOAST_SEQ.fetch_add(1, Ordering::Relaxed);
    toasts.update(|v| {
        v.push(ToastMsg {
            id,
            text: text.into(),
            kind: kind.to_string(),
        })
    });
}

/// The toast stack, fixed bottom-right. Render it once per page and push
/// into its signal with [`push_toast`]; clicking a toast dismisses it.
#[component]
pub fn ToastHost(toasts: Signal<Vec<ToastMsg>>) -> View {
    let items = toasts.get();
    if items.is_empty() {
        return View::default();
    }
    let theme = theme_class();
    view! {
        div(class: {format!("kit-toasts {theme}")},
            style: "position: fixed; right: 16px; bottom: 16px; z-index: 1100; display: flex; flex-direction: column; gap: 8px;") {
            for items.into_iter() -> t {
                div(class: format!("kit-toast kit-toast-{}", t.kind),
                    style: toast_style(&t.kind),
                    key: t.id,
                    on:click = move |_| toasts.update(|v| v.retain(|m| m.id != t.id))) {
                    span(style: "flex: 1; font-size: 14px; color: inherit;") { {t.text} }
                    span(style: "opacity: 0.7; padding: 0 2px;") { "×" }
                }
            }
        }
    }
}

fn toast_style(kind: &str) -> String {
    let accent = match kind {
        "success" => "#188554",
        "error" => "#d64545",
        _ => "#2f6fed",
    };
    format!(
        "display: flex; gap: 8px; align-items: center; background: #ffffff; border: 1px solid {accent}; border-left: 4px solid {accent}; border-radius: 8px; padding: 10px 14px; min-width: 220px; color: #1f2933; box-shadow: 0 8px 24px rgba(15, 23, 42, 0.18);"
    )
}

// ── Tooltip ─────────────────────────────────────────────────────────────────

/// A tooltip bubble above its content. The engine has no hover events, so
/// the tooltip **toggles on click** (documented deviation); the bubble sits
/// inside the clickable wrapper, so clicking it again closes.
#[component]
pub fn Tooltip(tip: String, children: Children) -> View {
    let open = use_signal(|| false);
    let bubble = if open.get() {
        view! {
            span(class: "kit-tooltip-bubble",
                style: "position: absolute; bottom: 100%; left: 0; margin-bottom: 6px; background: #111827; color: #f9fafb; font-size: 12px; padding: 4px 8px; border-radius: 6px; white-space: nowrap; z-index: 1200;") {
                {tip}
            }
        }
    } else {
        View::default()
    };
    view! {
        span(class: "kit-tooltip",
            style: "position: relative; display: inline-block;",
            on:click = move |_| open.update(|o| *o = !*o)) {
            {children}
            {bubble}
        }
    }
}

// ── shared pager pieces ─────────────────────────────────────────────────────

const NAV_ROW: &str = "display: flex; gap: 10px; align-items: center; padding: 8px 0;";
const NAV_BTN: &str = "background: transparent; border: 1px solid #c9d2dd; border-radius: 6px; padding: 4px 10px; font-size: 13px; color: #1f2933;";
const ROW: &str = "display: flex; padding: 8px 12px; border-bottom: 1px solid #f0f3f7; font-size: 14px; color: #1f2933;";

// ── VirtualList ─────────────────────────────────────────────────────────────

/// A long list rendered as a page-sized window plus a pager (the engine has
/// no scroll events, so virtualization is by page — documented deviation).
/// `page_size == 0` shows everything. `on_row` receives the item's absolute
/// index when a row is clicked.
#[component]
pub fn VirtualList(
    items: Vec<String>,
    page_size: usize,
    page: Signal<usize>,
    on_row: Rc<dyn Fn(usize)>,
) -> View {
    let total = items.len();
    let (start, pages, page_shown) = if page_size == 0 {
        (0, 1, 1)
    } else {
        let pages = ((total + page_size - 1) / page_size).max(1);
        let p = page.get().min(pages - 1);
        (p * page_size, pages, p + 1)
    };
    let end = if page_size == 0 {
        total
    } else {
        (start + page_size).min(total)
    };

    // Rows go through the element builder: each row's click closure must own
    // the Rc row handler plus its absolute index, which doesn't fit a `for`
    // body (no per-iteration statements there).
    let mut body: Vec<VNode> = Vec::new();
    for (i, item) in items.into_iter().enumerate().skip(start).take(end - start) {
        body.push(
            crate::__rt::elem("div")
                .attr("class", "kit-vlist-row")
                .attr("style", ROW)
                .key(crate::__rt::key_val(&i))
                .on("click", {
                    let on_row = on_row.clone();
                    Rc::new(move |_: Event| on_row(i))
                })
                .children(vec![crate::__rt::text(item)])
                .build(),
        );
    }
    view! {
        div(class: "kit-vlist") {
            div(class: "kit-vlist-window",
                style: "border: 1px solid #e5eaf0; border-radius: 8px;") {
                {crate::__rt::frag(body)}
            }
            div(class: "kit-vlist-nav", style: NAV_ROW) {
                button(style: NAV_BTN, on:click = move |_| page.update(|p| *p = p.saturating_sub(1))) { "◀" }
                span(style: "font-size: 13px; color: #6b7280;") { "Page {page_shown} of {pages}" }
                button(style: NAV_BTN, on:click = move |_| page.update(|p| if *p + 1 < pages { *p += 1 })) { "▶" }
            }
        }
    }
}

// ── Table ───────────────────────────────────────────────────────────────────

/// A sortable, paged table drawn with flex rows (no `<table>` — the engine's
/// box model is flex-first). Click a header to sort (numeric-aware:
/// `"9" < "10"`; click again to flip); `on_row` receives the row's
/// **original** index in `rows`, whatever the current sort/page.
const CELL: &str = "flex: 1; font-size: 14px;";

#[component]
pub fn Table(
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    sort: Signal<Option<(usize, bool)>>,
    page_size: usize,
    page: Signal<usize>,
    on_row: Rc<dyn Fn(usize)>,
) -> View {
    let ncols = headers.len();
    let mut indexed: Vec<(usize, Vec<String>)> = rows.into_iter().enumerate().collect();
    if let Some((col, asc)) = sort.get() {
        if col < ncols {
            indexed.sort_by(|a, b| cmp_cell(&a.1, &b.1, col));
            if !asc {
                indexed.reverse();
            }
        }
    }
    let total = indexed.len();
    let (start, pages, page_shown) = if page_size == 0 {
        (0, 1, 1)
    } else {
        let pages = ((total + page_size - 1) / page_size).max(1);
        let p = page.get().min(pages - 1);
        (p * page_size, pages, p + 1)
    };
    let end = if page_size == 0 {
        total
    } else {
        (start + page_size).min(total)
    };

    // Rows × cells is two nested dimensions, and each row's click closure
    // captures its original index — element-builder territory again.
    let mut body: Vec<VNode> = Vec::new();
    for (orig, cells) in indexed.into_iter().skip(start).take(end - start) {
        let mut row_nodes: Vec<VNode> = Vec::new();
        for cell in cells {
            row_nodes.push(
                crate::__rt::elem("span")
                    .attr("class", "kit-table-td")
                    .attr("style", CELL)
                    .children(vec![crate::__rt::text(cell)])
                    .build(),
            );
        }
        body.push(
            crate::__rt::elem("div")
                .attr("class", "kit-table-row")
                .attr("style", ROW)
                .key(crate::__rt::key_val(&orig))
                .on("click", {
                    let on_row = on_row.clone();
                    Rc::new(move |_: Event| on_row(orig))
                })
                .children(row_nodes)
                .build(),
        );
    }
    view! {
        div(class: "kit-table") {
            div(class: "kit-table-head",
                style: "display: flex; border-bottom: 2px solid #e5eaf0;") {
                for headers.iter().enumerate() -> (ci, h) {
                    button(class: "kit-table-th",
                        style: "flex: 1; text-align: left; background: transparent; border: none; padding: 8px 12px; font-size: 13px; font-weight: 600; color: #374151;",
                        on:click = move |_| sort.update(|s| {
                            *s = match *s {
                                Some((c, asc)) if c == ci => Some((ci, !asc)),
                                _ => Some((ci, true)),
                            };
                        })) {
                        span { {h.clone()} {arrow_for(sort.get(), ci)} }
                    }
                }
            }
            div(class: "kit-table-body") { {crate::__rt::frag(body)} }
            div(class: "kit-table-nav", style: NAV_ROW) {
                button(style: NAV_BTN, on:click = move |_| page.update(|p| *p = p.saturating_sub(1))) { "◀" }
                span(style: "font-size: 13px; color: #6b7280;") { "Page {page_shown} of {pages}" }
                button(style: NAV_BTN, on:click = move |_| page.update(|p| if *p + 1 < pages { *p += 1 })) { "▶" }
            }
        }
    }
}

/// Numeric-aware cell compare for [`Table`]: numbers sort numerically,
/// everything else lexicographically; blanks sort last-ish (as "").
fn cmp_cell(a: &[String], b: &[String], col: usize) -> std::cmp::Ordering {
    let (x, y) = (
        a.get(col).map(String::as_str).unwrap_or(""),
        b.get(col).map(String::as_str).unwrap_or(""),
    );
    match (x.parse::<f64>(), y.parse::<f64>()) {
        (Ok(p), Ok(q)) => p.partial_cmp(&q).unwrap_or(std::cmp::Ordering::Equal),
        _ => x.cmp(y),
    }
}

/// Header sort indicator: `▲` ascending, `▼` descending, blank unsorted.
fn arrow_for(sort: Option<(usize, bool)>, col: usize) -> String {
    match sort {
        Some((c, true)) if c == col => " ▲".to_string(),
        Some((c, false)) if c == col => " ▼".to_string(),
        _ => String::new(),
    }
}

// ── DatePicker ──────────────────────────────────────────────────────────────

/// An always-open inline calendar bound to a `yyyy-mm-dd` string signal.
/// Browse months with ‹ › ; click a day to write the ISO date back into the
/// signal. (A popup-with-input variant is a planned follow-up.)
const WEEKDAYS: [&str; 7] = ["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"];
const DAY_IDLE: &str = "width: 34px; height: 30px; background: transparent; border: none; font-size: 13px; color: #1f2933; border-radius: 6px;";
const DAY_SELECTED: &str = "width: 34px; height: 30px; background: #2f6fed; border: none; font-size: 13px; color: #ffffff; border-radius: 6px;";

#[component]
pub fn DatePicker(value: Signal<String>) -> View {
    let theme = theme_class();
    let (y0, m0) = match parse_ymd(&value.get()) {
        Some((y, m, _)) => (y, m),
        None => today_ym(),
    };
    // The month being browsed: seeded from the value once, then independent
    // of it (picking a day syncs back; browsing months doesn't).
    let view_ym = use_signal(move || (y0, m0));
    let (y, m) = view_ym.get();
    let first_dow = day_of_week(y, m, 1) as usize;
    let ndays = days_in_month(y, m);
    let label = format!("{m:02}/{y}");
    let selected = value.get();

    // Day grid: per-day click closures capture the Copy signal handle and
    // that day's formatted ISO date.
    let mut grid: Vec<VNode> = Vec::new();
    for _ in 0..first_dow {
        grid.push(
            crate::__rt::elem("span")
                .attr("class", "kit-cal-blank")
                .attr("style", "width: 34px; height: 30px;")
                .build(),
        );
    }
    for d in 1..=ndays {
        let iso = format!("{y:04}-{m:02}-{d:02}");
        let is_sel = selected == iso;
        grid.push(
            crate::__rt::elem("button")
                .attr(
                    "class",
                    if is_sel {
                        "kit-cal-day kit-cal-day-selected"
                    } else {
                        "kit-cal-day"
                    },
                )
                .attr("style", if is_sel { DAY_SELECTED } else { DAY_IDLE })
                .on("click", Rc::new(move |_: Event| value.set(iso.clone())))
                .children(vec![crate::__rt::text(d.to_string())])
                .build(),
        );
    }

    view! {
        div(class: {format!("kit-datepicker {theme}")},
            style: "display: inline-block; border: 1px solid #e5eaf0; border-radius: 10px; padding: 12px; background: #ffffff; color: #1f2933;") {
            div(class: "kit-cal-head",
                style: "display: flex; align-items: center; justify-content: space-between; padding-bottom: 8px;") {
                button(class: "kit-cal-prev", style: NAV_BTN, on:click = move |_| view_ym.update(prev_month)) { "‹" }
                strong(class: "kit-cal-label", style: "font-size: 14px;") { {label} }
                button(class: "kit-cal-next", style: NAV_BTN, on:click = move |_| view_ym.update(next_month)) { "›" }
            }
            div(class: "kit-cal-weekdays", style: "display: flex;") {
                for WEEKDAYS.iter().copied() -> (w) {
                    span(style: "width: 34px; text-align: center; font-size: 11px; color: #9aa4b2; padding-bottom: 4px;") { {w} }
                }
            }
            div(class: "kit-cal-grid", style: "display: flex; flex-wrap: wrap; width: 238px;") {
                {crate::__rt::frag(grid)}
            }
            div(class: "kit-cal-value", style: "padding-top: 8px; font-size: 13px; color: #6b7280;") {
                "Selected: {selected}"
            }
        }
    }
}

fn prev_month(v: &mut (i64, u32)) {
    if v.1 == 1 {
        v.1 = 12;
        v.0 -= 1;
    } else {
        v.1 -= 1;
    }
}

fn next_month(v: &mut (i64, u32)) {
    if v.1 == 12 {
        v.1 = 1;
        v.0 += 1;
    } else {
        v.1 += 1;
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 30,
    }
}

/// Day of week by Sakamoto's method — 0 = Sunday. (Checked against
/// 2000-01-01 = Saturday and 2026-09-01 = Tuesday.)
fn day_of_week(y: i64, m: u32, d: u32) -> u32 {
    let (y, m, d) = (y as i64, m as i64, d as i64);
    let a = (14 - m) / 12;
    let yy = y - a;
    let mm = m + 12 * a - 2;
    ((d + yy + yy.div_euclid(4) - yy.div_euclid(100) + yy.div_euclid(400) + (31 * mm) / 12)
        .rem_euclid(7)) as u32
}

/// Parse `yyyy-mm-dd`, rejecting impossible month/day combinations.
fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let mut it = s.split('-');
    let y = it.next()?.parse::<i64>().ok()?;
    let m = it.next()?.parse::<u32>().ok()?;
    let d = it.next()?.parse::<u32>().ok()?;
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some((y, m, d))
}

/// Today as (year, month) in UTC — the fallback browse month when the bound
/// value is empty or unparseable. Howard Hinnant's civil-from-days over the
/// Unix epoch, pure std.
fn today_ym() -> (i64, u32) {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86_400)
        .unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32)
}
