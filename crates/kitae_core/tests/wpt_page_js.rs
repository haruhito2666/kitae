//! Page-JS conformance harness (the Track-A "conformance spine", first
//! slice).
//!
//! Runs a vendored subset of the web-platform-tests DOM/events/URL
//! corpora through the real `page-js` path: each test file is spliced
//! into a document with WPT's own `testharness.js` inlined — the same
//! bootstrap every browser passes its DOM tests with — loaded through
//! `Browser::load_html` with page JS on, and driven to completion by
//! polling like a host's idle tick.
//!
//! `docs/WPT_SCORECARD.md` records the measured pass rate; the baseline
//! assertion guards against regressions only (measure, never claim).
//! Set `KITAE_WPT_DIFF=1` to print every per-test result line.
//!
//! Normalizations (documented, mirrored from the other WPT harnesses):
//!   * `<script src=...>` tags that reference `/resources/` are dropped;
//!     `testharness.js` is inlined and a reporter script is appended that
//!     emits one `KITAE_WPT|<status>|<name>|<message>` console line per
//!     test plus a `KITAE_WPT_DONE|<pass>|<fail>|<total>` summary.
//!   * `.any.js` files contribute their body (minus `// META:` lines) as
//!     one inline script.
//!   * Statuses: 0 pass, 1 fail, 2 timeout, 3 not-run, 4 precondition.
//!     Not-run/timeout count as not-passed.

use kitae_core::{ApproxMeasurer, Browser, Host, PublicState};
use kitae_layout::{DisplayList, TextMeasurer};
use kitae_net::FetchError;

const TESTHARNESS: &str = include_str!("data/wpt_pagejs/testharness.js");

/// (label, file, needs_loopback). The URL corpora are data-driven: they
/// `fetch("resources/urltestdata*.json")` relative to the page, so those
/// files load from a loopback origin the harness serves instead of the
/// (unreachable) `wpt.test` host.
const FILES: &[(&str, &str, bool)] = &[
    (
        "wpt_events_constructors",
        "Event-constructors.any.js",
        false,
    ),
    (
        "wpt_events_bubbles_true",
        "Event-dispatch-bubbles-true.html",
        false,
    ),
    (
        "wpt_events_bubbles_false",
        "Event-dispatch-bubbles-false.html",
        false,
    ),
    (
        "wpt_events_order_at_target",
        "Event-dispatch-order-at-target.html",
        false,
    ),
    (
        "wpt_events_stop_propagation",
        "Event-dispatch-multiple-stopPropagation.html",
        false,
    ),
    (
        "wpt_events_target_dispatch",
        "EventTarget-dispatchEvent.html",
        false,
    ),
    (
        "wpt_events_once",
        "AddEventListenerOptions-once.any.js",
        false,
    ),
    ("wpt_url_constructor", "url-constructor.any.js", true),
    ("wpt_url_origin", "url-origin.any.js", true),
    ("wpt_nodes_text_content", "Node-textContent.html", false),
    (
        "wpt_mutation_observer_text_content",
        "MutationObserver-textContent.html",
        false,
    ),
];

/// Recorded in docs/WPT_SCORECARD.md (measured 2026-10-16, tier JS-4):
/// 1213/1349 = 89.9% with the MutationObserver file enabled.
const BASELINE_PASS_RATE: f64 = 0.898;

fn vendored(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/data/wpt_pagejs/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("vendored WPT file {name}: {e}"))
}

/// `.any.js`: drop `// META:` lines, return the body as one script chunk.
fn any_js_body(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("// META:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `.html`: drop every `<script src=...>` tag whose target lives under
/// `/resources/` (testharness itself is inlined by the caller); keep
/// everything else so the engine runs the file's inline scripts in
/// document order.
fn strip_resource_scripts(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(i) = rest.find("<script") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(end) = tail.find("</script>") else {
            out.push_str(tail);
            return out;
        };
        let tag = &tail[..end + "</script>".len()];
        let is_resource = tag.contains("src=") && tag.contains("/resources/");
        if !is_resource {
            out.push_str(tag);
        }
        rest = &tail[end + "</script>".len()..];
    }
    out.push_str(rest);
    out
}

fn page_for(name: &str) -> String {
    let src = vendored(name);
    let body = if name.ends_with(".any.js") {
        // A bare script body has no document; wrap it. The trailing
        // paragraph keeps the document out of the text-thin interstitial
        // policy, so a page whose scripts failed still reports its
        // console (the failure message) instead of going silent.
        format!("<script>{}</script><p>wpt runner</p>", any_js_body(&src))
    } else if name.ends_with(".html") {
        strip_resource_scripts(&src)
    } else {
        panic!("unsupported WPT file kind: {name}")
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>wpt {name}</title>\
         <script>try{{(function(){{{TESTHARNESS}}})()}}catch(e){{console.log('KITAE_WPT_BOOT|' + e + '|' + (e && e.stack ? e.stack : ''));if(0){{}}}}</script>\
         <script>\
         function subsetTestByKey(key, func) {{\
           var args = Array.prototype.slice.call(arguments, 2);\
           return func.apply(null, args);\
         }}\
         </script>\
         <script>\
         add_completion_callback(function (tests, status) {{\
           console.log('KITAE_WPT_META|' + typeof tests + '|' + (tests ? tests.length : 'n/a') + '|' + (tests && tests[0] ? typeof tests[0].status + '=' + tests[0].status : 'no0'));\
           var pass = 0, fail = 0;\
           for (var i = 0; i < tests.length; i++) {{\
             var t = tests[i];\
             var msg = (t.message === null || t.message === undefined ? '' : ('' + t.message)).replace(/[\\r\\n]+/g, ' ').slice(0, 900);\
             console.log('KITAE_WPT|' + t.status + '|' + t.name + '|' + msg);\
             if (t.status === 0) pass++; else fail++;\
           }}\
           console.log('KITAE_WPT_DONE|' + pass + '|' + fail + '|' + tests.length);\
         }});\
         </script>\
         <script>setup({{ output: false }});</script></head><body>{body}</body></html>"
    )
}

/// A minimal loopback HTTP origin for the data-driven URL corpora: the
/// vendored JSON files are served at their WPT-relative paths so the
/// page's own `fetch("resources/...")` resolves same-origin, exactly as
/// it would on a real WPT server.
struct Loopback {
    base: String,
}

fn start_loopback(routes: Vec<(String, Vec<u8>, &'static str)>) -> Loopback {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    let addr = listener.local_addr().expect("loopback addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = vec![0u8; 16 * 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = req
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let hit = routes.iter().find(|(p, _, _)| *p == path);
            match hit {
                Some((_, body, ctype)) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(body);
                }
                None => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
            let _ = stream.flush();
        }
    });
    Loopback {
        base: format!("http://{addr}/"),
    }
}

/// The loopback origin for a run: JSON corpora under `/resources/`, page
/// served from the root so relative fetches resolve.
fn url_loopback() -> Loopback {
    start_loopback(vec![
        (
            "/resources/urltestdata.json".to_string(),
            vendored("urltestdata.json").into_bytes(),
            "application/json",
        ),
        (
            "/resources/urltestdata-javascript-only.json".to_string(),
            vendored("urltestdata-javascript-only.json").into_bytes(),
            "application/json",
        ),
    ])
}

struct WptHost {
    console: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Host for WptHost {
    fn publish(&mut self, _state: &PublicState, _dl: &DisplayList) {}
    fn scroll_updated(&mut self, _state: &PublicState) {}
    fn set_loading(&mut self, _loading: bool) {}
    fn open_external(&mut self, _url: &str) {}
    fn notify(&mut self, message: &str) {
        if let Some(line) = message.strip_prefix("console\t") {
            self.console.lock().unwrap().push(line.to_string());
        }
    }
}

struct Outcome {
    pass: usize,
    fail: usize,
    lines: Vec<String>,
}

/// Load one vendored file with page JS on and drive the hostile-timeout
/// poll loop until the summary line shows up (or the deadline trips).
fn run_one(file: &str, base: &str) -> Outcome {
    let console = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut b = Browser::new(
        Box::new(WptHost {
            console: console.clone(),
        }),
        Box::new(ApproxMeasurer),
        (800.0, 600.0),
    );
    b.set_page_js(true);
    b.load_html(&page_for(file), base);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut done = false;
    while !done {
        std::thread::sleep(std::time::Duration::from_millis(2));
        b.poll_page_js();
        done = console
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("KITAE_WPT_DONE|"));
        assert!(
            std::time::Instant::now() < deadline,
            "WPT file {file} never completed; console so far: {:?}",
            console.lock().unwrap()
        );
    }
    let lines = console.lock().unwrap().clone();
    let done_line = lines
        .iter()
        .find(|l| l.starts_with("KITAE_WPT_DONE|"))
        .cloned()
        .unwrap_or_default();
    let nums: Vec<usize> = done_line
        .trim_start_matches("KITAE_WPT_DONE|")
        .split('|')
        .filter_map(|p| p.parse().ok())
        .collect();
    let (pass, fail) = match nums.as_slice() {
        [p, f, ..] => (*p, *f),
        _ => (0, 0),
    };
    Outcome { pass, fail, lines }
}

/// The page-JS tier must be switched on for the harness to measure
/// anything — without the feature there is no page runtime and no rate to
/// hold. The skip is deliberate and visible rather than a silent 0/0.
#[cfg(not(feature = "page-js"))]
#[test]
fn wpt_page_js_conformance_subset_requires_the_page_js_feature() {
    eprintln!(
        "wpt_page_js: skipped without the page-js feature \
         (run: cargo test -p kitae_core --features page-js --test wpt_page_js)"
    );
}

#[cfg(feature = "page-js")]
#[test]
fn wpt_page_js_conformance_subset_holds_the_baseline() {
    let diff = std::env::var("KITAE_WPT_DIFF").is_ok();
    let only = std::env::var("KITAE_WPT_ONLY").ok();
    let mut total_pass = 0usize;
    let mut total = 0usize;
    let mut loopback: Option<Loopback> = None;
    for (label, file, needs_loopback) in FILES {
        if let Some(only) = &only {
            if !label.contains(only.as_str()) && !file.contains(only.as_str()) {
                continue;
            }
        }
        let base = if *needs_loopback {
            let l = loopback.get_or_insert_with(url_loopback);
            format!("{}{file}", l.base)
        } else {
            format!("https://wpt.test/{file}")
        };
        let outcome = run_one(file, &base);
        total_pass += outcome.pass;
        total += outcome.pass + outcome.fail;
        eprintln!(
            "{label}: {}/{} passed ({file})",
            outcome.pass,
            outcome.pass + outcome.fail
        );
        if diff || outcome.fail > 0 {
            for line in &outcome.lines {
                if let Some(rest) = line.strip_prefix("KITAE_WPT|") {
                    if diff || !rest.starts_with('0') {
                        eprintln!("    {rest}");
                    }
                }
            }
        }
    }
    let rate = if total == 0 {
        0.0
    } else {
        total_pass as f64 / total as f64
    };
    eprintln!("total: {total_pass}/{total} ({:.1}%)", rate * 100.0);
    assert!(total > 0, "the vendored WPT subset must actually run");
    assert!(
        rate + 1e-9 >= BASELINE_PASS_RATE,
        "WPT page-JS pass rate fell below the recorded baseline: {:.3} < {:.3} \
         (see docs/WPT_SCORECARD.md; update the baseline only in the same \
         commit that re-measures)",
        rate,
        BASELINE_PASS_RATE
    );
    let _ = std::any::type_name::<dyn TextMeasurer>();
    let _ = std::any::type_name::<FetchError>();
}
