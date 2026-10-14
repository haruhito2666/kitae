//! The Kitae start page — a three-section onboarding story that doubles
//! as a living engine test-bed. Every section deliberately exercises a
//! specific part of the renderer, so a rendering defect here pins the
//! next engine fix:
//!
//! * hero — a full-bleed QTSQ background image (the encoded file ships
//!   inside the binary and rides an inline `data:image/qtsq;base64,` URL,
//!   which the engine decodes natively — no other format involved),
//!   `text-shadow` blur, and a curved bottom edge (an inline SVG dome — the
//!   engine paints elliptical `border-radius` as clamped corners, so the
//!   curve is drawn honestly);
//! * how-to — keyboard shortcut rows, real link pills, and the
//!   interactive demo card: a counter whose every click becomes one
//!   `SetText` patch spliced into the display list without a full-page
//!   relayout;
//! * FAQ — real `<details>`/`<summary>` accordions (the engine's A5
//!   toggle path: clicking a summary flips the parent's `open` attribute
//!   and relayouts), with the site footer directly below.
//!
//! The component tree mounts at `about:start`; the engine applies its patch
//! ops to the live DOM, so style, layout, find-in-page and the
//! accessibility tree all work unchanged — without a single line of script.

use std::rc::Rc;
use std::sync::OnceLock;

use kitae_ui::prelude::*;

/// The hero background image, embedded verbatim as a QTSQ file — the
/// engine's native image format — and exposed to the page as an inline
/// `data:image/qtsq;base64,` URL. The relayout stamping pass decodes it
/// straight from the payload (`ensure_qtsq_raster`), so no other format is
/// ever involved. Decryption uses the QTSQ device key when present; where
/// it is not, the engine keeps its placeholder box.
static HERO_BG_DATA_URL: OnceLock<String> = OnceLock::new();

/// The inline `data:image/qtsq;base64,` URL of the embedded hero image.
/// Shared with the plain-HTML mirror (`start_page_html`) so both paths
/// carry the identical bytes.
pub(crate) fn hero_bg_data_url() -> &'static str {
    HERO_BG_DATA_URL.get_or_init(|| {
        const HERO_BG_QTSQ: &[u8] = include_bytes!("../assets/hero_garden.qtsq");
        format!("data:image/qtsq;base64,{}", b64_encode(HERO_BG_QTSQ))
    })
}

/// Minimal standard base64 encoder (RFC 4648, padded) for the inline QTSQ
/// data URL.
fn b64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
            | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The page stylesheet. It ships inside the component tree as a `<style>`
/// element — `styles_for_dom` collects `<style>` blocks exactly as it does
/// for fetched pages (the UA sheet keeps the element itself invisible).
const START_CSS: &str = r#"
:root { --bg:#0e0e12; --ink:#ededf2; --muted:#9a9aa8; --line:#2a2a35; --red:#e5484d; }
body { margin:0; padding:0; background:#0e0e12; color:#ededf2;
       font-family:-apple-system,'Helvetica Neue',Arial,sans-serif; }
.story-root { min-height:100vh; }
.hero { position:relative; overflow:hidden; text-align:center; height:100vh;
        display:flex; align-items:center; justify-content:center;
        padding:0 24px;
        background:linear-gradient(to bottom, #181826 0%, #101018 100%); }
/* The engine can't resolve a percentage height on an out-of-flow box yet
   (the abs drain has no containing-block height), so the background wrap
   pins the same 100vh height as the hero. The image itself stays inline
   inside the wrap: replaced elements only reach the display list through
   the inline compose path — a positioned replaced element as the direct
   abs child never composes. object-fit:fill stretches the raster over the
   whole hero — the full image, no crop. */
.hero-bg { position:absolute; left:0; top:0; width:100%; height:100vh; }
.hero-bg-img { width:100%; height:100vh; object-fit:fill; }
.hero-copy { position:relative; max-width:640px; }
.hero-title { font-family:Georgia,'Times New Roman',serif; font-size:64px;
              font-weight:600; margin:0 0 10px;
              text-shadow:0 2px 24px rgba(0,0,0,0.55); }
.hero-italic { font-family:Georgia,'Times New Roman',serif; font-style:italic;
               font-size:30px; color:#d8d8e4; margin:0 0 26px; }
.hero-sub { font-size:16px; line-height:1.65; color:#a6a6b4; margin:0 auto; max-width:540px; }
.hero-curve { position:absolute; left:50%; margin-left:-1200px; bottom:-20px; }
.story { max-width:1280px; margin:0 auto; padding:0 48px; }
/* Every section owns one full window: each .screen is exactly one viewport
   tall (min-height:100vh) and centers its chapter, so no neighbouring
   section's content peeks into the previous window. */
/* The flex layout resolves a column container's main-axis free space
   against its WIDTH, so column flex can't center vertically. Every
   centered section is therefore a ROW flex: a zero-width, full-height
   pad makes the cross axis one viewport tall and `align-items:center`
   drops the section's content into the exact vertical middle. */
.screen-pad { width:0; height:100vh; }
.screen { position:relative; min-height:100vh;
          display:flex; align-items:center; justify-content:center; }
.chapter { width:100%; margin:0; text-align:center; }
.badge { display:inline-block; border:1px solid #3d3d4c; border-radius:999px;
         padding:6px 18px; font-size:12px; color:#b8b8c8; font-style:italic;
         font-family:Georgia,'Times New Roman',serif; letter-spacing:1px; }
.ch-title { font-family:Georgia,'Times New Roman',serif; font-size:40px;
            font-weight:600; margin:20px 0 44px; }
/* The two topic cards share the row: flex-grow splits the wide story
   column evenly, so the section uses the full window width and stays
   short enough to fit inside its one-viewport screen. */
.topic-row { display:flex; flex-wrap:wrap; gap:32px; text-align:left; }
.topic { flex-grow:1; flex-shrink:1; flex-basis:480px;
         border:1px solid #26262f; border-radius:22px;
         padding:30px 32px 34px; background:#12121a;
         transition:transform 0.2s ease; }
.topic:hover { transform:translateY(-3px); }
.topic-title { font-family:Georgia,'Times New Roman',serif; font-size:24px;
                font-weight:600; margin:0 0 18px; }
.card-note { color:#9a9aa8; font-size:14px; line-height:1.7; margin:0 0 16px; }
.pill-row { display:flex; flex-wrap:wrap; gap:10px; margin:2px 0 20px; }
.pill { display:inline-block; border:1px solid #34343f; border-radius:999px;
        padding:8px 16px; color:#cfcfda; text-decoration:none; font-size:14px;
        background:#1b1b24; white-space:nowrap;
        transition:transform 0.16s ease, background-color 0.16s ease, color 0.16s ease; }
.pill:hover { border-color:#e5484d; color:#ffffff; background:#232331;
              transform:translateY(-2px); }
/* Key rows are flex rows with BLOCK children (divs): the engine's flex
   boxes block-level items properly, while plain inline spans get no box
   at all in inline flow (R3) — a span chip lost its background entirely,
   and spans in a flex row overlapped. Divs side by side with a gap are
   the reliable shape. */
.key-row { display:flex; align-items:center; gap:12px; margin:0 0 14px; }
/* No min-width here: the flex row advances items by their content width
   when a min-width clamp is present, which slid the description under the
   chip. Padding alone sizes the chip. */
.key { text-align:center;
       font-family:Menlo,monospace; font-size:13px; background:#20202b;
       border:1px solid #34343f; border-bottom-width:2px; border-radius:8px;
       padding:5px 14px; color:#e8e8f0;
       transition:background-color 0.15s ease, color 0.15s ease; }
.key:hover { background:#2b2b3a; color:#ffffff; }
.key-desc { color:#a8a8b6; font-size:14px; }
.kitae-card { border:1px solid #26262f; border-radius:14px; background:#16161f;
              padding:18px 20px 20px; margin:0 0 18px; }
.kitae-card h3 { font-size:17px; font-weight:600; margin:0 0 6px; }
.kitae-card p { color:#9a9aa8; font-size:13px; line-height:1.6; margin:0 0 12px; }
.demo-btn { background:#e5484d; color:#ffffff; border:none; border-radius:999px;
            padding:9px 20px; font-size:14px; font-weight:600;
            transition:background-color 0.15s ease, transform 0.15s ease; }
.demo-btn:hover { background:#f0565b; transform:translateY(-1px); }
.demo-btn:active { transform:translateY(1px); }
.faq-wrap { max-width:920px; margin:0 auto; text-align:left; }
.faq { border:1px solid #26262f; border-radius:14px; background:#12121a;
       padding:16px 22px; margin:0 0 14px; text-align:left;
       transition:transform 0.18s ease; }
.faq:hover { transform:translateY(-2px); }
.faq summary { cursor:pointer; font-weight:600; font-size:15px; color:#dedeea;
               transition:color 0.15s ease; }
.faq summary:hover { color:#ffffff; }
.faq .card-note { margin:10px 0 0; }
/* The footer anchors to the bottom of the FAQ window (abs against the
   .screen containing block), so "FAQ + footer" fills exactly one window
   with no dead void between the last answer and the site footer. */
.site-footer { position:absolute; left:0; right:0; bottom:0; text-align:center;
               padding:26px 0 30px; border-top:1px solid #2a2a35; }
.colophon-tag { font-family:Georgia,'Times New Roman',serif; font-style:italic;
                font-size:22px; color:#c9c9d6; margin:0 0 10px; }
.colophon-fine { font-size:13px; color:#77778a; max-width:460px; margin:0 auto;
                 line-height:1.6; }
"#;

/// The hero's curved bottom edge — Dia-style dark dome rising into the
/// hero's lower area. Drawn honestly as an SVG path: the engine paints
/// `border-radius: 50%` as clamped rounded corners, not an ellipse. The
/// hero's `overflow: hidden` clips the dome's overscan; the fill is the
/// page background so the story section appears to curve up into the hero.
#[component]
fn HeroCurve() -> View {
    let dome = kitae_ui::__rt::elem("path")
        .attr("d", "M0 240 L0 150 Q 1200 -30 2400 150 L2400 240 Z")
        .attr("fill", "#0e0e12")
        .build();
    let svg = kitae_ui::__rt::elem("svg")
        .attr("class", "hero-curve")
        .attr("width", "2400")
        .attr("height", "240")
        .attr("viewBox", "0 0 2400 240")
        .children(vec![dome])
        .build();
    View::from_nodes(vec![svg])
}

/// The interactive demo card: a counter that proves incremental updates —
/// one signal change becomes one `SetText` patch, spliced into the display
/// list without a full-page relayout.
///
/// `use_signal` allocates fresh state per call, so every signal on this
/// page is created once in [`start_page_app`] and passed in — that is the
/// local-runtime state contract: state lives in the closure, components are
/// pure functions of it.
#[component]
fn CounterDemo(count: Signal<u32>) -> View {
    view! {
        div(class: "kitae-card") {
            h3 { "Incremental demo" }
            p { "One signal change → one patch op → one spliced frame. No full relayout." }
            button(class: "demo-btn", on:click = move |_| count.update(|c| *c += 1)) { "Count: {count}" }
        }
    }
}

/// One window of the start page: a kicker badge, a serif heading and free
/// content, vertically centered the engine-friendly way (row flex + the
/// zero-width full-height pad, see START_CSS).
#[component]
fn Section(kicker: String, title: String, children: Children) -> View {
    view! {
        section(class: "screen") {
            div(class: "screen-pad")
            div(class: "chapter") {
                div(class: "badge") { "{kicker}" }
                h2(class: "ch-title") { "{title}" }
                {children}
            }
        }
    }
}

/// The start page component. `href` links keep the ordinary `<a>` path:
/// they produce LinkAreas and navigate through the engine's link handling.
#[component]
pub fn StartPage(count: Signal<u32>) -> View {
    view! {
        div(class: "story-root") {
            style { {START_CSS} }
            header(class: "hero") {
                div(class: "hero-bg") {
                    img(class: "hero-bg-img", src: (hero_bg_data_url()), alt: "")
                }
                div(class: "screen-pad")
                div(class: "hero-copy") {
                    h1(class: "hero-title") { "Kitae, built the hard way." }
                    p(class: "hero-italic") { "No scripts. No shortcuts." }
                    p(class: "hero-sub") {
                        "A browser engine written in Rust with its own parser, its own layout and its own renderer — and no JavaScript anywhere in the loop. The chapters below are a short tour; this very page is the test bench it runs on."
                    }
                }
                HeroCurve()
            }
            main(class: "story") {
                Section("How to use · How it works".into(), "Get going, then look under the hood".into()) {
                    div(class: "topic-row") {
                        div(class: "topic") {
                            h3(class: "topic-title") { "How to use" }
                            div(class: "pill-row") {
                                a(class: "pill", href: "https://example.com") { "example.com" }
                                a(class: "pill", href: "https://text.npr.org") { "text.npr.org" }
                                a(class: "pill", href: "https://lite.duckduckgo.com/lite") { "DuckDuckGo Lite" }
                                a(class: "pill", href: "https://en.wikipedia.org/wiki/Main_Page") { "Wikipedia" }
                                a(class: "pill", href: "https://wiby.me") { "wiby.me" }
                            }
                            p(class: "card-note") {
                                "Visit any of these — no script runs anywhere in Kitae, so pages arrive as text, links and images. Plain words in the address bar search DuckDuckGo Lite."
                            }
                            div(class: "key-row") { div(class: "key") { "⌘K" } div(class: "key-desc") { "Command palette — every tab and action, type to filter" } }
                            div(class: "key-row") { div(class: "key") { "⌘L" } div(class: "key-desc") { "Focus the address bar" } }
                            div(class: "key-row") { div(class: "key") { "⌘T" } div(class: "key-desc") { "New tab · ⌘⌥W closes the current one" } }
                            div(class: "key-row") { div(class: "key") { "⌘," } div(class: "key-desc") { "Settings" } }
                        }
                        div(class: "topic") {
                            h3(class: "topic-title") { "How it works" }
                            CounterDemo(count)
                            p(class: "card-note") {
                                "That counter is a real form control laid out by kitae_layout; each click is one SetText patch spliced into the display list — no full relayout."
                            }
                            p(class: "card-note") {
                                b { "file:///some/page.html" } " opens local files, and " b { "kitae --dump <url>" } " prints a headless text dump in a terminal."
                            }
                        }
                    }
                }
                Section("FAQ".into(), "Frequently asked questions".into()) {
                    div(class: "faq-wrap") {
                        details(class: "faq", open: "") {
                            summary { "Does any JavaScript run, ever?" }
                            p(class: "card-note") {
                                "No. Kitae parses, lays out and renders every page itself; there is no script engine to run, so no page can execute code."
                            }
                        }
                        details(class: "faq") {
                            summary { "What image formats can it show?" }
                            p(class: "card-note") {
                                "QTSQ natively, plus PNG, JPEG, GIF, WebP and SVG decoded by the engine's own media stack."
                            }
                        }
                        details(class: "faq") {
                            summary { "Can it play video or audio?" }
                            p(class: "card-note") {
                                "Yes — video and audio elements render and play through the engine's own media layer, with real controls."
                            }
                        }
                        details(class: "faq") {
                            summary { "What happens on script-heavy sites?" }
                            p(class: "card-note") {
                                "You get the server-rendered HTML: text, links and images. Anything that only exists through script never arrives — that is the point."
                            }
                        }
                        details(class: "faq") {
                            summary { "Is this page itself running script?" }
                            p(class: "card-note") {
                                "No — it is kitae_ui components styled with plain CSS and rendered by the Kitae engine, this very binary."
                            }
                        }
                    }
                    footer(class: "site-footer") {
                        p(class: "colophon-tag") { "The web, without the noise." }
                        p(class: "colophon-fine") {
                            "This page is kitae_ui components styled with plain CSS and rendered by the Kitae engine. Every gradient, curve and toggle is a live test — if something renders wrong, that's the next fix."
                        }
                    }
                }
            }
        }
    }
}

/// The start page as a render closure for [`crate::ui::LocalApp::new`]. All
/// signals are allocated here, once, so every re-render reads the same
/// state (the Phase 2 live runtime hoists the same closure server-side).
pub fn start_page_app() -> Rc<dyn Fn() -> View> {
    let count = use_signal(|| 0u32);
    Rc::new(move || StartPage(count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_the_start_copy() {
        let mut app = crate::ui::LocalApp::new(start_page_app());
        let (dom, _body) = app.mount_dom();
        let html = dom.outer_html(dom.root().unwrap());
        assert!(html.contains("Kitae, built the hard way."));
        assert!(html.contains("No scripts. No shortcuts."));
        assert!(html.contains("https://example.com"));
        assert!(html.contains("How to use"));
        assert!(html.contains("How it works"));
        assert!(html.contains("Frequently asked questions"));
        assert!(html.contains("Does any JavaScript run, ever?"));
        assert!(html.contains("The web, without the noise."));
        // The stylesheet ships inside the component tree; styles_for_dom
        // collects <style> blocks exactly as it does for fetched pages.
        assert!(html.contains(".faq"));
        // No script anywhere — the page is data, not code.
        assert!(!html.to_lowercase().contains("<script"));
    }

    #[test]
    fn faq_opens_one_item_by_default() {
        let mut app = crate::ui::LocalApp::new(start_page_app());
        let (dom, _body) = app.mount_dom();
        let html = dom.outer_html(dom.root().unwrap());
        let open_count = html.matches("<details class=\"faq\" open=\"\"").count()
            + html.matches("<details open=\"\"").count();
        assert_eq!(open_count, 1, "exactly one FAQ item starts open");
        // The engine's details/summary layout: a closed details shows only
        // its summary, an open one its answers too.
        assert!(
            html.contains("<summary>"),
            "FAQ summaries are real summary elements"
        );
    }
}
