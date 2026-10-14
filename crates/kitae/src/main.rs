//! Kitae — SDL2 shell over the kitae_core engine.
//!
//! This binary is a thin host: it owns the OS window and event pump, but all
//! browser logic (net, HTML, CSS, style, layout, history, the JS-content
//! policy) lives in the kitae_core crate behind the `Host` trait — the same
//! interface the Swift macOS app implements. It exists as a portable
//! regression gate: if a page renders correctly here, the engine is right
//! and any host bug is on the host side.
//!
//! No JavaScript is ever fetched, stored, or executed — script-only pages
//! get a noscript fallback or an interstitial with an escape hatch.

use std::rc::Rc;
use std::time::Duration;

use kitae_core::{run_dump, Browser, Host, PublicState, START_URL};
use kitae_layout::DisplayList;
use kitae_render::{Event, FfiMeasurer, Renderer};

const WINDOW_W: i32 = 1100;
const WINDOW_H: i32 = 740;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--dump" {
        run_dump(&to_start_url(&args[2]));
        return;
    }
    // `kitae --password <pw> file.qtsq` pre-seeds the session QTSQ password
    // so a locked file decodes on the first load instead of locking.
    let mut password: Option<String> = None;
    let mut rest: Vec<&String> = Vec::new();
    let mut skip_next = false;
    for arg in args.iter().skip(1) {
        if skip_next {
            password = Some(arg.clone());
            skip_next = false;
        } else if arg == "--password" {
            skip_next = true;
        } else {
            rest.push(arg);
        }
    }
    if skip_next {
        eprintln!("kitae: --password needs a value");
        std::process::exit(1);
    }
    let start = rest
        .first()
        .map(|a| to_start_url(a))
        .unwrap_or_else(|| START_URL.to_string());
    run_gui(&start, password.as_deref());
}

/// `kitae test.qtsq` (or any existing local path) opens the file viewer,
/// same code path as `kitae http://…`. URLs pass through untouched; bare
/// paths become absolute file:// URLs — without this, normalize_input
/// would read the dot in `test.qtsq` as a host and try https.
fn to_start_url(arg: &str) -> String {
    if arg.starts_with("http://")
        || arg.starts_with("https://")
        || arg.starts_with("file://")
        || arg.starts_with("about:")
    {
        return arg.to_string();
    }
    let path = std::path::Path::new(arg);
    if path.exists() {
        let absolute = std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        return format!("file://{absolute}");
    }
    arg.to_string()
}

/// The shell as a host: forwards frames to the C++ renderer and escape
/// hatches to the OS via `open`.
struct ShellHost {
    renderer: Rc<Renderer>,
}

impl Host for ShellHost {
    fn publish(&mut self, state: &PublicState, dl: &DisplayList) {
        self.renderer.set_title(&format!("{} — Kitae", state.title));
        self.renderer.set_address(&state.url);
        self.renderer.submit(dl, state.scroll_y);
    }

    fn scroll_updated(&mut self, state: &PublicState) {
        self.renderer
            .set_scroll(state.scroll_y, state.content_height);
    }

    fn set_loading(&mut self, _loading: bool) {
        // The SDL chrome has no loading affordance; the next publish paints.
    }

    fn open_external(&mut self, url: &str) {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
}

/// Widths for layout, measured by the same renderer that paints, so what is
/// laid out matches what is drawn (Helvetica for text, Menlo for pre/code).
struct ShellMeasurer {
    renderer: Rc<Renderer>,
}

impl kitae_layout::TextMeasurer for ShellMeasurer {
    fn measure(&self, text: &str, size: f32, weight: u32, italic: bool, monospace: bool) -> f32 {
        FfiMeasurer::new(&self.renderer).measure(text, size, weight, italic, monospace)
    }

    fn metrics(&self, size: f32, monospace: bool) -> (f32, f32) {
        FfiMeasurer::new(&self.renderer).metrics(size, monospace)
    }
}

fn run_gui(start_url: &str, password: Option<&str>) {
    let Some(renderer) = Renderer::new(WINDOW_W, WINDOW_H, "Kitae — The web, without the noise.")
    else {
        eprintln!("kitae: could not open the SDL2 window");
        std::process::exit(1);
    };
    // Shared between the host (painting), the measurer (widths) and the
    // event loop below; every touch is on the main SDL thread.
    let renderer = Rc::new(renderer);

    let (vw, vh) = renderer.viewport();
    let mut browser = Browser::new(
        Box::new(ShellHost {
            renderer: renderer.clone(),
        }),
        Box::new(ShellMeasurer {
            renderer: renderer.clone(),
        }),
        (vw, vh),
    );
    if let Some(pw) = password {
        // Before navigation the password is just held; the first load of a
        // locked file decodes straight away.
        browser.set_qtsq_password(pw);
    }
    browser.navigate(start_url);

    loop {
        match renderer.poll_event() {
            // The shell painted a frame; brief pause keeps idle CPU near zero
            // without starving the (blocking) event pump.
            None => std::thread::sleep(Duration::from_millis(8)),
            Some(Event::Quit) => break,
            Some(Event::Navigate(input)) => browser.navigate(&input),
            Some(Event::Back) => browser.go(-1),
            Some(Event::Forward) => browser.go(1),
            Some(Event::Reload) => browser.reload(),
            Some(Event::Scroll(dy)) => browser.scroll_by(dy),
            Some(Event::Resize(_, _)) => {
                browser.resize(renderer.viewport().0, renderer.viewport().1)
            }
            // Clicks arrive in page space (the shell adds scroll before
            // handing them over), exactly what the engine hit-tests against.
            Some(Event::Click(x, y)) => browser.click(x, y),
        }
    }
}
