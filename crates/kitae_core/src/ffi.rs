//! C ABI facade over the engine, for hosts that are not Rust programs.
//!
//! Today that host is the Swift macOS app (`app/`): it owns the NSApplication
//! run loop, injects Core Text measurement through a function-pointer vtable
//! (`KitaeMeasurerVtable`), drives the engine with commands, and polls
//! serialized frames. The contract and the frame wire format live in
//! `include/kitae_core.h`.
//!
//! Design notes:
//! * One engine = one OS thread. The [`Browser`] is moved there once and only
//!   that thread touches it afterwards (`SendBrowser` documents why that is
//!   sound); commands arrive through a channel, frames leave through a
//!   mailbox — no locks are ever held across a page load.
//! * The measurer vtable is called synchronously from the engine thread. The
//!   host contract requires the function pointers (and `ctx`) to be callable
//!   from any thread — true for Core Text measurement on macOS.
//! * Escape-hatch links (`kitae:open-external:`, `mailto:`) are queued for
//!   the host to open on its own side (e.g. NSWorkspace); nothing is ever
//!   executed here.

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use kitae_layout::{DisplayList, ResolvedClip, TextMeasurer};
use kitae_net::resolve;
use kitae_style::{ColorScheme, EnvironmentState, FilterFn, FilterList, MediaType};

use crate::agent::{self, HostRaster, PageRasterVtable, SessionState};
use crate::{Browser, PublicState, START_URL};

const FRAME_VERSION: u32 = 20;
const AX_SEMANTICS_MAGIC: u32 = 0x3153_5841;
const AX_SEMANTICS_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Engine thread plumbing
// ---------------------------------------------------------------------------

enum Command {
    Navigate(String),
    Back,
    Forward,
    Reload,
    Scroll(f32),
    Resize(f32, f32),
    Click(f32, f32),
    FocusNext,
    FocusPrev,
    Activate,
    SetInput(String, String),
    Submit(String),
    SelectAll,
    ClearSelection,
    SetHover(Option<String>),
    SetActive(Option<String>),
    Find(String),
    FindNext,
    FindPrev,
    FindClear,
    MediaOpen,
    /// a = playing (0/1), b = position seconds.
    MediaPlay(f32, f32),
    MediaState,
    /// url = password; stores the session QTSQ password and reloads the
    /// current page so a locked viewer unlocks.
    SetPassword(String),
    /// Environment media state (Q6): what `@media (prefers-…)` sees.
    SetEnvironment(EnvironmentState),
    /// The `prefers-*` half of the environment only — what
    /// `KITAE_CMD_SET_ENVIRONMENT` carries. The media type stays as it is,
    /// so an appearance change never drops a print run back to screen.
    SetPreferences(EnvironmentState),
    /// H3: the media environment the page is laid out in (screen / print).
    SetMediaType(MediaType),
    /// SCRIPTING_DECISION.md: enable/disable page JavaScript (next load).
    SetPageJs(bool),
    /// Save the current page as a device-encrypted QTSQ archive at `url`.
    /// The outcome arrives later on the notifications queue.
    SavePage(String),
    /// Phase 2 (`live`): drain the mounted kitae_live socket once —
    /// embedders without the idle-poll loop drive it explicitly.
    LivePoll,
    /// Phase 2 (`live`): answer through the notifications queue with
    /// "live\t1" / "live\t0" (page is / isn't a connected Live session).
    LiveState,
    /// A6: Esc — close the topmost open modal dialog.
    Escape,
    /// A7: step the focused control's value by `dir` (±1) — range/number
    /// move one `step`, select moves the selection one option.
    StepFocus(i32),
    /// A11a: insert `text` at the caret of the focused editable control.
    TypeText(String),
    /// A11a: delete the char before the caret.
    Backspace,
    /// A11a: delete the char after the caret.
    DeleteForward,
    /// A11a: move the caret by `delta` chars (negative = left).
    MoveCaret(isize),
    /// A11a: place the caret at an absolute char offset (clamped).
    SetCaret(usize),
    ExtendCaret(isize),
    SelectionBegin(usize),
    SelectionExtend(usize),
    SelectionBeginPoint(f32, f32),
    SelectionExtendPoint(f32, f32),
    SelectionEnd,
    CompositionUpdate(String),
    CompositionCommit(Option<String>),
    CompositionCancel,
    /// H2: the host's open-panel answer for a clicked `input[type=file]` —
    /// element id (or name) plus the picked absolute paths.
    SetFileFiles(String, Vec<String>),
    /// H6: the host's answer to an `auth\t` notification — the sign-in
    /// user name and password ("user\npass" over the wire). Stores the
    /// session credentials and re-fetches the challenged URL.
    SetAuth(String, String),
    /// H5: hit-test the last display list at a page point (a = x, b = y);
    /// a hit narrates `inspect\t{node}` on the notifications queue.
    InspectPick(f32, f32),
    /// H5: narrate the element + computed-style report for `node` (a =
    /// node id) as `inspectinfo\t{lines…}` on the notifications queue.
    InspectInfo(usize),
    SetPrivacy {
        dnt: bool,
        block_third_party_cookies: bool,
        https_only: bool,
        warn_insecure_http: bool,
    },
    SetSearchProvider(u8),
    ResolveInsecureNavigation(bool),
    /// B4: install the host's rasterizer (the app links kitae_headless and
    /// hands over its vtable; see [`PageRasterVtable`]).
    SetRaster(SendRaster),
    /// B4: run one verb, given as its wire JSON, on this engine's browser.
    /// The answer arrives on the notifications queue as `agent-result`.
    AgentAct(String),
    /// B4: reply with the whole activity journal as `agent-journal`.
    AgentJournal,
    /// B4: the user approved `name` for this session (`*` = everything).
    AgentGrant(String),
    /// B4: forget every grant.
    AgentRevokeGrants,
    /// B4: a script declared these verbs — comma/space separated names.
    AgentDeclare(String),
}

impl Command {
    /// The commands that need the agent state beside the browser.
    fn is_agent(&self) -> bool {
        matches!(
            self,
            Command::SetRaster(_)
                | Command::AgentAct(_)
                | Command::AgentJournal
                | Command::AgentGrant(_)
                | Command::AgentRevokeGrants
                | Command::AgentDeclare(_)
        )
    }
}

/// A [`PageRasterVtable`] on its way to the engine thread. The function
/// pointers themselves carry no thread affinity — the host contract is that
/// they may be called from the engine thread — so the wrapper only says what
/// the vtable's own documentation already requires.
struct SendRaster(PageRasterVtable);

unsafe impl Send for SendRaster {}

pub struct Engine {
    tx: mpsc::Sender<Command>,
    frame: Arc<Mutex<Option<Vec<u8>>>>,
    loading: Arc<Mutex<bool>>,
    externals: Arc<Mutex<VecDeque<String>>>,
    /// `save_finished` outcomes: "1\t{path}" on success, "0\t{message}" on
    /// failure (tabs in the message are flattened to spaces).
    notifications: Arc<Mutex<VecDeque<String>>>,
}

/// The `Browser` (and its boxed host + measurer) is moved to the engine
/// thread and only that thread touches it afterwards — the channel and the
/// mutex mailboxes are the only cross-thread surfaces. The measurer's vtable
/// must be callable from any thread (host contract; Core Text is).
struct SendBrowser(Browser);

// Safety: see the comment above. Confined here; nothing else in the crate
// relies on cross-thread access.
unsafe impl Send for SendBrowser {}

impl Engine {
    fn new(width: f32, height: f32, vt: KitaeMeasurerVtable) -> Engine {
        let (tx, rx) = mpsc::channel::<Command>();
        let frame = Arc::new(Mutex::new(None));
        let loading = Arc::new(Mutex::new(false));
        let externals = Arc::new(Mutex::new(VecDeque::new()));
        let notifications = Arc::new(Mutex::new(VecDeque::new()));

        let host = MailboxHost {
            frame: frame.clone(),
            loading: loading.clone(),
            externals: externals.clone(),
            notifications: notifications.clone(),
        };
        let measurer = CbMeasurer { vt };
        let mut browser = Browser::new(Box::new(host), Box::new(measurer), (width, height));
        // Phase 0: the app drives loads through a UI that keeps polling,
        // so top-level http(s) loads ride the shared fetch pool and
        // finish on the idle tick below. Local schemes (about:, file:,
        // data:) still load inline, and so do POST submits.
        browser.set_background_fetch(true);

        // Wrap before the closure. The `use` defeats edition-2021 disjoint
        // capture (which would otherwise capture `browser.0`, a non-Send
        // `Browser`, directly).
        let browser = SendBrowser(browser);
        let _ = &browser;
        // S-5 (SECURITY_AUDIT.md): the panic guard reports through the same
        // notifications queue the host already drains.
        let crash_notes = notifications.clone();
        // B4: the agent commands answer on that queue too.
        let agent_notes = notifications.clone();
        // The engine thread answers LiveState through this queue (the host
        // keeps its own Arc — the same one).
        #[cfg(feature = "live")]
        let notes = notifications.clone();
        std::thread::Builder::new()
            .name("kitae-engine".into())
            .spawn(move || {
                let mut browser = browser;
                // B4: the agent plane's state for this engine — the policy,
                // journal and rasterizer. The app drives the *same* verbs the
                // servers do, over the browser that paints its window.
                let mut agent = SessionState::new();
                // S-5/H8: even the startup navigation is guarded — a panic
                // before the loop would leave a dead tab with no page.
                run_protected(&crash_notes, "startup", &mut browser.0, |b| {
                    b.navigate(START_URL);
                });
                loop {
                    // Block until a command arrives — but wake up regularly
                    // so a mounted Live page can poll its socket (Phase 2).
                    // The 2ms idle tick costs nothing when no page is live.
                    let cmd = match rx.recv_timeout(std::time::Duration::from_millis(2)) {
                        Ok(cmd) => cmd,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            // S-5: a panic in a tick must not kill the
                            // thread — recover, report, keep serving.
                            let b = &mut browser.0;
                            run_protected(&crash_notes, "idle tick", b, |b| {
                                #[cfg(feature = "live")]
                                b.poll_live();
                                // B1: drive CSS transitions while any run — the
                                // browser relayouts + republishes when a frame
                                // of interpolation is due; a no-op otherwise.
                                b.tick_transitions();
                                // H1: push download-registry changes to the
                                // host (progress coalesced to ~100 ms).
                                b.poll_downloads();
                                // Phase 0: apply completed background (pooled)
                                // top-level loads. `load` returned right after
                                // submitting; the pipeline replays here.
                                b.poll_fetches();
                                // SCRIPTING_DECISION.md: page event loop —
                                // timers, fetch settle, queued navigations.
                                b.poll_page_js();
                            });
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    // B4: the agent commands need the session state beside
                    // the browser, so they take the guard that has both.
                    if cmd.is_agent() {
                        let notes = agent_notes.clone();
                        run_agent_guarded(
                            &crash_notes,
                            "agent command",
                            &mut browser.0,
                            &mut agent,
                            |b, state| handle_agent_command(cmd, &notes, b, state),
                        );
                        continue;
                    }
                    // S-5: every command runs behind the panic guard — a
                    // crash costs one notification plus a crash page, not
                    // the tab.
                    run_protected(&crash_notes, "command", &mut browser.0, |b| match cmd {
                        // B4: handled above, where the agent state is in reach.
                        Command::SetRaster(_)
                        | Command::AgentAct(_)
                        | Command::AgentJournal
                        | Command::AgentGrant(_)
                        | Command::AgentRevokeGrants
                        | Command::AgentDeclare(_) => {}
                        Command::Navigate(url) => b.navigate(&url),
                        Command::Back => b.go(-1),
                        Command::Forward => b.go(1),
                        Command::Reload => b.reload(),
                        Command::Scroll(dy) => b.scroll_by(dy),
                        Command::Resize(w, h) => b.resize(w, h),
                        Command::Click(x, y) => b.click(x, y),
                        Command::FocusNext => {
                            b.focus_next();
                        }
                        Command::FocusPrev => {
                            b.focus_prev();
                        }
                        Command::Activate => {
                            b.activate_focused();
                        }
                        Command::SetInput(id, v) => {
                            b.set_input_value(&id, &v);
                        }
                        Command::Submit(id) => {
                            b.submit_form(&id);
                        }
                        Command::SelectAll => {
                            b.select_all();
                        }
                        Command::ClearSelection => b.clear_selection(),
                        Command::SetHover(id) => {
                            b.set_hover_id(id.as_deref());
                        }
                        Command::SetActive(id) => {
                            b.set_active_id(id.as_deref());
                        }
                        Command::Find(query) => {
                            b.set_find_query(&query);
                        }
                        Command::FindNext => b.find_next(),
                        Command::FindPrev => b.find_prev(),
                        Command::FindClear => b.clear_find(),
                        Command::MediaOpen => b.media_open(),
                        Command::MediaPlay(playing, position) => {
                            b.media_play(playing != 0.0, position);
                        }
                        Command::MediaState => {
                            // Reply is the next published frame — the state
                            // query just forces a fresh mirror out.
                            b.media_open();
                        }
                        Command::SetPassword(pw) => {
                            b.set_qtsq_password(&pw);
                        }
                        Command::SetEnvironment(env) => {
                            b.set_environment(env);
                        }
                        Command::SetPreferences(env) => {
                            b.set_preferences(&env);
                        }
                        Command::SetMediaType(media) => {
                            b.set_media_type(media);
                        }
                        Command::SetPageJs(on) => {
                            b.set_page_js(on);
                        }
                        Command::SavePage(path) => {
                            // Outcomes reach the embedder via the
                            // notifications queue, not this send's return.
                            let _ = b.save_page_qtsq(&path);
                        }
                        Command::LivePoll => {
                            #[cfg(feature = "live")]
                            b.poll_live();
                        }
                        Command::LiveState => {
                            #[cfg(feature = "live")]
                            {
                                let msg = if b.is_live_connected() {
                                    "live\t1".to_string()
                                } else {
                                    "live\t0".to_string()
                                };
                                lock_poison_ok(&notes).push_back(msg);
                            }
                        }
                        Command::Escape => b.escape_key(),
                        Command::StepFocus(d) => {
                            b.step_focused(d);
                        }
                        // A11a: caret + text editing for focused form controls.
                        Command::TypeText(t) => {
                            b.type_into_focused(&t);
                        }
                        Command::Backspace => {
                            b.backspace_focused();
                        }
                        Command::DeleteForward => {
                            b.delete_forward();
                        }
                        Command::MoveCaret(d) => {
                            b.move_caret(d);
                        }
                        Command::SetCaret(o) => {
                            b.set_caret(o);
                        }
                        Command::ExtendCaret(d) => {
                            b.extend_caret(d);
                        }
                        Command::SelectionBegin(o) => {
                            b.begin_selection(o);
                        }
                        Command::SelectionExtend(o) => {
                            b.extend_selection(o);
                        }
                        Command::SelectionBeginPoint(x, y) => {
                            b.begin_selection_at_point(x, y);
                        }
                        Command::SelectionExtendPoint(x, y) => {
                            b.extend_selection_at_point(x, y);
                        }
                        Command::SelectionEnd => {
                            b.end_selection();
                        }
                        Command::CompositionUpdate(text) => {
                            b.update_marked_text(&text);
                        }
                        Command::CompositionCommit(text) => match text {
                            Some(text) => {
                                b.commit_composition_text(&text);
                            }
                            None => {
                                b.commit_composition();
                            }
                        },
                        Command::CompositionCancel => {
                            b.cancel_composition();
                        }
                        Command::SetFileFiles(id, paths) => {
                            b.set_file_files(&id, &paths);
                        }
                        // H6: store the sign-in credentials and re-fetch.
                        Command::SetAuth(user, pass) => {
                            b.set_auth(&user, &pass);
                        }
                        // H5: pick the element under the point / narrate its
                        // report; both answers ride the notifications queue.
                        Command::InspectPick(x, y) => {
                            b.pick_inspect(x, y);
                        }
                        Command::InspectInfo(node) => {
                            b.report_inspect_info(node as u32);
                        }
                        Command::SetPrivacy {
                            dnt,
                            block_third_party_cookies,
                            https_only,
                            warn_insecure_http,
                        } => {
                            kitae_net::set_dnt_enabled(dnt);
                            kitae_net::set_block_third_party_cookies(block_third_party_cookies);
                            kitae_net::set_https_only(https_only);
                            kitae_net::set_warn_insecure_http(warn_insecure_http);
                        }
                        Command::SetSearchProvider(index) => {
                            let provider = match index {
                                1 => kitae_net::SearchProvider::Wiby,
                                2 => kitae_net::SearchProvider::Marginalia,
                                3 => kitae_net::SearchProvider::Wikipedia,
                                _ => kitae_net::SearchProvider::DuckDuckGo,
                            };
                            kitae_net::set_search_provider(provider);
                        }
                        Command::ResolveInsecureNavigation(allow) => {
                            b.resolve_insecure_navigation(allow);
                        }
                    });
                }
            })
            .expect("kitae_core: could not spawn the engine thread");

        Engine {
            tx,
            frame,
            loading,
            externals,
            notifications,
        }
    }
}

/// S-5/H8: run `body`, converting a panic into a crash notification AND a
/// visible crash page instead of unwinding out of the engine thread (a dead
/// thread = a silently frozen tab). The browser keeps whatever state it had;
/// [`Browser::show_crash_page`] replaces only what the user sees, the crash
/// page's Reload link refetches, and the next command continues from there.
///
/// Isolation decision (parity H8): in-process *reload-on-panic*. The engine
/// already runs one thread per tab, so a crash is bounded to its tab; the
/// guard turns "thread dies" into "tab resets to a crash page with a reload
/// affordance". A multi-process model would isolate further but contradicts
/// the embedder architecture — a static library, no IPC layer, and every
/// host (SDL shell, Swift app, tests) would need a process manager. Rejected;
/// revisitable if an embedder needs OS-level isolation.
fn run_protected(
    notes: &Arc<Mutex<VecDeque<String>>>,
    what: &str,
    browser: &mut Browser,
    body: impl FnOnce(&mut Browser),
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(browser)));
    if result.is_err() {
        eprintln!("kitae_core: recovered from a panic in the engine {what}");
        lock_poison_ok(notes).push_back(format!("crash\tpanic recovered in the engine {what}"));
        // The crash page itself is an ordinary render; if THAT panics too,
        // give up quietly — the notification already went out and the thread
        // must keep serving commands regardless.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            browser.show_crash_page(what);
        }));
    }
}

/// The [`run_protected`] guard for commands that also touch the agent state:
/// a panic inside a verb costs one notification and a crash page, not the
/// tab, and the state stays usable because the borrow ends with the closure.
fn run_agent_guarded(
    notes: &Arc<Mutex<VecDeque<String>>>,
    what: &str,
    browser: &mut Browser,
    state: &mut SessionState,
    body: impl FnOnce(&mut Browser, &mut SessionState),
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(browser, state)));
    if result.is_err() {
        eprintln!("kitae_core: recovered from a panic in the engine {what}");
        lock_poison_ok(notes).push_back(format!("crash\tpanic recovered in the engine {what}"));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            browser.show_crash_page(what);
        }));
    }
}

/// B4: the agent's commands, run on the engine thread. Every answer goes out
/// on the notifications queue — the C ABI cannot return a value to the caller
/// that sent the command, and the app is polling that queue anyway.
fn handle_agent_command(
    cmd: Command,
    notes: &Arc<Mutex<VecDeque<String>>>,
    browser: &mut Browser,
    state: &mut SessionState,
) {
    let push = |message: String| lock_poison_ok(notes).push_back(message);
    match cmd {
        Command::SetRaster(vt) => {
            // Safety: the vtable contract in `kitae_core.h` — both functions
            // are callable from this thread with a read-only browser, and the
            // context stays valid until `free` runs.
            state.set_raster(std::sync::Arc::new(unsafe { HostRaster::new(vt.0) }));
            push("agent-raster\t1".to_string());
        }
        Command::AgentAct(request) => {
            let last = state.journal().last().map(|row| row.seq).unwrap_or(0);
            let reply = agent::run_wire(state, browser, &request);
            let rows: Vec<agent::JournalRow> = state
                .journal()
                .iter()
                .filter(|row| row.seq > last)
                .cloned()
                .collect();
            // The reply carries the rows *this* act added, so the activity
            // panel updates from the answer it is already receiving; opening
            // the panel asks for the whole journal separately.
            let reply = match kitae_json::parse(&reply) {
                Ok(kitae_json::Value::Obj(mut pairs)) => {
                    pairs.push(("journal".to_string(), agent::journal_json(&rows)));
                    kitae_json::Value::Obj(pairs).to_json()
                }
                _ => reply,
            };
            push(format!("agent-result\t{reply}"));
        }
        Command::AgentJournal => {
            push(format!(
                "agent-journal\t{}",
                agent::journal_json(state.journal()).to_json()
            ));
            push(format!("agent-policy\t{}", state.policy_json().to_json()));
        }
        Command::AgentGrant(name) => {
            state.grant(&name);
            push(format!("agent-policy\t{}", state.policy_json().to_json()));
        }
        Command::AgentRevokeGrants => {
            state.revoke_grants();
            push(format!("agent-policy\t{}", state.policy_json().to_json()));
        }
        Command::AgentDeclare(list) => {
            let verbs: Vec<String> = list
                .split([',', ' ', '\n', '\t'])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(String::from)
                .collect();
            state.declare(&verbs);
            push(format!("agent-policy\t{}", state.policy_json().to_json()));
        }
        _ => {}
    }
}

/// H8: lock a mailbox mutex even when a previous holder panicked while
/// holding it. A poisoned guard would panic the engine thread (killing the
/// tab for real) or the host thread (aborting the app) on the *next* call —
/// recovery keeps the tab painting stale-but-live frames after any crash.
fn lock_poison_ok<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// S-5: the same guard for `extern "C"` entry points — a panic unwinding
/// through the C ABI is UB (Rust aborts the process). Returns `None` when
/// the body panicked; C callers see the documented null/0 "absent" answer.
fn ffi_catch<T>(body: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).ok()
}

struct MailboxHost {
    frame: Arc<Mutex<Option<Vec<u8>>>>,
    loading: Arc<Mutex<bool>>,
    externals: Arc<Mutex<VecDeque<String>>>,
    notifications: Arc<Mutex<VecDeque<String>>>,
}

impl crate::Host for MailboxHost {
    fn publish(&mut self, state: &PublicState, dl: &DisplayList) {
        let buf = serialize_frame(state, dl);
        *lock_poison_ok(&self.frame) = Some(buf);
    }

    fn scroll_updated(&mut self, _state: &PublicState) {
        // Embedded hosts scroll their view locally (view-local scroll in the
        // Swift app), so a scroll-only update needs no new frame.
    }

    fn set_loading(&mut self, loading: bool) {
        *lock_poison_ok(&self.loading) = loading;
    }

    fn open_external(&mut self, url: &str) {
        lock_poison_ok(&self.externals).push_back(url.to_string());
    }

    fn save_finished(&mut self, path: &str, error: Option<&str>) {
        let msg = match error {
            None => format!("1\t{path}"),
            Some(e) => format!("0\t{}", e.replace('\t', " ")),
        };
        lock_poison_ok(&self.notifications).push_back(msg);
    }

    fn notify(&mut self, message: &str) {
        // H1: download progress and other engine-to-app messages share the
        // notification queue the save flow uses; the app dispatches on the
        // tab-separated prefix.
        lock_poison_ok(&self.notifications).push_back(message.to_string());
    }
}

/// [`TextMeasurer`] calling into the host through the vtable.
struct CbMeasurer {
    vt: KitaeMeasurerVtable,
}

impl TextMeasurer for CbMeasurer {
    fn measure(&self, text: &str, size: f32, weight: u32, italic: bool, monospace: bool) -> f32 {
        self.measure_family(text, size, weight, italic, monospace, "")
    }

    fn metrics(&self, size: f32, monospace: bool) -> (f32, f32) {
        self.metrics_family(size, monospace, "")
    }

    // The shell paints runs with the run's own family (Q11), so the word
    // widths layout actually positions with must come from the same face.
    // Falling back to the family-blind default measured every named-family
    // run in the host default font — painted glyphs then drifted wider than
    // the layout positions and inter-word gaps collapsed.
    fn measure_run_family(
        &self,
        text: &str,
        size: f32,
        weight: u32,
        italic: bool,
        monospace: bool,
        family: &str,
    ) -> f32 {
        self.measure_family(text, size, weight, italic, monospace, family)
    }

    fn measure_family(
        &self,
        text: &str,
        size: f32,
        weight: u32,
        italic: bool,
        monospace: bool,
        family: &str,
    ) -> f32 {
        // Interior NULs cannot cross the ABI; drop them.
        let cleaned: String = text.chars().filter(|&c| c != '\0').collect();
        let Ok(text) = CString::new(cleaned) else {
            return 0.0;
        };
        let Ok(family) = CString::new(family.replace('\0', "")) else {
            return 0.0;
        };
        (self.vt.measure)(
            self.vt.ctx,
            text.as_ptr(),
            size,
            weight,
            italic as c_int,
            monospace as c_int,
            family.as_ptr(),
        )
    }

    fn metrics_family(&self, size: f32, monospace: bool, family: &str) -> (f32, f32) {
        let Ok(family) = CString::new(family.replace('\0', "")) else {
            return (0.0, 0.0);
        };
        let (mut ascent, mut descent) = (0.0f32, 0.0f32);
        (self.vt.metrics)(
            self.vt.ctx,
            size,
            monospace as c_int,
            &mut ascent,
            &mut descent,
            family.as_ptr(),
        );
        (ascent, descent)
    }

    fn ex_px(&self, size: f32, weight: u32, italic: bool, monospace: bool, family: &str) -> f32 {
        let Some(f) = self.vt.x_height else {
            return size * 0.5;
        };
        let Ok(family) = CString::new(family.replace('\0', "")) else {
            return size * 0.5;
        };
        f(
            self.vt.ctx,
            size,
            weight,
            italic as c_int,
            monospace as c_int,
            family.as_ptr(),
        )
    }
}

// ---------------------------------------------------------------------------
// Frame serialization (little-endian; see include/kitae_core.h)
// ---------------------------------------------------------------------------

fn serialize_frame(state: &PublicState, dl: &DisplayList) -> Vec<u8> {
    let mut v = Vec::with_capacity(8192);

    v.extend_from_slice(&FRAME_VERSION.to_le_bytes()); // frame format version (v15: + sticky side-list, v16: + per-item filter chains, v17: + scroll containers, v18: + per-item clips/blend, v19: + per-text blur radius, v20: + per-media-item controls flag byte)

    let mut flags = 0u32;
    if state.can_go_back {
        flags |= 1;
    }
    if state.can_go_forward {
        flags |= 2;
    }
    if state.print_media {
        flags |= 4;
    }
    v.extend_from_slice(&flags.to_le_bytes());

    v.extend_from_slice(&state.scroll_y.to_le_bytes());
    v.extend_from_slice(&state.content_height.to_le_bytes());
    v.extend_from_slice(&dl.page_background.to_le_bytes());
    put_str(&mut v, &state.url);
    put_str(&mut v, &state.title);

    v.extend_from_slice(&(dl.rects.len() as u32).to_le_bytes());
    v.extend_from_slice(&(dl.texts.len() as u32).to_le_bytes());
    v.extend_from_slice(&(dl.links.len() as u32).to_le_bytes());
    v.extend_from_slice(&(dl.images.len() as u32).to_le_bytes());

    for r in &dl.rects {
        v.extend_from_slice(&r.x.to_le_bytes());
        v.extend_from_slice(&r.y.to_le_bytes());
        v.extend_from_slice(&r.w.to_le_bytes());
        v.extend_from_slice(&r.h.to_le_bytes());
        v.extend_from_slice(&r.color.to_le_bytes());
        // v7: per-corner radii (px, box-width basis already applied where
        // authored as % — painters use them directly) + paint descriptor.
        let radii = r.radius.resolve(16.0, 16.0, r.w.max(0.0));
        for corner in radii {
            v.extend_from_slice(&corner.to_le_bytes());
        }
        match &r.paint {
            kitae_layout::RectPaint::Solid => v.push(0),
            kitae_layout::RectPaint::LinearGradient { angle_deg, stops } => {
                v.push(1);
                v.extend_from_slice(&angle_deg.to_le_bytes());
                put_stops(&mut v, stops);
            }
            kitae_layout::RectPaint::RadialGradient {
                cx,
                cy,
                radius,
                stops,
            } => {
                v.push(2);
                v.extend_from_slice(&cx.to_le_bytes());
                v.extend_from_slice(&cy.to_le_bytes());
                match radius {
                    Some(px) => {
                        v.push(1);
                        v.extend_from_slice(&px.to_le_bytes());
                    }
                    None => v.push(0),
                }
                put_stops(&mut v, stops);
            }
            kitae_layout::RectPaint::ConicGradient {
                angle_deg,
                cx,
                cy,
                stops,
            } => {
                // B5: tag 4 — angle, fractional center, sweep stops.
                v.push(4);
                v.extend_from_slice(&angle_deg.to_le_bytes());
                v.extend_from_slice(&cx.to_le_bytes());
                v.extend_from_slice(&cy.to_le_bytes());
                put_stops(&mut v, stops);
            }
            kitae_layout::RectPaint::Image(url) => {
                v.push(3);
                let abs = resolve(&state.resource_base_url, url).unwrap_or_else(|| url.clone());
                put_str(&mut v, &abs);
            }
        }
    }

    for t in &dl.texts {
        v.extend_from_slice(&t.x.to_le_bytes());
        v.extend_from_slice(&t.y.to_le_bytes());
        v.extend_from_slice(&t.size.to_le_bytes());
        v.extend_from_slice(&t.weight.to_le_bytes());
        v.extend_from_slice(&(t.italic as u32).to_le_bytes());
        v.extend_from_slice(&(t.monospace as u32).to_le_bytes());
        v.extend_from_slice(&t.color.to_le_bytes());
        // v10: letter-spacing (Q3). Painters draw per-char at measure(char)
        // + spacing advances; 0.0 means draw the whole run normally.
        v.extend_from_slice(&t.letter_spacing.to_le_bytes());
        // v14: first non-generic `font-family` (Q11) — the host resolves the
        // same family the measurer did so widths and glyphs agree.
        put_str(&mut v, &t.family);
        put_str(&mut v, &t.text);
    }

    for l in &dl.links {
        v.extend_from_slice(&l.x.to_le_bytes());
        v.extend_from_slice(&l.y.to_le_bytes());
        v.extend_from_slice(&l.w.to_le_bytes());
        v.extend_from_slice(&l.h.to_le_bytes());
        // Display-list hrefs may be relative; hand the host absolute ones.
        let abs = resolve(&state.resource_base_url, &l.href).unwrap_or_else(|| l.href.clone());
        put_str(&mut v, &abs);
    }

    for im in &dl.images {
        v.extend_from_slice(&im.x.to_le_bytes());
        v.extend_from_slice(&im.y.to_le_bytes());
        v.extend_from_slice(&im.w.to_le_bytes());
        v.extend_from_slice(&im.h.to_le_bytes());
        let fit = match im.fit {
            kitae_style::ObjectFit::Fill => 0u32,
            kitae_style::ObjectFit::Contain => 1u32,
            kitae_style::ObjectFit::Cover => 2u32,
            kitae_style::ObjectFit::None => 3u32,
            kitae_style::ObjectFit::ScaleDown => 4u32,
        };
        v.extend_from_slice(&fit.to_le_bytes());
        let abs = resolve(&state.resource_base_url, &im.src).unwrap_or_else(|| im.src.clone());
        put_str(&mut v, &abs);
        put_str(&mut v, &im.alt);
        // v4: decoded-raster reference (0 = placeholder box). The host
        // resolves the id against kitae_qtsq_raster (J3 side-table).
        v.extend_from_slice(&im.payload.unwrap_or(0).to_le_bytes());
        // v13: animated flag (Q8) — the raster is a multi-frame animation;
        // hosts pull frames via kitae_raster_frames/kitae_raster_frame.
        v.push(im.animated as u8);
    }

    // v3: interaction state (empty string = none).
    put_str(&mut v, state.focused_id.as_deref().unwrap_or(""));
    put_str(&mut v, state.selection.as_deref().unwrap_or(""));

    // v5: media section (J4). u8 present flag; when set: kind, duration,
    // rate, channels, playing, position, url, caf_path. All little-endian.
    match &state.media {
        None => v.push(0),
        Some(m) => {
            v.push(1);
            v.push(match m.kind {
                crate::MediaKind::Audio => 0u8,
                crate::MediaKind::Video => 1u8,
            });
            v.extend_from_slice(&m.duration.to_le_bytes());
            v.extend_from_slice(&m.rate.to_le_bytes());
            v.push(m.channels);
            v.push(m.playing as u8);
            v.extend_from_slice(&m.position.to_le_bytes());
            put_str(&mut v, &m.url);
            put_str(&mut v, &m.caf_path);
        }
    }

    // v6: media items (J5). Count + per-item geometry/src/transport, in page
    // order. The frame image is the sibling DisplayImage with the
    // `\0media:{id}` sentinel src (its payload carries the stepped raster).
    v.extend_from_slice(&(dl.media.len() as u32).to_le_bytes());
    for m in &dl.media {
        v.extend_from_slice(&m.id.to_le_bytes());
        v.extend_from_slice(&m.x.to_le_bytes());
        v.extend_from_slice(&m.y.to_le_bytes());
        v.extend_from_slice(&m.w.to_le_bytes());
        v.extend_from_slice(&m.h.to_le_bytes());
        let abs = resolve(&state.resource_base_url, &m.src).unwrap_or_else(|| m.src.clone());
        put_str(&mut v, &abs);
        v.push(match m.kind {
            kitae_layout::MediaKind::Audio => 0u8,
            kitae_layout::MediaKind::Video => 1u8,
            _ => 1u8,
        });
        v.push(m.state.0 as u8);
        v.extend_from_slice(&m.state.1.to_le_bytes());
        // v20 (F4): transport-chrome flag — bit0 set = the element carries
        // `controls`, so the host paints the builtin chrome band.
        v.push(m.controls as u8);
    }

    // v7: box shadows, parents before children (paint under their boxes).
    v.extend_from_slice(&(dl.shadows.len() as u32).to_le_bytes());
    for s in &dl.shadows {
        v.extend_from_slice(&s.x.to_le_bytes());
        v.extend_from_slice(&s.y.to_le_bytes());
        v.extend_from_slice(&s.w.to_le_bytes());
        v.extend_from_slice(&s.h.to_le_bytes());
        let radii = s.radius.resolve(16.0, 16.0, s.w.max(0.0));
        for corner in radii {
            v.extend_from_slice(&corner.to_le_bytes());
        }
        v.extend_from_slice(&s.color.to_le_bytes());
        v.extend_from_slice(&s.blur.to_le_bytes());
        v.extend_from_slice(&s.spread.to_le_bytes());
        v.extend_from_slice(&s.offset_x.to_le_bytes());
        v.extend_from_slice(&s.offset_y.to_le_bytes());
        v.push(s.inset as u8);
    }

    // v8: find-in-page (P1). Match count, active index (u32::MAX = none),
    // then per match in page order: rect count + page-space rects. The
    // embedder overlays these (dim for matches, strong for the active one).
    v.extend_from_slice(&(state.find_matches.len() as u32).to_le_bytes());
    v.extend_from_slice(
        &state
            .find_active
            .map(|a| a as u32)
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for m in &state.find_matches {
        v.extend_from_slice(&(m.rects.len() as u32).to_le_bytes());
        for r in &m.rects {
            v.extend_from_slice(&r.x.to_le_bytes());
            v.extend_from_slice(&r.y.to_le_bytes());
            v.extend_from_slice(&r.w.to_le_bytes());
            v.extend_from_slice(&r.h.to_le_bytes());
        }
    }

    // v8: accessibility tree (P2). Pre-order flattening: node count, then
    // per node id + role wire id + label + page-space rect + parent index
    // (u32::MAX = root), then the focused AX id ("" = unfocused/unknown).
    // Hosts that skip a11y stop reading after the find section.
    let mut flat: Vec<(&crate::AxNode, u32)> = Vec::new();
    let mut stack: Vec<(&crate::AxNode, u32)> = state
        .ax_tree
        .iter()
        .map(|n| (n, u32::MAX))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    while let Some((node, parent)) = stack.pop() {
        let idx = flat.len() as u32;
        flat.push((node, parent));
        for child in node.children.iter().rev() {
            stack.push((child, idx));
        }
    }
    v.extend_from_slice(&(flat.len() as u32).to_le_bytes());
    for (node, parent) in &flat {
        put_str(&mut v, &node.id);
        v.push(node.role.wire_id());
        put_str(&mut v, &node.label);
        v.extend_from_slice(&node.x.to_le_bytes());
        v.extend_from_slice(&node.y.to_le_bytes());
        v.extend_from_slice(&node.w.to_le_bytes());
        v.extend_from_slice(&node.h.to_le_bytes());
        v.extend_from_slice(&parent.to_le_bytes());
    }
    put_str(&mut v, state.ax_focused.as_deref().unwrap_or(""));
    v.push(state.qtsq_locked as u8); // v9
    v.push(state.qtsq_sig.wire_id()); // v11: QtsqSigState (0 unsigned, 1 valid, 2 invalid, 3 unverified)

    // v12: item transforms (Q2). Sparse tail — only items whose enclosing
    // CSS box carries a non-identity transform, as (kind, index, 2×3
    // row-major matrix). Kind 4 carries link transforms while preserving v20;
    // readers that predate it ignore the fully consumed unknown entry.
    let mut mats: Vec<(u8, u32, [f32; 6])> = Vec::new();
    for (i, r) in dl.rects.iter().enumerate() {
        if let Some(m) = r.transform {
            mats.push((0, i as u32, m.0));
        }
    }
    for (i, t) in dl.texts.iter().enumerate() {
        if let Some(m) = t.transform {
            mats.push((1, i as u32, m.0));
        }
    }
    for (i, im) in dl.images.iter().enumerate() {
        if let Some(m) = im.transform {
            mats.push((2, i as u32, m.0));
        }
    }
    for (i, s) in dl.shadows.iter().enumerate() {
        if let Some(m) = s.transform {
            mats.push((3, i as u32, m.0));
        }
    }
    for (i, link) in dl.links.iter().enumerate() {
        if let Some(m) = link.transform {
            mats.push((4, i as u32, m.0));
        }
    }
    v.extend_from_slice(&(mats.len() as u32).to_le_bytes());
    for (kind, idx, m) in mats {
        v.push(kind);
        v.extend_from_slice(&idx.to_le_bytes());
        for f in m {
            v.extend_from_slice(&f.to_le_bytes());
        }
    }

    // v15: sticky side-list (Q5). The host shifts these boxes at paint time
    // (and the engine's link hit-test matches) via StickyItem::shift —
    // layout paints them at their flow position.
    v.extend_from_slice(&(dl.stickies.len() as u32).to_le_bytes());
    for s in &dl.stickies {
        for (start, count) in [s.rect, s.text, s.link, s.image, s.media, s.shadow] {
            v.extend_from_slice(&start.to_le_bytes());
            v.extend_from_slice(&count.to_le_bytes());
        }
        v.extend_from_slice(&s.base_y.to_le_bytes());
        v.extend_from_slice(&s.height.to_le_bytes());
        v.extend_from_slice(&s.container_top.to_le_bytes());
        v.extend_from_slice(&s.container_bottom.to_le_bytes());
        v.extend_from_slice(&s.top_offset.to_le_bytes());
        v.extend_from_slice(&s.bottom_offset.to_le_bytes());
        v.push(s.has_top as u8);
        v.push(s.has_bottom as u8);
    }

    // v16: per-item `filter` chain. Sparse tail — only items whose
    // enclosing box carries a non-empty `filter`, as
    // (kind, index, function-count, function[]). The host applies the
    // chain to the backing CALayer via CIFilter.
    let mut filter_entries: Vec<(u8, u32, &FilterList)> = Vec::new();
    for (i, r) in dl.rects.iter().enumerate() {
        if let Some(fl) = r.filters.as_ref().filter(|f| !f.0.is_empty()) {
            filter_entries.push((0, i as u32, fl));
        }
    }
    for (i, t) in dl.texts.iter().enumerate() {
        if let Some(fl) = t.filters.as_ref().filter(|f| !f.0.is_empty()) {
            filter_entries.push((1, i as u32, fl));
        }
    }
    for (i, im) in dl.images.iter().enumerate() {
        if let Some(fl) = im.filters.as_ref().filter(|f| !f.0.is_empty()) {
            filter_entries.push((2, i as u32, fl));
        }
    }
    for (i, s) in dl.shadows.iter().enumerate() {
        if let Some(fl) = s.filters.as_ref().filter(|f| !f.0.is_empty()) {
            filter_entries.push((3, i as u32, fl));
        }
    }
    v.extend_from_slice(&(filter_entries.len() as u32).to_le_bytes());
    for (kind, idx, fl) in &filter_entries {
        v.push(*kind);
        v.extend_from_slice(&idx.to_le_bytes());
        v.extend_from_slice(&(fl.0.len() as u32).to_le_bytes());
        for f in &fl.0 {
            put_filter_fn(&mut v, f);
        }
    }

    // v17: scroll containers (B2). Each entry: (node:u32, x:f32, y:f32,
    // w:f32, h:f32, content_h:f32). The host uses this to paint a
    // scrollbar and route wheel events back through the engine.
    v.extend_from_slice(&(dl.scroll_containers.len() as u32).to_le_bytes());
    for sc in &dl.scroll_containers {
        v.extend_from_slice(&sc.node.to_le_bytes());
        v.extend_from_slice(&sc.x.to_le_bytes());
        v.extend_from_slice(&sc.y.to_le_bytes());
        v.extend_from_slice(&sc.w.to_le_bytes());
        v.extend_from_slice(&sc.h.to_le_bytes());
        v.extend_from_slice(&sc.content_h.to_le_bytes());
    }

    // v18: per-item clip-path/mask (B7) + blend/group (B8). Sparse tail —
    // only items stamped with any of the three, as (kind, index, clip
    // descriptor, blend u8, group u32). Blend/group-only entries carry
    // descriptor tag 0 (the reserved blend-only slot). The host installs
    // the shape as a CGPath clip before drawing, maps `blend` (0 normal,
    // 1–15 per the header table) to its blend mode, and opens an offscreen
    // layer per nonzero `group` so descendant blends never reach the page.
    let mut clip_entries: Vec<(u8, u32, Option<&ResolvedClip>, u8, u32)> = Vec::new();
    for (i, r) in dl.rects.iter().enumerate() {
        if r.clip.is_some() || r.blend != 0 || r.group != 0 {
            clip_entries.push((0, i as u32, r.clip.as_ref(), r.blend, r.group));
        }
    }
    for (i, t) in dl.texts.iter().enumerate() {
        if t.clip.is_some() || t.blend != 0 || t.group != 0 {
            clip_entries.push((1, i as u32, t.clip.as_ref(), t.blend, t.group));
        }
    }
    for (i, im) in dl.images.iter().enumerate() {
        if im.clip.is_some() || im.blend != 0 || im.group != 0 {
            clip_entries.push((2, i as u32, im.clip.as_ref(), im.blend, im.group));
        }
    }
    for (i, s) in dl.shadows.iter().enumerate() {
        if s.clip.is_some() || s.blend != 0 || s.group != 0 {
            clip_entries.push((3, i as u32, s.clip.as_ref(), s.blend, s.group));
        }
    }
    v.extend_from_slice(&(clip_entries.len() as u32).to_le_bytes());
    for (kind, idx, c, blend, group) in &clip_entries {
        v.push(*kind);
        v.extend_from_slice(&idx.to_le_bytes());
        match c {
            Some(clip) => put_clip(&mut v, clip),
            None => v.push(0), // tag 0: no clip geometry, blend/group only
        }
        v.push(*blend);
        v.extend_from_slice(&group.to_le_bytes());
    }

    // v19: E3 per-text blur radius for R8 shadow duplicate runs. Sparse —
    // only runs whose TextRun::shadow_blur > 0, as (index u32, blur f32).
    // A blurred run is a shadow copy (its color already carries the shadow
    // color); the host blurs the alpha it draws with (Core Text: NSShadow,
    // GPU: per-glyph blur). 0 is the "crisp duplicate" pre-v19 encoding.
    let blur_entries: Vec<(u32, f32)> = dl
        .texts
        .iter()
        .enumerate()
        .filter(|(_, t)| t.shadow_blur > 0.0)
        .map(|(i, t)| (i as u32, t.shadow_blur))
        .collect();
    v.extend_from_slice(&(blur_entries.len() as u32).to_le_bytes());
    for (idx, blur) in &blur_entries {
        v.extend_from_slice(&idx.to_le_bytes());
        v.extend_from_slice(&blur.to_le_bytes());
    }

    if !dl.scroll_containers.is_empty() {
        v.extend_from_slice(&(dl.scroll_containers.len() as u32).to_le_bytes());
        for sc in &dl.scroll_containers {
            v.extend_from_slice(&sc.content_w.to_le_bytes());
            v.push(sc.mode);
            v.extend_from_slice(&sc.parent.to_le_bytes());
        }
    }

    let semantic_nodes: Vec<(u32, &crate::AxNode)> = flat
        .iter()
        .enumerate()
        .filter_map(|(index, (node, _))| node.has_ax_semantics().then_some((index as u32, *node)))
        .collect();
    if !semantic_nodes.is_empty() {
        if dl.scroll_containers.is_empty() {
            v.extend_from_slice(&0u32.to_le_bytes());
        }
        let mut tail = Vec::new();
        tail.extend_from_slice(&AX_SEMANTICS_MAGIC.to_le_bytes());
        tail.extend_from_slice(&AX_SEMANTICS_VERSION.to_le_bytes());
        tail.extend_from_slice(&0u32.to_le_bytes());
        tail.extend_from_slice(&(semantic_nodes.len() as u32).to_le_bytes());
        for (index, node) in semantic_nodes {
            let mut flags = 0u8;
            if node.disabled {
                flags |= 1;
            }
            if let Some(expanded) = node.expanded {
                flags |= 2;
                if expanded {
                    flags |= 4;
                }
            }
            if let Some(selected) = node.selected {
                flags |= 8;
                if selected {
                    flags |= 16;
                }
            }
            if node.value.is_some() {
                flags |= 32;
            }
            if node.value_min.is_some() {
                flags |= 64;
            }
            if node.value_max.is_some() {
                flags |= 128;
            }
            tail.extend_from_slice(&index.to_le_bytes());
            tail.push(flags);
            tail.push(node.checked.wire_id());
            tail.push(node.heading_level);
            tail.push(node.live_politeness.wire_id());
            tail.extend_from_slice(&node.value_min.unwrap_or(0.0).to_le_bytes());
            tail.extend_from_slice(&node.value_max.unwrap_or(0.0).to_le_bytes());
            put_str(&mut tail, node.value.as_deref().unwrap_or(""));
        }
        let payload_len = (tail.len() - 12) as u32;
        tail[8..12].copy_from_slice(&payload_len.to_le_bytes());
        v.extend_from_slice(&tail);
    }

    v
}

/// Serialize one resolved clip shape (B7) to the v18 wire. Tags stay
/// stable so Swift can build a switch — bump the spec in
/// `include/kitae_core.h` alongside.
fn put_clip(v: &mut Vec<u8>, c: &ResolvedClip) {
    match c {
        ResolvedClip::Circle { cx, cy, r } => {
            v.push(1);
            for f in [cx, cy, r] {
                v.extend_from_slice(&f.to_le_bytes());
            }
        }
        ResolvedClip::Ellipse { cx, cy, rx, ry } => {
            v.push(2);
            for f in [cx, cy, rx, ry] {
                v.extend_from_slice(&f.to_le_bytes());
            }
        }
        ResolvedClip::Inset { x, y, w, h, radii } => {
            v.push(3);
            for f in [x, y, w, h] {
                v.extend_from_slice(&f.to_le_bytes());
            }
            for (hr, vr) in radii {
                v.extend_from_slice(&hr.to_le_bytes());
                v.extend_from_slice(&vr.to_le_bytes());
            }
        }
        ResolvedClip::Polygon { points, evenodd } => {
            v.push(4);
            v.push(*evenodd as u8);
            v.extend_from_slice(&(points.len() as u32).to_le_bytes());
            for (px, py) in points {
                v.extend_from_slice(&px.to_le_bytes());
                v.extend_from_slice(&py.to_le_bytes());
            }
        }
    }
}

/// Serialize one `FilterFn` to the v16 wire. The host applies it via
/// CIFilter (or paints drop-shadow via CGContext shadow). Function tags
/// stay stable so Swift can build a switch — bump the spec in
/// `include/kitae_core.h` alongside.
fn put_filter_fn(v: &mut Vec<u8>, f: &FilterFn) {
    match f {
        FilterFn::Blur(radius) => {
            v.push(0);
            v.extend_from_slice(&radius.to_le_bytes());
        }
        FilterFn::DropShadow {
            dx,
            dy,
            blur,
            color,
        } => {
            v.push(1);
            v.extend_from_slice(&dx.to_le_bytes());
            v.extend_from_slice(&dy.to_le_bytes());
            v.extend_from_slice(&blur.to_le_bytes());
            v.extend_from_slice(&color.to_le_bytes());
        }
        FilterFn::Brightness(n) => {
            v.push(2);
            v.extend_from_slice(&n.to_le_bytes());
        }
        FilterFn::Contrast(n) => {
            v.push(3);
            v.extend_from_slice(&n.to_le_bytes());
        }
        FilterFn::Grayscale(n) => {
            v.push(4);
            v.extend_from_slice(&n.to_le_bytes());
        }
        FilterFn::HueRotate(deg) => {
            v.push(5);
            v.extend_from_slice(&deg.to_le_bytes());
        }
        FilterFn::Invert(n) => {
            v.push(6);
            v.extend_from_slice(&n.to_le_bytes());
        }
        FilterFn::Opacity(n) => {
            v.push(7);
            v.extend_from_slice(&n.to_le_bytes());
        }
        FilterFn::Saturate(n) => {
            v.push(8);
            v.extend_from_slice(&n.to_le_bytes());
        }
        FilterFn::Sepia(n) => {
            v.push(9);
            v.extend_from_slice(&n.to_le_bytes());
        }
    }
}

fn put_stops(v: &mut Vec<u8>, stops: &[(f32, u32)]) {
    v.extend_from_slice(&(stops.len() as u32).to_le_bytes());
    for (offset, color) in stops {
        v.extend_from_slice(&offset.to_le_bytes());
        v.extend_from_slice(&color.to_le_bytes());
    }
}

fn put_str(v: &mut Vec<u8>, s: &str) {
    v.extend_from_slice(&(s.len() as u32).to_le_bytes());
    v.extend_from_slice(s.as_bytes());
}

// ---------------------------------------------------------------------------
// C API (mirrors include/kitae_core.h exactly)
// ---------------------------------------------------------------------------

/// Host-provided text measurement. `measure` returns the advance width of
/// `text` at `size` points; `metrics` returns ascent/descent. Both must be
/// safe to call from the engine thread (Core Text measurement is). `family`
/// is the run's first non-generic `font-family` ("" = the host's default).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KitaeMeasurerVtable {
    pub ctx: *mut c_void,
    pub measure: extern "C" fn(
        ctx: *mut c_void,
        text: *const c_char,
        size: f32,
        weight: u32,
        italic: c_int,
        monospace: c_int,
        family: *const c_char,
    ) -> f32,
    pub metrics: extern "C" fn(
        ctx: *mut c_void,
        size: f32,
        monospace: c_int,
        ascent: *mut f32,
        descent: *mut f32,
        family: *const c_char,
    ),
    /// B9: x-height (the `ex` unit) of the face at `size`; `None` keeps
    /// the 0.5em fallback. `family` is "" for the host default.
    pub x_height: Option<
        extern "C" fn(
            ctx: *mut c_void,
            size: f32,
            weight: u32,
            italic: c_int,
            monospace: c_int,
            family: *const c_char,
        ) -> f32,
    >,
}

/// The C shape of a host rasterizer's two functions — see
/// [`PageRasterVtable`] for the contract they must honour.
pub type KitaeRasterFn = unsafe extern "C" fn(*mut c_void, *const Browser, *mut u8, usize) -> isize;

/// B4: install the host's page rasterizer. `ctx` is opaque to the engine and
/// `free_ctx` (may be NULL) releases it when the engine drops; `png` and
/// `pdf` (either may be NULL) fill a caller-provided buffer.
///
/// The app links `kitae_headless` and reaches this through
/// `kitae_headless_install_raster()`, which is one call with no function
/// pointers in the host's own code.
///
/// # Safety
/// `ctx` must stay valid until `free_ctx` runs; both functions must be
/// callable from the engine thread with a read-only browser.
#[no_mangle]
pub extern "C" fn kitae_engine_set_raster(
    engine: *mut Engine,
    ctx: *mut c_void,
    png: Option<KitaeRasterFn>,
    pdf: Option<KitaeRasterFn>,
    free_ctx: Option<unsafe extern "C" fn(*mut c_void)>,
) {
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return;
    };
    let vt = PageRasterVtable {
        ctx,
        png,
        pdf,
        free: free_ctx,
    };
    let _ = engine.tx.send(Command::SetRaster(SendRaster(vt)));
}

#[no_mangle]
pub extern "C" fn kitae_engine_new(
    viewport_w: c_int,
    viewport_h: c_int,
    vtable: *const KitaeMeasurerVtable,
) -> *mut Engine {
    // S-5: a panic here must not unwind through the C ABI (UB → abort).
    // Null is the documented "could not start" answer.
    let Some(engine) = ffi_catch(|| {
        let vt = unsafe { vtable.as_ref()? };
        Some(Box::new(Engine::new(
            viewport_w as f32,
            viewport_h as f32,
            *vt,
        )))
    })
    .flatten() else {
        return std::ptr::null_mut();
    };
    Box::into_raw(engine)
}

/// Shut the engine down. The thread finishes any in-flight fetch first.
#[no_mangle]
pub extern "C" fn kitae_engine_destroy(engine: *mut Engine) {
    if engine.is_null() {
        return;
    }
    // S-5: dropping runs Browser::drop (channel close + join); a panic in a
    // Drop impl would abort the process — catch it and leak quietly instead.
    let _ = ffi_catch(|| unsafe { drop(Box::from_raw(engine)) });
}

/// Command kinds for [`kitae_engine_command`].
pub const KITAE_CMD_NAVIGATE: c_int = 1;
pub const KITAE_CMD_BACK: c_int = 2;
pub const KITAE_CMD_FORWARD: c_int = 3;
pub const KITAE_CMD_RELOAD: c_int = 4;
pub const KITAE_CMD_SCROLL: c_int = 5;
pub const KITAE_CMD_RESIZE: c_int = 6;
pub const KITAE_CMD_CLICK: c_int = 7;
pub const KITAE_CMD_FOCUS_NEXT: c_int = 8;
pub const KITAE_CMD_FOCUS_PREV: c_int = 9;
pub const KITAE_CMD_ACTIVATE: c_int = 10;
pub const KITAE_CMD_SET_INPUT: c_int = 11;
pub const KITAE_CMD_SUBMIT: c_int = 12;
pub const KITAE_CMD_SELECT_ALL: c_int = 13;
pub const KITAE_CMD_CLEAR_SELECTION: c_int = 14;
pub const KITAE_CMD_MEDIA_OPEN: c_int = 15;
pub const KITAE_CMD_MEDIA_PLAY: c_int = 16; /* a = playing (0/1), b = position secs */
pub const KITAE_CMD_MEDIA_STATE: c_int = 17;
pub const KITAE_CMD_HOVER: c_int = 18; /* url = element id (empty clears) */
pub const KITAE_CMD_ACTIVE: c_int = 19; /* url = element id (empty clears) */
pub const KITAE_CMD_FIND: c_int = 20; /* url = query (empty clears) */
pub const KITAE_CMD_FIND_NEXT: c_int = 21;
pub const KITAE_CMD_FIND_PREV: c_int = 22;
pub const KITAE_CMD_FIND_CLEAR: c_int = 23;
pub const KITAE_CMD_SET_PASSWORD: c_int = 24; /* url = password */
pub const KITAE_CMD_SET_ENVIRONMENT: c_int = 25; /* a = scheme (0 light, 1 dark); b bits: 1 reduce-motion, 2 more-contrast */
pub const KITAE_CMD_SAVE_PAGE: c_int = 26; /* url = destination path; outcome → notifications */
pub const KITAE_CMD_LIVE_POLL: c_int = 27; /* drain a mounted kitae_live socket (embedders without an idle loop) */
pub const KITAE_CMD_LIVE_STATE: c_int = 28; /* reply via notifications: "live\t1" connected / "live\t0" not */
pub const KITAE_CMD_ESCAPE: c_int = 29; /* close the topmost open modal dialog */
pub const KITAE_CMD_STEP_FOCUS: c_int = 30; /* a = direction (+1 / -1); range/number/select value step */
pub const KITAE_CMD_TYPE: c_int = 31; /* url = text to insert at the caret (A11a) */
pub const KITAE_CMD_BACKSPACE: c_int = 32; /* delete the char before the caret (A11a) */
pub const KITAE_CMD_DELETE_FORWARD: c_int = 33; /* delete the char after the caret (A11a) */
pub const KITAE_CMD_MOVE_CARET: c_int = 34; /* a = char delta (negative = left) (A11a) */
pub const KITAE_CMD_SET_CARET: c_int = 35; /* a = absolute char offset, clamped (A11a) */
pub const KITAE_CMD_SET_FILE_FILES: c_int = 36; /* H2: url = "id\npath1\tpath2…" (empty paths clear) */
pub const KITAE_CMD_SET_AUTH: c_int = 37; /* H6: url = "user\npass" — the 401 sign-in answer */
pub const KITAE_CMD_INSPECT_PICK: c_int = 38; /* H5: a = x, b = y (page space); reply via */
/* notifications: "inspect\t{node}" on a hit */
pub const KITAE_CMD_INSPECT_INFO: c_int = 39; /* H5: a = node id; reply via notifications: */
/* "inspectinfo\t{lines…}" (multi-line)      */
pub const KITAE_CMD_SET_PRIVACY_POLICY: c_int = 40;
pub const KITAE_CMD_SET_SEARCH_PROVIDER: c_int = 41;
pub const KITAE_CMD_RESOLVE_INSECURE_NAVIGATION: c_int = 42;
pub const KITAE_CMD_SELECTION_BEGIN: c_int = 43;
pub const KITAE_CMD_SELECTION_EXTEND: c_int = 44;
pub const KITAE_CMD_SELECTION_END: c_int = 45;
pub const KITAE_CMD_COMPOSITION_UPDATE: c_int = 46;
pub const KITAE_CMD_COMPOSITION_COMMIT: c_int = 47;
pub const KITAE_CMD_COMPOSITION_CANCEL: c_int = 48;
#[allow(dead_code)]
pub const KITAE_CMD_SET_MARKED_TEXT: c_int = KITAE_CMD_COMPOSITION_UPDATE;
#[allow(dead_code)]
pub const KITAE_CMD_COMMIT_COMPOSITION: c_int = KITAE_CMD_COMPOSITION_COMMIT;
#[allow(dead_code)]
pub const KITAE_CMD_CANCEL_COMPOSITION: c_int = KITAE_CMD_COMPOSITION_CANCEL;
pub const KITAE_CMD_EXTEND_CARET: c_int = 49;
pub const KITAE_CMD_SELECTION_BEGIN_POINT: c_int = 50;
pub const KITAE_CMD_SELECTION_EXTEND_POINT: c_int = 51;
pub const KITAE_CMD_SET_MEDIA: c_int = 52; /* a = media type (0 screen, 1 print) — H3 */
pub const KITAE_CMD_SET_PAGE_JS: c_int = 53; /* a = 1 enable, 0 disable page JavaScript — SCRIPTING_DECISION.md */
pub const KITAE_CMD_AGENT_ACT: c_int = 54; /* url = the verb's JSON; the answer
                                           arrives on the notifications queue
                                           as "agent-result\t{json}" (B4) */
pub const KITAE_CMD_AGENT_JOURNAL: c_int = 55; /* reply: "agent-journal\t{json}" +
                                               "agent-policy\t{json}" (B4) */
pub const KITAE_CMD_AGENT_GRANT: c_int = 56; /* url = verb name, or "*" — the
                                             user's answer to a prompt (B4) */
pub const KITAE_CMD_AGENT_REVOKE_GRANTS: c_int = 57; /* drop every grant (B4) */
pub const KITAE_CMD_AGENT_DECLARE: c_int = 58; /* url = declared verb names (B4) */

/// Send a command. `a`/`b` carry scroll deltas, click points or viewport
/// sizes; `url` carries the navigation target (ignored otherwise), except:
/// `SET_INPUT` expects `"id\nvalue"`, `SUBMIT`/`HOVER`/`ACTIVE` expect an id,
/// `SET_FILE_FILES` expects `"id\npath1\tpath2…"` (H2), `SET_AUTH` expects
/// `"user\npass"` (H6).
#[no_mangle]
pub extern "C" fn kitae_engine_command(
    engine: *mut Engine,
    kind: c_int,
    a: f32,
    b: f32,
    url: *const c_char,
) {
    // S-5: command *parsing* runs on the caller's thread — guard it too.
    let Some(cmd) = ffi_catch(|| {
        let engine = unsafe { engine.as_ref()? };
        parse_command(engine, kind, a, b, url)
    })
    .flatten() else {
        return;
    };
    let Some(engine) = (unsafe { engine.as_ref() }) else {
        return;
    };
    let _ = engine.tx.send(cmd);
}

/// Translate a raw C command into the engine's [`Command`] enum (unknown
/// kinds are dropped). Split out of `kitae_engine_command` so the guard
/// wraps the parse separately from the channel send.
fn parse_command(
    _engine: &Engine,
    kind: c_int,
    a: f32,
    b: f32,
    url: *const c_char,
) -> Option<Command> {
    Some(match kind {
        KITAE_CMD_NAVIGATE => Command::Navigate(read_cstr(url)),
        KITAE_CMD_BACK => Command::Back,
        KITAE_CMD_FORWARD => Command::Forward,
        KITAE_CMD_RELOAD => Command::Reload,
        KITAE_CMD_SCROLL => Command::Scroll(a),
        KITAE_CMD_RESIZE => Command::Resize(a, b),
        KITAE_CMD_CLICK => Command::Click(a, b),
        KITAE_CMD_FOCUS_NEXT => Command::FocusNext,
        KITAE_CMD_FOCUS_PREV => Command::FocusPrev,
        KITAE_CMD_ACTIVATE => Command::Activate,
        KITAE_CMD_SET_INPUT => {
            let s = read_cstr(url);
            match s.split_once('\n') {
                Some((id, v)) => Command::SetInput(id.to_string(), v.to_string()),
                None => Command::SetInput(s, String::new()),
            }
        }
        KITAE_CMD_SUBMIT => Command::Submit(read_cstr(url)),
        KITAE_CMD_SELECT_ALL => Command::SelectAll,
        KITAE_CMD_CLEAR_SELECTION => Command::ClearSelection,
        KITAE_CMD_HOVER => Command::SetHover(nonempty_cstr(url)),
        KITAE_CMD_ACTIVE => Command::SetActive(nonempty_cstr(url)),
        KITAE_CMD_FIND => Command::Find(read_cstr(url)),
        KITAE_CMD_FIND_NEXT => Command::FindNext,
        KITAE_CMD_FIND_PREV => Command::FindPrev,
        KITAE_CMD_FIND_CLEAR => Command::FindClear,
        KITAE_CMD_MEDIA_OPEN => Command::MediaOpen,
        KITAE_CMD_MEDIA_PLAY => Command::MediaPlay(a, b),
        KITAE_CMD_MEDIA_STATE => Command::MediaState,
        KITAE_CMD_SET_PASSWORD => Command::SetPassword(read_cstr(url)),
        KITAE_CMD_SET_ENVIRONMENT => Command::SetPreferences(EnvironmentState {
            color_scheme: if a >= 0.5 {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            },
            reduced_motion: (b as i32 & 1) != 0,
            contrast_more: (b as i32 & 2) != 0,
            ..EnvironmentState::default()
        }),
        KITAE_CMD_SET_PAGE_JS => Command::SetPageJs(a >= 0.5),
        KITAE_CMD_SET_MEDIA => Command::SetMediaType(if a >= 0.5 {
            MediaType::Print
        } else {
            MediaType::Screen
        }),
        KITAE_CMD_SAVE_PAGE => Command::SavePage(read_cstr(url)),
        KITAE_CMD_LIVE_POLL => Command::LivePoll,
        KITAE_CMD_LIVE_STATE => Command::LiveState,
        KITAE_CMD_ESCAPE => Command::Escape,
        KITAE_CMD_STEP_FOCUS => Command::StepFocus(if a < 0.0 { -1 } else { 1 }),
        // A11a: caret + text editing. TypeText carries the string in `url`;
        // the caret commands fold the float arg to an integer offset/delta.
        KITAE_CMD_TYPE => Command::TypeText(read_cstr(url)),
        KITAE_CMD_BACKSPACE => Command::Backspace,
        KITAE_CMD_DELETE_FORWARD => Command::DeleteForward,
        KITAE_CMD_MOVE_CARET => Command::MoveCaret(a as isize),
        KITAE_CMD_SET_CARET => Command::SetCaret(if a < 0.0 { 0 } else { a as usize }),
        KITAE_CMD_EXTEND_CARET => Command::ExtendCaret(a as isize),
        KITAE_CMD_SELECTION_BEGIN => {
            let offset = parse_offset_arg(a)?;
            Command::SelectionBegin(offset)
        }
        KITAE_CMD_SELECTION_EXTEND => {
            let offset = parse_offset_arg(a)?;
            Command::SelectionExtend(offset)
        }
        KITAE_CMD_SELECTION_BEGIN_POINT => Command::SelectionBeginPoint(a, b),
        KITAE_CMD_SELECTION_EXTEND_POINT => Command::SelectionExtendPoint(a, b),
        KITAE_CMD_SELECTION_END => Command::SelectionEnd,
        KITAE_CMD_COMPOSITION_UPDATE => Command::CompositionUpdate(read_cstr(url)),
        KITAE_CMD_COMPOSITION_COMMIT => {
            Command::CompositionCommit((!url.is_null()).then(|| read_cstr(url)))
        }
        KITAE_CMD_COMPOSITION_CANCEL => Command::CompositionCancel,
        // H2: `"id\npath1\tpath2…"` — the id up to the newline, the picks
        // tab-separated (paths cannot carry newlines in the wire payload;
        // tabs in a real path are vanishingly rare and only break that one
        // pick). An empty remainder clears the control's selection.
        KITAE_CMD_SET_FILE_FILES => {
            let s = read_cstr(url);
            match s.split_once('\n') {
                Some((id, rest)) => Command::SetFileFiles(
                    id.to_string(),
                    rest.split('\t')
                        .filter(|p| !p.is_empty())
                        .map(String::from)
                        .collect(),
                ),
                None => Command::SetFileFiles(s, Vec::new()),
            }
        }
        // H6: the sign-in sheet's answer — user name up to the newline,
        // password after it (neither may carry a newline on this wire).
        KITAE_CMD_SET_AUTH => {
            let s = read_cstr(url);
            match s.split_once('\n') {
                Some((user, pass)) => Command::SetAuth(user.to_string(), pass.to_string()),
                None => Command::SetAuth(s, String::new()),
            }
        }
        KITAE_CMD_INSPECT_PICK => Command::InspectPick(a, b),
        KITAE_CMD_INSPECT_INFO => Command::InspectInfo(if a < 0.0 { 0 } else { a as usize }),
        KITAE_CMD_SET_PRIVACY_POLICY => {
            let bits = a as u32;
            Command::SetPrivacy {
                dnt: bits & 1 != 0,
                block_third_party_cookies: bits & 2 != 0,
                https_only: bits & 4 != 0,
                warn_insecure_http: bits & 8 != 0,
            }
        }
        KITAE_CMD_SET_SEARCH_PROVIDER => {
            Command::SetSearchProvider(if a < 0.0 { 0 } else { a as u8 })
        }
        KITAE_CMD_RESOLVE_INSECURE_NAVIGATION => Command::ResolveInsecureNavigation(a >= 0.5),
        // B4: the agent plane. `AgentAct` carries the verb's wire JSON, so a
        // host needs no new command per verb.
        KITAE_CMD_AGENT_ACT => Command::AgentAct(read_cstr(url)),
        KITAE_CMD_AGENT_JOURNAL => Command::AgentJournal,
        KITAE_CMD_AGENT_GRANT => Command::AgentGrant(read_cstr(url)),
        KITAE_CMD_AGENT_REVOKE_GRANTS => Command::AgentRevokeGrants,
        KITAE_CMD_AGENT_DECLARE => Command::AgentDeclare(read_cstr(url)),
        _ => return None,
    })
}

/// Copy the newest published frame into `buf`. Returns the frame's full
/// length; when `cap` is at least that, the frame is copied and consumed,
/// otherwise it stays pending (grow the buffer and call again). Zero means
/// nothing new since the last successful call.
#[no_mangle]
pub extern "C" fn kitae_engine_frame(engine: *mut Engine, buf: *mut u8, cap: usize) -> usize {
    ffi_catch(|| {
        let Some(engine) = (unsafe { engine.as_ref() }) else {
            return 0usize;
        };
        let Ok(mut guard) = engine.frame.lock() else {
            return 0usize;
        };
        let Some(data) = guard.as_ref() else {
            return 0usize;
        };
        let len = data.len();
        if !buf.is_null() && cap >= len {
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf, len) };
            *guard = None; // consumed
        }
        len
    })
    .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn kitae_frame_version() -> u32 {
    FRAME_VERSION
}

#[no_mangle]
pub extern "C" fn kitae_engine_frame_available(engine: *mut Engine, out_len: *mut usize) -> c_int {
    ffi_catch(|| {
        let Some(engine) = (unsafe { engine.as_ref() }) else {
            return 0;
        };
        let Ok(guard) = engine.frame.lock() else {
            return 0;
        };
        let Some(data) = guard.as_ref() else {
            return 0;
        };
        if !out_len.is_null() {
            unsafe { *out_len = data.len() };
        }
        1
    })
    .unwrap_or(0)
}

/// 1 while a navigation is in flight, 0 otherwise (for the loading hairline).
#[no_mangle]
pub extern "C" fn kitae_engine_loading(engine: *mut Engine) -> c_int {
    ffi_catch(|| {
        let Some(engine) = (unsafe { engine.as_ref() }) else {
            return 0;
        };
        u8::from(*engine.loading.lock().unwrap_or_else(|e| e.into_inner())) as c_int
    })
    .unwrap_or(0)
}

/// Pop one escape-hatch URL queued by the engine (kitae:open-external: /
/// mailto:). Returns 1 and NUL-terminates into `buf` (≥ 4 KiB recommended),
/// 0 when the queue is empty or `buf` is too small (the URL is dropped).
#[no_mangle]
pub extern "C" fn kitae_engine_take_external(
    engine: *mut Engine,
    buf: *mut c_char,
    cap: usize,
) -> c_int {
    ffi_catch(|| {
        let Some(engine) = (unsafe { engine.as_ref() }) else {
            return 0;
        };
        let Some(url) = engine
            .externals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        else {
            return 0;
        };
        if buf.is_null() || cap <= url.len() {
            return 0;
        }
        unsafe {
            for (i, &b) in url.as_bytes().iter().enumerate() {
                buf.add(i).write(b as c_char);
            }
            buf.add(url.len()).write(0);
        }
        1
    })
    .unwrap_or(0)
}

/// Pop one queued engine notification — currently Save-Page outcomes:
/// `"1\t{path}"` on success, `"0\t{message}"` on failure. Returns 1 and
/// NUL-terminates into `buf` (≥ 4 KiB recommended), 0 when the queue is
/// empty or `buf` is too small (the message is dropped).
/// How long the oldest queued notification is, without consuming it: 1 when
/// one is queued (with its length written to `out_len`, excluding the NUL),
/// 0 when the queue is empty. A host must size its buffer from this —
/// `kitae_engine_take_notification` cannot put an oversized message back, and
/// an agent result (a screenshot's base64, a journal dump) can be large.
#[no_mangle]
pub extern "C" fn kitae_engine_notification_available(
    engine: *mut Engine,
    out_len: *mut usize,
) -> c_int {
    ffi_catch(|| {
        let Some(engine) = (unsafe { engine.as_ref() }) else {
            return 0;
        };
        let queue = engine
            .notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(msg) = queue.front() else {
            return 0;
        };
        if !out_len.is_null() {
            unsafe { *out_len = msg.len() };
        }
        1
    })
    .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn kitae_engine_take_notification(
    engine: *mut Engine,
    buf: *mut c_char,
    cap: usize,
) -> c_int {
    ffi_catch(|| {
        let Some(engine) = (unsafe { engine.as_ref() }) else {
            return 0;
        };
        let Some(msg) = engine
            .notifications
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
        else {
            return 0;
        };
        if buf.is_null() || cap <= msg.len() {
            return 0;
        }
        unsafe {
            for (i, &b) in msg.as_bytes().iter().enumerate() {
                buf.add(i).write(b as c_char);
            }
            buf.add(msg.len()).write(0);
        }
        1
    })
    .unwrap_or(0)
}

fn read_cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn parse_offset_arg(value: f32) -> Option<usize> {
    if !value.is_finite() || value < 0.0 || value.fract() != 0.0 || value > u32::MAX as f32 {
        None
    } else {
        Some(value as usize)
    }
}

/// `read_cstr` mapped to `None` when empty (id-clearing commands).
fn nonempty_cstr(p: *const c_char) -> Option<String> {
    let s = read_cstr(p);
    (!s.is_empty()).then_some(s)
}

#[no_mangle]
pub extern "C" fn kitae_registrable_domain(
    url: *const c_char,
    out: *mut c_char,
    cap: usize,
) -> usize {
    let input = read_cstr(url);
    let Some(domain) = kitae_net::registrable_domain(&input) else {
        return 0;
    };
    let len = domain.len() + 1;
    if !out.is_null() && cap >= len {
        unsafe {
            std::ptr::copy_nonoverlapping(domain.as_ptr(), out as *mut u8, domain.len());
            out.add(domain.len()).write(0);
        }
    }
    len
}

#[no_mangle]
pub extern "C" fn kitae_hsts_covers(url: *const c_char) -> c_int {
    c_int::from(kitae_net::hsts_covers(&read_cstr(url)))
}

#[no_mangle]
pub extern "C" fn kitae_hsts_expires_in(url: *const c_char) -> i64 {
    kitae_net::hsts_effective_policy(&read_cstr(url))
        .map(|policy| policy.expires_in(std::time::Instant::now()).as_secs() as i64)
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn kitae_hsts_includes_subdomains(url: *const c_char) -> c_int {
    c_int::from(
        kitae_net::hsts_effective_policy(&read_cstr(url))
            .is_some_and(|policy| policy.include_subdomains),
    )
}

#[no_mangle]
pub extern "C" fn kitae_clear_hsts(url: *const c_char) -> c_int {
    c_int::from(kitae_net::clear_hsts_for_host(&read_cstr(url)))
}

#[no_mangle]
pub extern "C" fn kitae_clear_all_hsts() {
    kitae_net::clear_hsts();
}

fn hsts_snapshot_bytes() -> Vec<u8> {
    let entries = kitae_net::hsts_snapshot();
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for entry in entries {
        let host = entry.host.as_bytes();
        out.extend_from_slice(&(host.len() as u32).to_le_bytes());
        out.extend_from_slice(host);
        out.push(entry.include_subdomains as u8);
        out.extend_from_slice(&entry.expires_at_unix.to_le_bytes());
    }
    out
}

#[no_mangle]
pub extern "C" fn kitae_hsts_snapshot_size() -> usize {
    hsts_snapshot_bytes().len()
}

#[no_mangle]
pub extern "C" fn kitae_hsts_snapshot(out: *mut u8, cap: usize) -> usize {
    let bytes = hsts_snapshot_bytes();
    if !out.is_null() && cap >= bytes.len() {
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len());
        }
    }
    bytes.len()
}

#[no_mangle]
pub extern "C" fn kitae_hsts_import_snapshot(data: *const u8, len: usize) -> c_int {
    if data.is_null() || len < 8 {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    if u32::from_le_bytes(bytes[0..4].try_into().unwrap()) != 1 {
        return 0;
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    if count > kitae_net::HSTS_MAX_ENTRIES {
        return 0;
    }
    let mut cursor = 8usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(header_end) = cursor.checked_add(4) else {
            return 0;
        };
        if header_end > len {
            return 0;
        }
        let host_len = u32::from_le_bytes(bytes[cursor..header_end].try_into().unwrap()) as usize;
        cursor = header_end;
        let Some(end) = cursor.checked_add(host_len) else {
            return 0;
        };
        let Some(record_end) = end.checked_add(9) else {
            return 0;
        };
        if record_end > len {
            return 0;
        }
        let host = match std::str::from_utf8(&bytes[cursor..end]) {
            Ok(host) => host.to_string(),
            Err(_) => return 0,
        };
        cursor = end;
        let include_subdomains = match bytes[cursor] {
            0 => false,
            1 => true,
            _ => return 0,
        };
        cursor += 1;
        let expires_at_unix = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
        cursor += 8;
        entries.push(kitae_net::HstsSnapshotEntry {
            host,
            include_subdomains,
            expires_at_unix,
        });
    }
    if cursor != len {
        return 0;
    }
    kitae_net::replace_hsts_snapshot(&entries) as c_int
}

// ---------------------------------------------------------------------------
// Image rasters (J3): the frame's image entries carry payload ids; the host
// pulls the actual pixels here. Layout: u32 width, u32 height, then
// width*height*4 bytes RGBA8 (row-major, straight alpha).
// ---------------------------------------------------------------------------

/// Query a decoded raster's byte size: `8 + w*h*4`. Returns 0 when the id is
/// unknown (placeholder images and web images have payload 0).
#[no_mangle]
pub extern "C" fn kitae_qtsq_raster_size(id: u64) -> usize {
    ffi_catch(|| match crate::qtsq_raster_for_id(id) {
        Some(r) => 8 + r.width as usize * r.height as usize * 4,
        None => 0,
    })
    .unwrap_or(0)
}

/// Copy a decoded raster into `buf` (`kitae_qtsq_raster_size` bytes):
/// u32 width, u32 height, then tightly packed RGBA8. Returns 1 on success,
/// 0 when the id is unknown or `cap` is too small (the raster stays valid).
#[no_mangle]
pub extern "C" fn kitae_qtsq_raster(id: u64, buf: *mut u8, cap: usize) -> c_int {
    ffi_catch(|| {
        let Some(r) = crate::qtsq_raster_for_id(id) else {
            return 0;
        };
        let total = 8 + r.width as usize * r.height as usize * 4;
        if buf.is_null() || cap < total {
            return 0;
        }
        let bytes = r.pixels.as_slice();
        unsafe {
            std::ptr::copy_nonoverlapping(r.width.to_le_bytes().as_ptr(), buf, 4);
            std::ptr::copy_nonoverlapping(r.height.to_le_bytes().as_ptr(), buf.add(4), 4);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.add(8), bytes.len());
        }
        1
    })
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Animation frames (Q8): animated images stamp `animated` in the wire; hosts
// pull the sequence here and step frames on their own clock. Frame pixels
// share the still's layout (u32 w, u32 h, RGBA8), so `kitae_qtsq_raster_size`
// sizes every frame; delays and loop count come from the helpers below.
// ---------------------------------------------------------------------------

/// Frame count for an animated image raster (0 = not animated).
#[no_mangle]
pub extern "C" fn kitae_raster_frames(id: u64) -> u32 {
    ffi_catch(|| {
        crate::qtsq_animation_for_id(id)
            .map(|a| a.frames.len() as u32)
            .unwrap_or(0)
    })
    .unwrap_or(0)
}

/// Presentation delay of frame `n` in milliseconds (0 = no such frame).
#[no_mangle]
pub extern "C" fn kitae_raster_frame_delay(id: u64, n: u32) -> u32 {
    ffi_catch(|| match crate::qtsq_animation_for_id(id) {
        Some(a) => a.frames.get(n as usize).map(|f| f.delay_ms).unwrap_or(0),
        None => 0,
    })
    .unwrap_or(0)
}

/// The file's loop declaration: -1 = not animated, otherwise the raw count
/// (0 = play once, n = n extra repetitions, 65535 = forever).
#[no_mangle]
pub extern "C" fn kitae_raster_loop(id: u64) -> c_int {
    ffi_catch(|| match crate::qtsq_animation_for_id(id) {
        None => -1,
        Some(a) => a.loop_count.unwrap_or(0) as c_int,
    })
    .unwrap_or(-1)
}

/// Copy frame `n` of an animated image into `buf`
/// (`kitae_qtsq_raster_size(id)` bytes, same layout as the still). Returns 1
/// on success, 0 when the id/frame is unknown or `cap` is too small.
#[no_mangle]
pub extern "C" fn kitae_raster_frame(id: u64, n: u32, buf: *mut u8, cap: usize) -> c_int {
    ffi_catch(|| {
        let Some(a) = crate::qtsq_animation_for_id(id) else {
            return 0;
        };
        let Some(f) = a.frames.get(n as usize) else {
            return 0;
        };
        let total = 8 + a.width as usize * a.height as usize * 4;
        if buf.is_null() || cap < total {
            return 0;
        }
        let bytes = f.pixels.as_slice();
        unsafe {
            std::ptr::copy_nonoverlapping(a.width.to_le_bytes().as_ptr(), buf, 4);
            std::ptr::copy_nonoverlapping(a.height.to_le_bytes().as_ptr(), buf.add(4), 4);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.add(8), bytes.len());
        }
        1
    })
    .unwrap_or(0)
}

/// Purge the decoded image caches (I3): decoded rasters, animation frames
/// and the network byte cache. Everything re-decodes on demand, so this is
/// safe under memory pressure. Thread-safe; takes no engine handle.
#[no_mangle]
pub extern "C" fn kitae_purge_image_caches() {
    ffi_catch(|| {
        crate::purge_image_caches();
    });
}

// ---------------------------------------------------------------------------
// Web video (F2/F4/F6 wire): the engine demuxes `<video>` sources into
// compressed samples; the embedder runs its platform decoder (VideoToolbox)
// and feeds decoded RGBA frames back via `kitae_web_video_feed`, which
// registers them as rasters under the media URL so the next publish stamps
// them. Codec ids match the v20 header table: 1 H264, 2 HEVC, 3 VP8, 4 VP9,
// 5 AV1; 0 = no web video. All take the resolved media URL (the frame's
// media `src`) and touch only global stores — no engine handle needed.
// ---------------------------------------------------------------------------

/// Codec id for the web video at `url` (0 = none/unknown).
#[no_mangle]
pub extern "C" fn kitae_web_video_codec(url: *const c_char) -> u32 {
    ffi_catch(|| crate::web_video_codec(&read_cstr(url))).unwrap_or(0)
}

/// Width/height of the web video at `url`. Returns 1 on success, 0 when
/// unknown (pointers untouched).
#[no_mangle]
pub extern "C" fn kitae_web_video_dims(url: *const c_char, w: *mut u32, h: *mut u32) -> c_int {
    ffi_catch(|| {
        let Some((vw, vh)) = crate::web_video_dims(&read_cstr(url)) else {
            return 0;
        };
        if w.is_null() || h.is_null() {
            return 0;
        }
        unsafe {
            w.write(vw);
            h.write(vh);
        }
        1
    })
    .unwrap_or(0)
}

/// Nominal frame rate of the web video at `url` (0 when unknown).
#[no_mangle]
pub extern "C" fn kitae_web_video_fps(url: *const c_char) -> f64 {
    ffi_catch(|| crate::web_video_fps(&read_cstr(url))).unwrap_or(0.0)
}

/// Copy the codec extradata (avcC/hvcC/av1C box) into `buf`; returns the full
/// length so the caller can retry with a larger buffer, 0 when unknown.
#[no_mangle]
pub extern "C" fn kitae_web_video_extradata(url: *const c_char, buf: *mut u8, cap: usize) -> usize {
    ffi_catch(|| {
        // `buf == null` is a sizing probe: return the full length.
        if buf.is_null() {
            return crate::web_video_extradata(&read_cstr(url), &mut []);
        }
        let mut out = vec![0u8; cap];
        let full = crate::web_video_extradata(&read_cstr(url), &mut out);
        if full > 0 && cap > 0 {
            let n = full.min(cap);
            unsafe { std::ptr::copy_nonoverlapping(out.as_ptr(), buf, n) };
        }
        full
    })
    .unwrap_or(0)
}

/// Number of compressed video samples in the track (0 when unknown).
#[no_mangle]
pub extern "C" fn kitae_web_video_sample_count(url: *const c_char) -> u32 {
    ffi_catch(|| crate::web_video_sample_count(&read_cstr(url)) as u32).unwrap_or(0)
}

/// Copy compressed sample `index` into `buf`; returns the full sample length
/// (0 when the URL or index misses) so the caller can size and retry.
#[no_mangle]
pub extern "C" fn kitae_web_video_sample(
    url: *const c_char,
    index: u32,
    buf: *mut u8,
    cap: usize,
) -> usize {
    ffi_catch(|| {
        // `buf == null` is a sizing probe: return the full sample length.
        if buf.is_null() {
            return crate::web_video_sample(&read_cstr(url), index as usize, &mut []);
        }
        let mut out = vec![0u8; cap];
        let full = crate::web_video_sample(&read_cstr(url), index as usize, &mut out);
        if full > 0 && cap > 0 {
            let n = full.min(cap);
            unsafe { std::ptr::copy_nonoverlapping(out.as_ptr(), buf, n) };
        }
        full
    })
    .unwrap_or(0)
}

/// Whether sample `index` is a sync sample (keyframe): 1 yes, 0 no/unknown.
#[no_mangle]
pub extern "C" fn kitae_web_video_sample_keyframe(url: *const c_char, index: u32) -> c_int {
    ffi_catch(|| crate::web_video_sample_keyframe(&read_cstr(url), index as usize) as c_int)
        .unwrap_or(0)
}

/// The embedder decoded one frame of the web video at `url`: `len` bytes of
/// RGBA8 (row-major, straight alpha), `w`×`h`. Returns 1 on success, 0 on
/// bad arguments. The next publish stamps the frame.
#[no_mangle]
pub extern "C" fn kitae_web_video_feed(
    url: *const c_char,
    w: u32,
    h: u32,
    rgba: *const u8,
    len: usize,
) -> c_int {
    ffi_catch(|| {
        if rgba.is_null() || w == 0 || h == 0 {
            return 0;
        }
        let expected = w as usize * h as usize * 4;
        if len < expected {
            return 0;
        }
        let pixels = unsafe { std::slice::from_raw_parts(rgba, expected) }.to_vec();
        crate::web_video_feed_frame(&read_cstr(url), w, h, pixels);
        1
    })
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests: drive the C ABI exactly the way a host does (vtable in, bytes out)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../kitae_qtsq/testdata");

    extern "C" fn test_measure(
        _ctx: *mut c_void,
        text: *const c_char,
        _size: f32,
        _weight: u32,
        _italic: c_int,
        _monospace: c_int,
        _family: *const c_char,
    ) -> f32 {
        // Approximate advance: 0.6 em per character (layout only needs
        // plausible widths here; the assertions are about pixels).
        let s = unsafe { CStr::from_ptr(text) }.to_string_lossy();
        s.chars().count() as f32 * 6.0
    }

    extern "C" fn test_metrics(
        _ctx: *mut c_void,
        _size: f32,
        _monospace: c_int,
        ascent: *mut f32,
        descent: *mut f32,
        _family: *const c_char,
    ) {
        unsafe {
            *ascent = 0.8;
            *descent = 0.2;
        }
    }

    /// Family-aware fake: runs measured for a NAMED family are wider than
    /// the family-blind default — mirroring a real shell, where the host
    /// default face is narrower than the page's face (Georgia).
    extern "C" fn aware_measure(
        _ctx: *mut c_void,
        text: *const c_char,
        _size: f32,
        _weight: u32,
        _italic: c_int,
        _monospace: c_int,
        family: *const c_char,
    ) -> f32 {
        let s = unsafe { CStr::from_ptr(text) }.to_string_lossy();
        let named = unsafe { CStr::from_ptr(family) }.to_string_lossy();
        let per_char = if named.is_empty() { 6.0 } else { 8.0 };
        s.chars().count() as f32 * per_char
    }

    /// Q11 contract: the shell paints every run in the run's own family, so
    /// the word-width path layout positions with (`measure_run_family`)
    /// must carry the family to the host. Dropping it measured every
    /// named-family run in the host default font — painted glyphs then
    /// overflowed their layout slots and inter-word spaces collapsed
    /// (the start page hero read "MakeKitaeyours.").
    #[test]
    fn run_measurement_is_family_aware() {
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: aware_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let m = CbMeasurer { vt };
        assert_eq!(
            m.measure_run_family("Make", 64.0, 600, false, false, "Georgia"),
            4.0 * 8.0,
            "named-family runs measure in that family, not the host default"
        );
        // The family-blind entry point stays family-blind by contract.
        assert_eq!(m.measure("Make", 64.0, 600, false, false), 4.0 * 6.0);
    }

    fn wait_for_frame(engine: &Engine) -> Vec<u8> {
        for _ in 0..400 {
            let len = kitae_engine_frame(
                engine as *const Engine as *mut Engine,
                std::ptr::null_mut(),
                0,
            );
            if len > 0 {
                let mut buf = vec![0u8; len];
                let got = kitae_engine_frame(
                    engine as *const Engine as *mut Engine,
                    buf.as_mut_ptr(),
                    buf.len(),
                );
                assert_eq!(got, len, "frame consumed on the second call");
                return buf;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("no frame published within 10s");
    }

    #[test]
    fn malformed_editing_commands_are_ignored() {
        assert_eq!(parse_offset_arg(-1.0), None);
        assert_eq!(parse_offset_arg(1.5), None);
        assert_eq!(parse_offset_arg(f32::NAN), None);
        assert_eq!(parse_offset_arg(f32::INFINITY), None);
        assert_eq!(parse_offset_arg(3.0), Some(3));

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(320, 240, &vt);
        assert!(!engine.is_null());
        assert!(matches!(
            parse_command(
                unsafe { &*engine },
                KITAE_CMD_EXTEND_CARET,
                2.0,
                0.0,
                std::ptr::null(),
            ),
            Some(Command::ExtendCaret(2))
        ));
        assert!(matches!(
            parse_command(
                unsafe { &*engine },
                KITAE_CMD_SELECTION_BEGIN_POINT,
                12.0,
                7.0,
                std::ptr::null(),
            ),
            Some(Command::SelectionBeginPoint(x, y)) if x == 12.0 && y == 7.0
        ));
        let _ = wait_for_frame(unsafe { &*engine });
        for value in [-1.0, 1.5, f32::NAN, f32::INFINITY] {
            kitae_engine_command(
                engine,
                KITAE_CMD_SELECTION_BEGIN,
                value,
                0.0,
                std::ptr::null(),
            );
            kitae_engine_command(
                engine,
                KITAE_CMD_SELECTION_EXTEND,
                value,
                0.0,
                std::ptr::null(),
            );
        }
        kitae_engine_command(
            engine,
            KITAE_CMD_SELECTION_BEGIN,
            0.0,
            0.0,
            std::ptr::null(),
        );
        assert_eq!(
            kitae_engine_frame(engine, std::ptr::null_mut(), 0),
            0,
            "malformed offsets do not enqueue a frame"
        );
        kitae_engine_destroy(engine);
    }

    /// The color of the text run carrying `needle` in a v20 frame, and the
    /// frame's `flags` word. Walks the rect list (paint tags vary) and then
    /// the text list, which is fixed-shape once the family string is skipped.
    fn run_color(frame: &[u8], needle: &str) -> (u32, u32) {
        let flags = u32::from_le_bytes(frame[4..8].try_into().unwrap());
        let mut p = 20usize; // version, flags, scroll, content_h, bg
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len; // url, title
        }
        let counts: Vec<usize> = (0..4)
            .map(|index| {
                u32::from_le_bytes(
                    frame[p + index * 4..p + (index + 1) * 4]
                        .try_into()
                        .unwrap(),
                ) as usize
            })
            .collect();
        p += 16;
        for _ in 0..counts[0] {
            p = skip_v7_rect(frame, p);
        }
        for _ in 0..counts[1] {
            // x, y, size, weight, italic, monospace (6 × 4 bytes), then color.
            let color = u32::from_le_bytes(frame[p + 24..p + 28].try_into().unwrap());
            p += 32; // …color + letter_spacing
            let family_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + family_len;
            let text_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4;
            let text = String::from_utf8(frame[p..p + text_len].to_vec()).unwrap();
            p += text_len;
            if text.contains(needle) {
                return (color, flags);
            }
        }
        panic!("no run matching {needle:?}");
    }

    /// H3 print over the wire: command 52 re-lays the page out for paper and
    /// the reply frame says so (`flags` bit2), carrying the print-styled
    /// colors; command 52 with a = 0 puts the screen frame back.
    #[test]
    fn set_media_command_round_trips_print_and_screen() {
        let dir = std::env::temp_dir().join(format!("kitae_print_ffi_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><head><style>\
             @media screen { p { color: #020202 } }\
             @media print { p { color: #010101 } }\
             </style></head><body><p>onpaper</p></body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let screen = wait_for_frame_url(unsafe { &*engine }, "page.html");
        assert_eq!(u32::from_le_bytes(screen[0..4].try_into().unwrap()), 20);
        let (color, flags) = run_color(&screen, "onpaper");
        assert_eq!(color, 0x020202FF, "the screen block styled the page");
        assert_eq!(flags & 4, 0, "a screen frame leaves bit2 clear");

        // Print media: a fresh frame, flagged, carrying the print color.
        kitae_engine_command(engine, KITAE_CMD_SET_MEDIA, 1.0, 0.0, std::ptr::null());
        let printed = loop {
            let frame = wait_for_frame(unsafe { &*engine });
            if u32::from_le_bytes(frame[4..8].try_into().unwrap()) & 4 != 0 {
                break frame;
            }
        };
        let (color, flags) = run_color(&printed, "onpaper");
        assert_eq!(color, 0x010101FF, "the print block styled the page");
        assert_eq!(flags & 4, 4, "the print frame advertises itself");

        // Back to screen media: the screen block applies again, bit2 clears.
        kitae_engine_command(engine, KITAE_CMD_SET_MEDIA, 0.0, 0.0, std::ptr::null());
        let restored = loop {
            let frame = wait_for_frame(unsafe { &*engine });
            if u32::from_le_bytes(frame[4..8].try_into().unwrap()) & 4 == 0 {
                break frame;
            }
        };
        let (color, flags) = run_color(&restored, "onpaper");
        assert_eq!(color, 0x020202FF, "screen styles are back");
        assert_eq!(flags & 4, 0);

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An appearance change mid-print must not knock the page off paper: the
    /// wire's SET_ENVIRONMENT carries the `prefers-*` half only.
    #[test]
    fn set_environment_leaves_print_media_alone() {
        let dir = std::env::temp_dir().join(format!("kitae_print_env_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><head><style>@media print { p { color: #010101 } }</style></head>\
             <body><p>onpaper</p></body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let _ = wait_for_frame_url(unsafe { &*engine }, "page.html");
        kitae_engine_command(engine, KITAE_CMD_SET_MEDIA, 1.0, 0.0, std::ptr::null());
        let _ = wait_for_frame(unsafe { &*engine });
        // Dark scheme + reduce motion while the print frame is current.
        kitae_engine_command(
            engine,
            KITAE_CMD_SET_ENVIRONMENT,
            1.0,
            1.0,
            std::ptr::null(),
        );
        let frame = loop {
            let frame = wait_for_frame(unsafe { &*engine });
            if u32::from_le_bytes(frame[4..8].try_into().unwrap()) & 4 != 0 {
                break frame;
            }
        };
        let (color, _) = run_color(&frame, "onpaper");
        assert_eq!(
            color, 0x010101FF,
            "still print-styled after the scheme flip"
        );
        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Wait for a published frame whose URL ends with `suffix` — the mailbox
    /// keeps only the newest frame, so early polls can return the boot page.
    fn wait_for_frame_url(engine: &Engine, suffix: &str) -> Vec<u8> {
        for _ in 0..400 {
            let frame = wait_for_frame_inner(engine);
            if frame.is_empty() {
                continue;
            }
            let p = 20usize; // version, flags, scroll, content_h, bg
            let url_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            let url = String::from_utf8(frame[p + 4..p + 4 + url_len].to_vec()).unwrap();
            if url.ends_with(suffix) {
                return frame;
            }
        }
        panic!("no frame for {suffix} within 10s");
    }

    #[test]
    fn base_href_reaches_v20_link_urls() {
        let dir = std::env::temp_dir().join(format!("kitae_base_ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><head><base href='assets/'></head>\
             <body><a href='next.html'>next</a><img src='pic.svg' alt='pic'></body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");
        assert_eq!(u32::from_le_bytes(frame[0..4].try_into().unwrap()), 20);

        let mut p = 20usize;
        let url_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + url_len;
        let title_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + title_len;
        let counts: Vec<usize> = (0..4)
            .map(|index| {
                u32::from_le_bytes(
                    frame[p + index * 4..p + (index + 1) * 4]
                        .try_into()
                        .unwrap(),
                ) as usize
            })
            .collect();
        p += 16;
        for _ in 0..counts[0] {
            p = skip_v7_rect(&frame, p);
        }
        for _ in 0..counts[1] {
            p += 8 * 4;
            let family_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + family_len;
            let text_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + text_len;
        }
        let href_len = u32::from_le_bytes(frame[p + 16..p + 20].try_into().unwrap()) as usize;
        let href = String::from_utf8(frame[p + 20..p + 20 + href_len].to_vec()).unwrap();
        assert_eq!(href, format!("file://{}/assets/next.html", dir.display()));
        assert_eq!(counts[2], 1);
        p += 20 + href_len;
        p += 20;
        let src_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        let src = String::from_utf8(frame[p + 4..p + 4 + src_len].to_vec()).unwrap();
        assert_eq!(src, format!("file://{}/assets/pic.svg", dir.display()));
        assert_eq!(counts[3], 1);
        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn ffi_editing_commands_round_trip_on_v20() {
        let dir = std::env::temp_dir().join(format!("kitae_a11ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><body><input id='q' value='abcdef'></body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(400, 600, &vt);
        assert!(!engine.is_null());
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");
        assert_eq!(u32::from_le_bytes(frame[0..4].try_into().unwrap()), 20);

        kitae_engine_command(engine, KITAE_CMD_CLICK, 30.0, 25.0, std::ptr::null());
        let _ = wait_for_frame(unsafe { &*engine });
        kitae_engine_command(
            engine,
            KITAE_CMD_SELECTION_BEGIN,
            1.0,
            0.0,
            std::ptr::null(),
        );
        let _ = wait_for_frame(unsafe { &*engine });
        kitae_engine_command(
            engine,
            KITAE_CMD_SELECTION_EXTEND,
            4.0,
            0.0,
            std::ptr::null(),
        );
        let _ = wait_for_frame(unsafe { &*engine });
        kitae_engine_command(engine, KITAE_CMD_SELECTION_END, 0.0, 0.0, std::ptr::null());
        let replacement = std::ffi::CString::new("X").unwrap();
        kitae_engine_command(engine, KITAE_CMD_TYPE, 0.0, 0.0, replacement.as_ptr());
        let frame = wait_for_frame(unsafe { &*engine });
        assert!(
            frame.windows(4).any(|w| w == b"aXef"),
            "replacement in v20 frame"
        );

        kitae_engine_command(engine, KITAE_CMD_SET_CARET, 1.0, 0.0, std::ptr::null());
        let _ = wait_for_frame(unsafe { &*engine });
        let marked = std::ffi::CString::new("Q").unwrap();
        kitae_engine_command(
            engine,
            KITAE_CMD_COMPOSITION_UPDATE,
            0.0,
            0.0,
            marked.as_ptr(),
        );
        let frame = wait_for_frame(unsafe { &*engine });
        assert!(
            frame.windows(5).any(|w| w == b"aQXef"),
            "marked text preview"
        );
        kitae_engine_command(
            engine,
            KITAE_CMD_COMPOSITION_COMMIT,
            0.0,
            0.0,
            std::ptr::null(),
        );
        let _ = wait_for_frame(unsafe { &*engine });

        let cancel_marked = std::ffi::CString::new("RK").unwrap();
        kitae_engine_command(
            engine,
            KITAE_CMD_COMPOSITION_UPDATE,
            0.0,
            0.0,
            cancel_marked.as_ptr(),
        );
        let frame = wait_for_frame(unsafe { &*engine });
        assert!(
            frame.windows(2).any(|w| w == b"RK"),
            "marked text is present before cancel"
        );
        kitae_engine_command(
            engine,
            KITAE_CMD_COMPOSITION_CANCEL,
            0.0,
            0.0,
            std::ptr::null(),
        );
        let frame = wait_for_frame(unsafe { &*engine });
        assert!(
            !frame.windows(2).any(|w| w == b"RK"),
            "cancel removes marked text"
        );

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn ffi_ax_semantics_tail_is_optional_and_v20_compatible() {
        let dir = std::env::temp_dir().join(format!("kitae_axtail_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><body><h2>Heading</h2><input value=\"hello\">\
             <input type=\"range\" min=\"2\" max=\"8\" value=\"5\">\
             <button disabled>Off</button><div aria-live=\"polite\">Saved</div>\
             </body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(400, 600, &vt);
        assert!(!engine.is_null());
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");
        assert_eq!(u32::from_le_bytes(frame[0..4].try_into().unwrap()), 20);

        let tail_start = frame
            .windows(4)
            .rposition(|window| window == b"AXS1")
            .expect("AX semantics tail");
        assert!(tail_start >= 4);
        assert_eq!(
            u32::from_le_bytes(frame[tail_start - 4..tail_start].try_into().unwrap()),
            0,
            "a zero legacy scroll count precedes the new tail"
        );
        assert_eq!(
            u32::from_le_bytes(frame[tail_start + 4..tail_start + 8].try_into().unwrap()),
            1,
            "tail version"
        );
        let payload_len =
            u32::from_le_bytes(frame[tail_start + 8..tail_start + 12].try_into().unwrap()) as usize;
        assert_eq!(tail_start + 12 + payload_len, frame.len());
        let mut p = tail_start + 12;
        let count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert!(count >= 4);
        p += 4;
        let mut saw_heading = false;
        let mut saw_disabled = false;
        let mut saw_range = false;
        let mut saw_live = false;
        for _ in 0..count {
            let _index = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
            let flags = frame[p + 4];
            let _checked = frame[p + 5];
            let heading = frame[p + 6];
            let live = frame[p + 7];
            let min = f32::from_le_bytes(frame[p + 8..p + 12].try_into().unwrap());
            let max = f32::from_le_bytes(frame[p + 12..p + 16].try_into().unwrap());
            let value_len = u32::from_le_bytes(frame[p + 16..p + 20].try_into().unwrap()) as usize;
            p += 20;
            let value = String::from_utf8(frame[p..p + value_len].to_vec()).unwrap();
            p += value_len;
            saw_heading |= heading == 2;
            saw_disabled |= flags & 1 != 0;
            saw_range |= flags & 64 != 0
                && (min - 2.0).abs() < 0.01
                && (max - 8.0).abs() < 0.01
                && value == "5";
            saw_live |= live == 1;
        }
        assert_eq!(p, frame.len());
        assert!(saw_heading, "heading level reaches the tail");
        assert!(saw_disabled, "disabled state reaches the tail");
        assert!(saw_range, "range value and bounds reach the tail");
        assert!(saw_live, "live politeness reaches the tail");

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Skip one v7 rect entry (box + color + 4 radii + paint tag/payload).
    fn skip_v7_rect(frame: &[u8], mut p: usize) -> usize {
        p += 4 * 4 + 4 + 4 * 4;
        match frame[p] {
            0 => p + 1,
            1 => {
                let n = u32::from_le_bytes(frame[p + 5..p + 9].try_into().unwrap()) as usize;
                p + 9 + n * 8
            }
            2 => {
                let mut q = p + 9;
                q += if frame[q] == 1 { 5 } else { 1 };
                let n = u32::from_le_bytes(frame[q..q + 4].try_into().unwrap()) as usize;
                q + 4 + n * 8
            }
            3 => {
                let len = u32::from_le_bytes(frame[p + 1..p + 5].try_into().unwrap()) as usize;
                p + 5 + len
            }
            t => panic!("unknown paint tag {t}"),
        }
    }

    /// Skip the v8 tail (find section + AX side-table), returning the end
    /// offset: match count + active index, per-match rect lists, then the
    /// AX node list and focused id.
    fn skip_v8_tail(frame: &[u8], p: usize) -> usize {
        skip_ax_section(frame, skip_find_section(frame, p))
    }

    /// Skip the v8 find section: match count + active index, then per-match
    /// rect lists.
    fn skip_find_section(frame: &[u8], mut p: usize) -> usize {
        let matches = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 8; // count + active index
        for _ in 0..matches {
            let n = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + n * 16;
        }
        p
    }

    /// Skip the v8 AX side-table: node count, per-node id + role + label +
    /// rect + parent index, then the focused id string.
    fn skip_ax_section(frame: &[u8], mut p: usize) -> usize {
        let nodes = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        for _ in 0..nodes {
            let id_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + id_len + 1; // id + role wire id
            let label_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + label_len + 16 + 4; // label + rect + parent index
        }
        let focused_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + focused_len;
        // v9 trails one byte (qtsq_locked); v11 a second (qtsq_sig); v12 a
        // sparse transform section (count + 29-byte entries: kind, index,
        // 6 floats).
        p += 2;
        let mats = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p + 4 + mats * 29
    }

    /// Skip the v16 per-item filter tail: count, then per item (kind,
    /// index, function count, function[]). The v15 sticky section sits
    /// before it on real frames, so callers need to first advance past
    /// the sticky tail before invoking this.
    fn skip_filter_section(frame: &[u8], mut p: usize) -> usize {
        let n = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        for _ in 0..n {
            p += 1 + 4; // kind + index
            let fns = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4;
            for _ in 0..fns {
                // One tag byte + 4 or 16 bytes of args (drop-shadow = 16;
                // everything else is one f32). Variable size by tag.
                let tag = frame[p];
                p += 1;
                match tag {
                    1 => p += 16, // drop-shadow: dx, dy, blur, color
                    _ => p += 4,  // all others: one f32
                }
            }
        }
        p
    }

    /// Byte size of one v18 clip descriptor (tag + payload), starting at the
    /// tag byte. Tags: 1 circle (3×f32), 2 ellipse (4×f32), 3 inset
    /// (4×f32 + 8×f32 radii), 4 polygon (evenodd u8 + count u32 + 2n×f32).
    /// Tag 0 carries B8 blend/group-only entries.
    fn clip_desc_len(frame: &[u8], p: usize) -> usize {
        match frame[p] {
            0 => 1,
            1 => 13,
            2 => 17,
            3 => 49,
            4 => {
                let n = u32::from_le_bytes(frame[p + 5..p + 9].try_into().unwrap()) as usize;
                9 + n * 8
            }
            t => panic!("unknown clip tag {t}"),
        }
    }

    /// Skip the v18 per-item clip/blend/group tail: count, then per entry
    /// (kind u8, index u32, clip descriptor, blend u8, group u32).
    fn skip_clip_section(frame: &[u8], mut p: usize) -> usize {
        let n = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        for _ in 0..n {
            p += 1 + 4; // kind + index
            p += clip_desc_len(frame, p);
            p += 1; // blend byte
            p += 4; // group u32
        }
        p
    }

    fn skip_ax_semantics_tail(frame: &[u8], p: usize) -> usize {
        let start = p;
        let mut cursor = p;
        if cursor + 4 <= frame.len()
            && u32::from_le_bytes(frame[cursor..cursor + 4].try_into().unwrap()) == 0
        {
            cursor += 4;
        }
        if cursor + 4 <= frame.len() && &frame[cursor..cursor + 4] == b"AXS1" {
            cursor += 4;
            let _version = u32::from_le_bytes(frame[cursor..cursor + 4].try_into().unwrap());
            cursor += 4;
            let payload =
                u32::from_le_bytes(frame[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4 + payload;
            return cursor;
        }
        start
    }

    fn read_f32s(frame: &[u8], p: usize, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| f32::from_le_bytes(frame[p + i * 4..p + i * 4 + 4].try_into().unwrap()))
            .collect()
    }

    /// One decoded v18 clip entry, as the host would see it.
    struct ClipWire {
        kind: u8,
        index: u32,
        desc: Vec<f32>,
        evenodd: bool,
        blend: u8,
        group: u32,
    }

    /// Decode the v18 clip tail into structured entries (for assertions).
    fn read_clip_section(frame: &[u8], mut p: usize) -> Vec<ClipWire> {
        let n = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        let mut out = Vec::new();
        for _ in 0..n {
            let kind = frame[p];
            let index = u32::from_le_bytes(frame[p + 1..p + 5].try_into().unwrap());
            p += 5;
            let (desc, evenodd) = match frame[p] {
                1 => (read_f32s(frame, p + 1, 3), false),
                2 => (read_f32s(frame, p + 1, 4), false),
                3 => (read_f32s(frame, p + 1, 12), false),
                4 => {
                    let evenodd = frame[p + 1] != 0;
                    let cnt = u32::from_le_bytes(frame[p + 2..p + 6].try_into().unwrap()) as usize;
                    (read_f32s(frame, p + 6, cnt * 2), evenodd)
                }
                0 => (Vec::new(), false),
                t => panic!("unknown clip tag {t}"),
            };
            p += clip_desc_len(frame, p);
            let blend = frame[p];
            let group = u32::from_le_bytes(frame[p + 1..p + 5].try_into().unwrap());
            p += 5;
            out.push(ClipWire {
                kind,
                index,
                desc,
                evenodd,
                blend,
                group,
            });
        }
        out
    }

    fn wait_for_frame_inner(engine: &Engine) -> Vec<u8> {
        loop {
            let len = kitae_engine_frame(
                engine as *const Engine as *mut Engine,
                std::ptr::null_mut(),
                0,
            );
            if len > 0 {
                let mut buf = vec![0u8; len];
                let got = kitae_engine_frame(
                    engine as *const Engine as *mut Engine,
                    buf.as_mut_ptr(),
                    buf.len(),
                );
                if got > 0 {
                    // `got` is the frame's current size at consume time:
                    // either the probed frame (got == len) or a newer,
                    // smaller one that was published between the probe and
                    // the consume — fully written (got < len). A larger
                    // replacement refuses the undersized buffer, consumes
                    // nothing and reports its own size (got > len), so a
                    // bigger allocation tries again.
                    if got <= len {
                        buf.truncate(got);
                        return buf;
                    }
                    continue;
                }
                // got == 0 after a nonzero probe: the mailbox emptied
                // between the calls — keep polling.
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    #[test]
    fn ffi_round_trips_qtsq_image_bytes() {
        let _store = crate::qtsq_test_lock();
        let dir = std::env::temp_dir().join(format!("kitae_j3ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::copy(
            format!("{FIXTURE_DIR}/fix_image.qtsq"),
            dir.join("photo.qtsq"),
        )
        .expect("copy fixture");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();
        std::fs::write(
            dir.join("page.html"),
            "<html><body><img src=\"photo.qtsq\" alt=\"photo\"></body></html>",
        )
        .expect("write page");

        crate::clear_qtsq_rasters();
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        assert!(!engine.is_null());
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());

        // URL-gated: the boot page frame could be the first non-empty one
        // under load, and it carries no image entries.
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");
        // Sequential walk matching the wire format in include/kitae_core.h.
        assert_eq!(
            u32::from_le_bytes(frame[0..4].try_into().unwrap()),
            20,
            "frame format version 20"
        );
        let mut p = 4 + 4 + 4 + 4 + 4; // version, flags, scroll, content_h, bg
        let url_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + url_len;
        let title_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + title_len;
        let (rects, texts, links, images) = (
            u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize,
            u32::from_le_bytes(frame[p + 4..p + 8].try_into().unwrap()) as usize,
            u32::from_le_bytes(frame[p + 8..p + 12].try_into().unwrap()) as usize,
            u32::from_le_bytes(frame[p + 12..p + 16].try_into().unwrap()) as usize,
        );
        p += 16;
        assert_eq!(
            (rects, texts, links, images),
            (1, 1, 0, 1),
            "placeholder rect + alt text + one img"
        );
        // v7 rects: box + color + 4 radii + paint tag/payload.
        for _ in 0..rects {
            p = skip_v7_rect(&frame, p);
        }
        // texts: f32 x, y, size; u32 weight, italic, monospace, color; f32
        // letter_spacing (v10); str family (v14); u32 len; bytes
        for _ in 0..texts {
            p += 8 * 4; // fixed fields through letter_spacing
            let family_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + family_len;
            let text_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + text_len;
        }
        // links: f32 x, y, w, h; u32 len; bytes
        for _ in 0..links {
            let href_len = u32::from_le_bytes(frame[p + 16..p + 20].try_into().unwrap()) as usize;
            p += 16 + 4 + href_len;
        }
        // The single image entry: f32 x,y,w,h; u32 fit; src; alt; u64 payload;
        // u8 animated (v13 — a QTSQ still is never animated).
        let e = &frame[p..];
        let mut q = 16 + 4; // x, y, w, h, fit
        let src_len = u32::from_le_bytes(e[q..q + 4].try_into().unwrap()) as usize;
        q += 4 + src_len;
        let alt_len = u32::from_le_bytes(e[q..q + 4].try_into().unwrap()) as usize;
        q += 4 + alt_len;
        let payload = u64::from_le_bytes(e[q..q + 8].try_into().unwrap());
        assert!(
            payload != 0,
            "QTSQ image carries a raster id, got {payload}"
        );
        assert_eq!(e[q + 8], 0, "still image not flagged animated");
        assert!(
            payload != 0,
            "QTSQ image carries a raster id, got {payload}"
        );
        // v3 tail: focused + selection strings (both empty here); v5 appends
        // the one-byte media-absent flag (0 = no media on this page); v6
        // appends the media-item count (0 = no `<video>` on this page); v7
        // appends the shadow count (0 = no shadows on this page); v8
        // appends the find section (0 matches, inactive) and the AX tree.
        let rest = &e[q + 8 + 1..]; // payload + v13 animated byte
        let mut p = 0;
        assert!(
            rest.iter().take(8).all(|&b| b == 0),
            "empty focused/selection"
        );
        p += 8;
        assert_eq!(rest[p], 0, "media-absent flag");
        p += 1;
        assert_eq!(&rest[p..p + 4], &[0, 0, 0, 0], "zero media items");
        p += 4;
        assert_eq!(&rest[p..p + 4], &[0, 0, 0, 0], "zero shadows");
        p += 4;
        assert_eq!(
            &rest[p..p + 4],
            &[0, 0, 0, 0],
            "zero find matches, no query"
        );
        p += 4;
        assert_eq!(&rest[p..p + 4], &[0xFF, 0xFF, 0xFF, 0xFF], "find inactive");
        p += 4;
        let ax_count = u32::from_le_bytes(rest[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        // The photo page exposes a Document root and the image node.
        assert!(ax_count >= 2, "document + image nodes, got {ax_count}");
        let mut saw_image = false;
        for _ in 0..ax_count {
            let id_len = u32::from_le_bytes(rest[p..p + 4].try_into().unwrap()) as usize;
            p += 4;
            let id = std::str::from_utf8(&rest[p..p + id_len])
                .unwrap()
                .to_string();
            p += id_len;
            let role = rest[p];
            p += 1;
            let label_len = u32::from_le_bytes(rest[p..p + 4].try_into().unwrap()) as usize;
            p += 4;
            let label = std::str::from_utf8(&rest[p..p + label_len])
                .unwrap()
                .to_string();
            p += label_len + 16 + 4; // rect + parent index
            if role == 7 {
                saw_image = true;
                assert_eq!(label, "photo", "img alt is the AX label");
                let _ = id;
            }
        }
        assert!(saw_image, "image node in the AX side-table");
        let focused_len = u32::from_le_bytes(rest[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + focused_len;
        p += 1; // v9 trailing qtsq_locked flag
        p += 1; // v11 trailing qtsq_sig verdict byte
                // v12: sparse transform tail — this page has none.
        let mats = u32::from_le_bytes(rest[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(mats, 0, "untransformed page carries no matrix entries");
        p += 4;
        // v15: sticky tail — this page has none.
        let stickies = u32::from_le_bytes(rest[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(stickies, 0, "unsticky page carries no sticky entries");
        p += 4;
        // v16: per-item filter tail — also empty here.
        p = skip_filter_section(rest, p);
        // v17: scroll-container tail.
        let scroll_count = u32::from_le_bytes(rest[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(scroll_count, 0, "no scroll containers on this page");
        p += 4 + scroll_count * 24;
        p = skip_clip_section(rest, p);
        // v19: per-text blur tail — this page carries none.
        let blurs = u32::from_le_bytes(rest[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(blurs, 0, "no text-shadow blur runs on this page");
        p += 4 + blurs * 8;
        assert_eq!(p, rest.len(), "blur section ends the frame");

        // Resolve the id through the C exports — the embedder's actual path.
        let total = kitae_qtsq_raster_size(payload);
        assert_eq!(total, 8 + 8 * 8 * 4, "8 + w*h*4 for the 8×8 fixture");
        let mut raster = vec![0u8; total];
        assert_eq!(
            kitae_qtsq_raster(payload, raster.as_mut_ptr(), raster.len()),
            1
        );
        assert_eq!(
            u32::from_le_bytes(raster[0..4].try_into().unwrap()),
            8,
            "width"
        );
        assert_eq!(
            u32::from_le_bytes(raster[4..8].try_into().unwrap()),
            8,
            "height"
        );
        assert_eq!(&raster[8..12], &[1, 2, 3, 255], "first RGBA pixel");
        // Undersized buffer is refused and leaves the raster valid.
        assert_eq!(kitae_qtsq_raster(payload, raster.as_mut_ptr(), 4), 0);
        assert_eq!(kitae_qtsq_raster_size(u64::MAX), 0, "unknown id");

        kitae_engine_destroy(engine);
        crate::clear_qtsq_rasters();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ffi_round_trips_transform_matrices() {
        // Q2: a rotated box's rect, text and link ride the sparse v12
        // tail with their 2×3 matrices.
        let dir = std::env::temp_dir().join(format!("kitae_q2ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><body><div style=\"width:100px; height:20px; background:#369; transform-origin:0 0; transform:rotate(90deg)\"><a href='mailto:rotated@example.com'>hi</a></div></body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");
        assert_eq!(u32::from_le_bytes(frame[0..4].try_into().unwrap()), 20);

        let mut p = 20usize;
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        let counts: Vec<usize> = (0..4)
            .map(|index| {
                u32::from_le_bytes(
                    frame[p + index * 4..p + (index + 1) * 4]
                        .try_into()
                        .unwrap(),
                ) as usize
            })
            .collect();
        p += 16;
        for _ in 0..counts[0] {
            p = skip_v7_rect(&frame, p);
        }
        for _ in 0..counts[1] {
            p += 8 * 4;
            for _ in 0..2 {
                let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
                p += 4 + len;
            }
        }
        for _ in 0..counts[2] {
            let len = u32::from_le_bytes(frame[p + 16..p + 20].try_into().unwrap()) as usize;
            p += 20 + len;
        }
        for _ in 0..counts[3] {
            p += 20;
            for _ in 0..2 {
                let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
                p += 4 + len;
            }
            p += 9;
        }
        p += 8 + 1;
        let media_items = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(media_items, 0);
        p += 4;
        let shadows = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + shadows * (16 + 4 + 16 + 16 + 1);

        let mut transform_end = skip_v8_tail(&frame, p);
        assert_eq!(transform_end + 20, frame.len(), "empty trailing sections");
        let n = 4usize;
        let at = transform_end - 4 - n * 29;
        let got = u32::from_le_bytes(frame[at..at + 4].try_into().unwrap()) as usize;
        assert_eq!(got, n, "transform count matches after AX tail");
        for name in ["sticky", "filter", "scroll", "clip", "blur"] {
            let count =
                u32::from_le_bytes(frame[transform_end..transform_end + 4].try_into().unwrap());
            assert_eq!(count, 0, "empty {name} tail");
            transform_end += 4;
        }
        let mut p = at + 4;
        let mut kinds: Vec<u8> = Vec::new();
        let mut link_matrix = None;
        let mut text_matrix = None;
        for _ in 0..n {
            let e = &frame[p..p + 29];
            let kind = e[0];
            let idx = u32::from_le_bytes(e[1..5].try_into().unwrap());
            let mut m = [0f32; 6];
            for (j, f) in m.iter_mut().enumerate() {
                *f = f32::from_le_bytes(e[5 + j * 4..9 + j * 4].try_into().unwrap());
            }
            match kind {
                1 => {
                    assert_eq!(idx, 0, "text: the div's run is the only one");
                    text_matrix = Some(m);
                }
                4 => {
                    assert_eq!(idx, 0, "link: the anchor is the only one");
                    link_matrix = Some(m);
                }
                _ => {}
            }
            kinds.push(kind);
            p += 29;
        }
        assert!(kinds.contains(&0) && kinds.contains(&1) && kinds.contains(&4));
        let link = link_matrix.expect("link transform entry");
        let text = text_matrix.expect("text transform entry");
        assert_eq!(link, text, "one box transform rides every item kind");
        assert!(link[0].abs() < 1e-4 && (link[1] + 1.0).abs() < 1e-4);
        assert!((link[3] - 1.0).abs() < 1e-4 && link[4].abs() < 1e-4);

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// B4: a page that applies `filter` carries the chain through the v16
    /// tail. Sparse — only items with a non-empty `filter` show up.
    #[test]
    fn ffi_filter_section_round_trip() {
        let _store = crate::qtsq_test_lock();
        let dir = std::env::temp_dir().join(format!("kitae_b4ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><body>             <p style=\"filter: blur(8px) brightness(1.2); background: #369; height: 40px\">card</p>             </body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");

        // Smoke check: the v16 filter section should contain a blur(8)
        // entry. The unique byte pattern is the tag byte 0x00 followed
        // by 8.0 in little-endian (0x00 0x00 0x00 0x41). Search the
        // tail of the frame for it.
        let needle: &[u8] = &[0x00, 0x00, 0x00, 0x00, 0x41];
        let pos = frame
            .windows(needle.len())
            .rposition(|w| w == needle)
            .expect("a blur tag + 8.0 LE prefix should appear in the v16 tail");
        let radius = f32::from_le_bytes(frame[pos + 1..pos + 5].try_into().unwrap());
        assert!((radius - 8.0).abs() < 0.01, "blur(8) reached the wire");
        // Frame is non-trivially sized.
        assert!(
            frame.len() > 300,
            "filtered frame is non-trivial (got {} bytes)",
            frame.len()
        );

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// B2: a page that applies `overflow: auto|scroll` to a box whose
    /// content is taller than the box records a ScrollContainer in the
    /// v17 tail. The host reads the (x, y, w, h, content_h) extents to
    /// paint a scrollbar and size the wheel target.
    #[test]
    fn ffi_scroll_container_section_round_trip() {
        let _store = crate::qtsq_test_lock();
        let dir = std::env::temp_dir().join(format!("kitae_b2ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><body>\
             <div style=\"overflow: auto; height: 40px; width: 300px\">\
             <p>line 1</p><p>line 2</p><p>line 3</p>\
             <p>line 4</p><p>line 5</p><p>line 6</p>\
             <p>line 7</p><p>line 8</p><p>line 9</p>\
             </div></body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");

        // The v17 section: u32 count + (node, x, y, w, h, content_h)
        // per entry. Each entry is 4 (node) + 5*4 (f32) = 24 bytes.
        // The optional B3 tail adds count + (content_w, mode, parent)
        // after the v19 blur tail.
        let scroll_count_off = frame.len() - 49;
        let entry_off = scroll_count_off + 4;
        let scroll_count =
            u32::from_le_bytes(frame[scroll_count_off..entry_off].try_into().unwrap()) as usize;
        assert_eq!(scroll_count, 1, "one scroll container");
        let node = u32::from_le_bytes(frame[entry_off..entry_off + 4].try_into().unwrap());
        // Entry layout: node(4), x(4), y(4), w(4), h(4), content_h(4).
        let w = f32::from_le_bytes(frame[entry_off + 12..entry_off + 16].try_into().unwrap());
        let h = f32::from_le_bytes(frame[entry_off + 16..entry_off + 20].try_into().unwrap());
        let content_h =
            f32::from_le_bytes(frame[entry_off + 20..entry_off + 24].try_into().unwrap());
        assert!(node > 0);
        assert!(
            (w - 300.0).abs() < 1.0,
            "viewport w is the box's width (got {w})"
        );
        assert!(
            (h - 40.0).abs() < 1.0,
            "viewport h is the box's height (got {h})"
        );
        assert!(
            content_h > h,
            "content_h exceeds the viewport (got {content_h})"
        );
        let b3_count =
            u32::from_le_bytes(frame[frame.len() - 13..frame.len() - 9].try_into().unwrap());
        assert_eq!(b3_count, 1, "B3 metadata follows the scroll entry");
        let content_w =
            f32::from_le_bytes(frame[frame.len() - 9..frame.len() - 5].try_into().unwrap());
        let mode = frame[frame.len() - 5];
        let parent = u32::from_le_bytes(frame[frame.len() - 4..].try_into().unwrap());
        assert!(content_w >= w - 1.0);
        assert_eq!(mode, 1, "overflow:auto is encoded as mode 1");
        assert_eq!(parent, u32::MAX, "top-level scroll container has no parent");

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ffi_hsts_snapshot_round_trip_and_rejects_malformed_data() {
        let host = CString::new("https://snapshot.example/").unwrap();
        assert!(kitae_net::observe_hsts(
            host.to_str().unwrap(),
            "max-age=120; includeSubDomains"
        ));
        let size = kitae_hsts_snapshot_size();
        assert!(size > 8);
        let mut bytes = vec![0u8; size];
        assert_eq!(kitae_hsts_snapshot(bytes.as_mut_ptr(), bytes.len()), size);
        kitae_clear_all_hsts();
        assert_eq!(kitae_hsts_import_snapshot(bytes.as_ptr(), bytes.len()), 1);
        let child = CString::new("https://sub.snapshot.example/").unwrap();
        assert_eq!(kitae_hsts_covers(child.as_ptr()), 1);
        assert_eq!(kitae_hsts_import_snapshot(bytes.as_ptr(), size - 1), 0);
        kitae_clear_all_hsts();
    }

    /// B7: the v18 clip tail — symbolic `clip-path`/mask shapes resolve
    /// against each element's border box, ride the wire as (kind, index,
    /// descriptor, blend), and the unclipped body background stays out.
    #[test]
    fn ffi_clip_section_round_trip() {
        let _store = crate::qtsq_test_lock();
        let dir = std::env::temp_dir().join(format!("kitae_b7ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><body>\
             <div style=\"clip-path: inset(10px 20px round 5px); background:#369; height:40px\">a</div>\
             <div style=\"clip-path: circle(10px at 10px 10px); background:#6c3; height:40px\"></div>\
             <div style=\"-webkit-mask: ellipse(20px 10px); background:#c36; height:40px\">b</div>\
             </body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");

        // This page carries no transforms, stickies, filters or scroll
        // containers, so those four count sections are zero-valued 4-byte
        // runs just before the v18 tail. Entry sizes here: inset =
        // 1+4+49+1+4, circle = 1+4+13+1+4, ellipse = 1+4+17+1+4 — three
        // clipped rects (divs 1..3) then two clipped text runs (a, b).
        // (Trailing +4 each: B8's group u32 joins every entry.)
        let entry_bytes: usize = (1 + 4 + 49 + 1 + 4)
            + (1 + 4 + 13 + 1 + 4)
            + (1 + 4 + 17 + 1 + 4)
            + (1 + 4 + 49 + 1 + 4)
            + (1 + 4 + 17 + 1 + 4);
        let at = frame.len() - 4 - 4 - entry_bytes; // v19 blur count follows
        let count = u32::from_le_bytes(frame[at..at + 4].try_into().unwrap()) as usize;
        assert!(
            frame[at - 16..at].iter().all(|&b| b == 0),
            "empty mats/sticky/filter/scroll section counts precede the clip tail"
        );
        assert_eq!(count, 5, "three clipped rects + two clipped text runs");
        assert_eq!(
            skip_clip_section(&frame, at),
            frame.len() - 4,
            "clip section precedes the blur tail"
        );
        let clips = read_clip_section(&frame, at);
        assert_eq!(clips.len(), 5);
        assert_eq!(
            clips.iter().map(|c| c.kind).collect::<Vec<_>>(),
            vec![0u8, 0, 0, 1, 1],
            "rects first, then texts"
        );
        assert_eq!(
            clips.iter().map(|c| c.index).collect::<Vec<_>>(),
            vec![1u32, 2, 3, 0, 1],
            "the body background rect (0) is never clipped"
        );
        assert!(
            clips.iter().all(|c| c.blend == 0),
            "blend slot reads normal (pre-B8)"
        );

        // Decode the four solid rect boxes to compare each clip against its
        // element's border box (header: version, flags, scroll, content_h,
        // page bg, url, title — then the four section counts).
        let mut p = 20usize;
        let url_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + url_len;
        let title_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + title_len;
        let n_rects = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(n_rects, 4, "body background + three div backgrounds");
        let mut boxes = Vec::new();
        let mut q = p + 16;
        for i in 0..n_rects {
            let xywh = read_f32s(&frame, q, 4);
            boxes.push((xywh[0], xywh[1], xywh[2], xywh[3]));
            q = skip_v7_rect(&frame, q);
            assert_eq!(frame[q - 1], 0, "rect {i} is a solid paint");
        }

        // div 1 (rect + text): inset(10px 20px round 5px) → offsets
        // 20/10 from the box origin, shrunk extent, uniform 5px radii.
        let (bx, by, bw, bh) = boxes[1];
        let e = &clips[0].desc;
        assert_eq!(e.len(), 12);
        assert_eq!(
            [e[0], e[1], e[2], e[3]],
            [bx + 20.0, by + 10.0, bw - 40.0, bh - 20.0]
        );
        assert!(e[4..].iter().all(|&r| r == 5.0), "uniform round 5px radii");
        assert_eq!(clips[3].desc, *e, "text 'a' carries the div's exact inset");
        // div 2: circle(10px at 10px 10px) → offsets from the box origin.
        assert_eq!(
            clips[1].desc,
            vec![boxes[2].0 + 10.0, boxes[2].1 + 10.0, 10.0]
        );
        // div 3's mask: ellipse(20px 10px) centered, fixed radii.
        let b3 = boxes[3];
        assert!(
            (clips[2].desc[0] - (b3.0 + b3.2 / 2.0)).abs() < 0.5
                && (clips[2].desc[1] - (b3.1 + b3.3 / 2.0)).abs() < 0.5,
            "ellipse centers in the masked box"
        );
        assert_eq!(&clips[2].desc[2..], &[20.0, 10.0], "rx/ry fold exactly");
        assert_eq!(clips[4].desc, clips[2].desc, "text 'b' shares div b's mask");

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ffi_blend_group_section_round_trip() {
        let _store = crate::qtsq_test_lock();
        let dir = std::env::temp_dir().join(format!("kitae_b8ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><body>\
             <div style=\"mix-blend-mode: multiply; background:#369; height:40px\">m</div>\
             <div style=\"isolation: isolate; background:#6c3; height:60px\">\
             <div style=\"background:#c36; height:40px\">c</div></div>\
             </body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");

        // Every B8 stamp on this page is clip-less: five entries of
        // kind(1)+index(4)+tag0(1)+blend(1)+group(4) = 11 bytes, ordered
        // rects (blend div, iso div, iso child) then texts ("m", "c").
        let entry_bytes = 5 * 11;
        let at = frame.len() - 4 - 4 - entry_bytes; // v19 blur count follows
        assert_eq!(
            u32::from_le_bytes(frame[at..at + 4].try_into().unwrap()),
            5,
            "blend + group entries end the frame"
        );
        assert!(
            frame[at - 16..at].iter().all(|&b| b == 0),
            "empty mats/sticky/filter/scroll counts precede the clip tail"
        );
        assert_eq!(
            skip_clip_section(&frame, at),
            frame.len() - 4,
            "tail consumes the frame (blur count remains)"
        );
        let entries = read_clip_section(&frame, at);
        assert_eq!(entries.len(), 5);
        assert!(
            entries.iter().all(|e| e.desc.is_empty()),
            "tag 0 = descriptor-less entry"
        );
        assert_eq!(
            entries.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![0u8, 0, 0, 1, 1]
        );
        assert_eq!(
            entries.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![1u32, 2, 3, 0, 1]
        );
        // multiply on the blend div + its text; group-only entries carry blend 0.
        assert_eq!(
            entries.iter().map(|e| e.blend).collect::<Vec<_>>(),
            vec![1u8, 0, 0, 1, 0]
        );
        let gid = entries[1].group;
        assert_ne!(gid, 0, "isolate stamps a nonzero group");
        assert_eq!(entries[2].group, gid, "the isolated child shares the group");
        assert_eq!(
            entries[4].group, gid,
            "text inside the isolate shares it too"
        );
        assert_eq!(entries[0].group, 0);
        assert_eq!(entries[3].group, 0);

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ffi_blur_tail_carries_text_shadow_blur_e3() {
        let _store = crate::qtsq_test_lock();
        let dir = std::env::temp_dir().join(format!("kitae_e3ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><body>\
             <p style=\"text-shadow: 2px 3px 4px #9000\">plain</p>\
             <p style=\"text-shadow: 0 0 0 #000\">crisp</p></body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");

        // Front walk (mirror of the wire doc) to the v19 blur tail.
        let mut p = 20usize; // version, flags, scroll, content_h, bg
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len; // url, title
        }
        let counts: Vec<usize> = (0..4)
            .map(|i| {
                u32::from_le_bytes(frame[p + i * 4..p + i * 4 + 4].try_into().unwrap()) as usize
            })
            .collect();
        let (rects, texts, links, images) = (counts[0], counts[1], counts[2], counts[3]);
        p += 16;
        for _ in 0..rects {
            p = skip_v7_rect(&frame, p);
        }
        for _ in 0..texts {
            p += 8 * 4;
            let family_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + family_len;
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        p += links * 20;
        for _ in 0..images {
            p += 16 + 4;
            for _ in 0..2 {
                let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
                p += 4 + len;
            }
            p += 8 + 1; // payload u64 + animated byte
        }
        // focused/selection + media + items + shadows.
        p += 8 + 1 + 4 + 4;
        // find + AX + ax_focused.
        p += 8;
        let ax_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        for _ in 0..ax_count {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len; // id
            p += 1; // role
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len + 16 + 4; // label + rect + parent
        }
        let focused_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + focused_len;
        p += 2; // v9 locked + v11 sig
                // v12 transform tail.
        let mats = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + mats * 29;
        // v15 sticky tail.
        const STICKY: usize = 12 * 4 + 6 * 4 + 2;
        let stickies = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + stickies * STICKY;
        // v16 filter tail + v17 scroll tail + v18 clip tail.
        p = skip_filter_section(&frame, p);
        let scrolls = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + scrolls * 24;
        p = skip_clip_section(&frame, p);

        // v19 blur tail: only the blurred shadow duplicate of "plain"
        // (blur 4px); the crisp shadow and every main run are absent.
        let blurs = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(blurs, 1, "one blurred shadow run, got {blurs}");
        let idx = u32::from_le_bytes(frame[p + 4..p + 8].try_into().unwrap());
        let blur = f32::from_le_bytes(frame[p + 8..p + 12].try_into().unwrap());
        assert!(
            blur > 3.9 && blur < 4.1,
            "blur radius 4 reaches the wire (got {blur})"
        );
        assert!(
            idx < texts as u32,
            "blur index points at a text run ({idx})"
        );
        p += 4 + blurs * 8;
        assert_eq!(p, frame.len(), "blur section ends the frame");

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ffi_sticky_side_list_round_trip() {
        let _store = crate::qtsq_test_lock();
        let dir = std::env::temp_dir().join(format!("kitae_q5ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><body><div style=\"height:60px\">lead</div><div style=\"position:sticky;top:12px;height:30px;background:#369\">head</div><div style=\"height:900px\">tail</div></body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");

        // Front walk (mirror of the wire doc) to the v15 sticky tail.
        let mut p = 20usize; // version, flags, scroll, content_h, bg
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len; // url, title
        }
        let counts: Vec<usize> = (0..4)
            .map(|i| {
                u32::from_le_bytes(frame[p + i * 4..p + i * 4 + 4].try_into().unwrap()) as usize
            })
            .collect();
        let (rects, texts, links, images) = (counts[0], counts[1], counts[2], counts[3]);
        p += 16;
        for _ in 0..rects {
            p = skip_v7_rect(&frame, p);
        }
        for _ in 0..texts {
            p += 8 * 4; // fixed fields through letter_spacing
            let family_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + family_len; // v14 family string
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        for _ in 0..links {
            let len = u32::from_le_bytes(frame[p + 16..p + 20].try_into().unwrap()) as usize;
            p += 16 + 4 + len;
        }
        for _ in 0..images {
            let src = u32::from_le_bytes(frame[p + 20..p + 24].try_into().unwrap()) as usize;
            let alt =
                u32::from_le_bytes(frame[p + 24 + src..p + 28 + src].try_into().unwrap()) as usize;
            p += 24 + src + alt + 8 + 1; // +1: v13 animated byte
        }
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len; // focused, selection
        }
        assert_eq!(frame[p], 0, "no media on this page");
        p += 1;
        let media_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(media_count, 0);
        p += 4;
        let shadow_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + shadow_count * (4 * 4 + 4 + 4 * 4 + 4 * 4 + 1);
        let find_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 8 + find_count * 4; // count, active, per-match rect counts
        for _ in 0..find_count {
            let rc = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + rc * 16;
        }
        let ax_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        for _ in 0..ax_count {
            let id_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + id_len + 1; // id, role
            let label_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + label_len + 16 + 4; // label, rect, parent
        }
        let ax_focused_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + ax_focused_len;
        p += 2; // v9 locked + v11 sig bytes
        let mats = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(mats, 0, "untransformed page");
        p += 4 + mats * 29;

        // v15 sticky tail.
        const ENTRY: usize = 12 * 4 + 6 * 4 + 2;
        let count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(count, 1, "one sticky box");
        let e = &frame[p + 4..p + 4 + ENTRY];
        p += 4 + count * ENTRY;
        // v16 filter tail.
        p = skip_filter_section(&frame, p);
        // v17: scroll-container tail.
        let scroll_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(scroll_count, 0, "no scroll containers on this page");
        p += 4 + scroll_count * 24;
        p = skip_clip_section(&frame, p);
        // v19: per-text blur tail — none on this page.
        let blurs = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(blurs, 0, "no blurred runs on the sticky page");
        p += 4 + blurs * 8;
        assert_eq!(p, frame.len(), "blur section ends the frame");
        let mut q = 0usize;
        let mut ranges = [(0u32, 0u32); 6];
        for r in &mut ranges {
            *r = (
                u32::from_le_bytes(e[q..q + 4].try_into().unwrap()),
                u32::from_le_bytes(e[q + 4..q + 8].try_into().unwrap()),
            );
            q += 8;
        }
        assert!(
            ranges[0].1 >= 1,
            "sticky paints at least its background rect"
        );
        assert!(ranges[1].1 >= 1, "sticky paints its text run");
        let f32_at = |q: usize| f32::from_le_bytes(e[q..q + 4].try_into().unwrap());
        let _base_y = f32_at(q);
        let height = f32_at(q + 4);
        let container_top = f32_at(q + 8);
        let container_bottom = f32_at(q + 12);
        let top_offset = f32_at(q + 16);
        assert!(
            (top_offset - 12.0).abs() < 0.5,
            "top offset reaches the wire"
        );
        assert!((height - 30.0).abs() < 1.0);
        assert_eq!(
            container_top, 0.0,
            "root sticky clamps against the document"
        );
        assert!(container_bottom > 900.0, "document bottom covers the tail");
        assert_eq!(e[ENTRY - 2], 1, "has_top");
        assert_eq!(e[ENTRY - 1], 0, "no bottom");

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ffi_inline_svg_data_url_carries_a_raster_payload() {
        let _store = crate::qtsq_test_lock();
        crate::clear_qtsq_rasters();
        let dir = std::env::temp_dir().join(format!("kitae_r6svg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            r##"<html><body><p>hi</p><svg width="40" height="20" viewBox="0 0 4 2"><rect width="4" height="2" fill="#12f"/></svg></body></html>"##,
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");

        // Front walk to the image entries (mirror of the wire doc) and
        // require the inline SVG's entry to carry a decoded raster.
        let mut p = 20usize;
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        let counts: Vec<usize> = (0..4)
            .map(|i| {
                u32::from_le_bytes(frame[p + i * 4..p + i * 4 + 4].try_into().unwrap()) as usize
            })
            .collect();
        let (rects, texts, links, images) = (counts[0], counts[1], counts[2], counts[3]);
        p += 16;
        for _ in 0..rects {
            p = skip_v7_rect(&frame, p);
        }
        for _ in 0..texts {
            p += 8 * 4;
            let family_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + family_len;
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        for _ in 0..links {
            let len = u32::from_le_bytes(frame[p + 16..p + 20].try_into().unwrap()) as usize;
            p += 16 + 4 + len;
        }
        assert!(images >= 1, "inline svg becomes an image entry");
        let mut saw_raster = false;
        for _ in 0..images {
            let src = u32::from_le_bytes(frame[p + 20..p + 24].try_into().unwrap()) as usize;
            let alt =
                u32::from_le_bytes(frame[p + 24 + src..p + 28 + src].try_into().unwrap()) as usize;
            let payload = u64::from_le_bytes(
                frame[p + 28 + src + alt..p + 36 + src + alt]
                    .try_into()
                    .unwrap(),
            );
            let animated = frame[p + 36 + src + alt];
            if src > 0 && payload != 0 {
                saw_raster = true;
                assert_eq!(animated, 0, "inline svg is never animated");
                let total = kitae_qtsq_raster_size(payload);
                assert!(total > 8, "raster registered: {total} bytes");
                let mut buf = vec![0u8; total];
                assert_eq!(
                    kitae_qtsq_raster(payload, buf.as_mut_ptr(), buf.len()),
                    1,
                    "raster copies out"
                );
            }
            p += 28 + src + alt + 8 + 1;
        }
        assert!(saw_raster, "the inline svg entry carries a decoded raster");

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ffi_media_open_play_round_trip() {
        let _store = crate::qtsq_test_lock();
        crate::clear_qtsq_rasters();
        crate::clear_qtsq_media();
        let dir = std::env::temp_dir().join(format!("kitae_j4ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::copy(
            format!("{FIXTURE_DIR}/fix_audio.qtsq"),
            dir.join("tone.qtsq"),
        )
        .expect("copy fixture");
        let page = std::ffi::CString::new(format!("file://{}/tone.qtsq", dir.display())).unwrap();

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        // Gate on the URL: under load the mailbox can still hold the boot
        // page when the first poll drains it, and that frame has no media
        // section to assert on (see `wait_for_frame_url`).
        let frame = wait_for_frame_url(unsafe { &*engine }, "tone.qtsq");

        // Walk the v5 tail to the media section: after version/flags/scroll/
        // content_h/bg, url, title, counts, entries, focused, selection —
        // then the u8 media-present flag. Easier: decode from the end. The
        // media section is the last thing written, and for audio it ends
        // with two strings (url, caf_path). Re-walk properly from the front
        // with a small cursor helper mirroring the wire doc.
        let mut p = 20usize; // version, flags, scroll, content_h, bg
        let url_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + url_len;
        let title_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + title_len;
        let (rects, texts, links, images) = (
            u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize,
            u32::from_le_bytes(frame[p + 4..p + 8].try_into().unwrap()) as usize,
            u32::from_le_bytes(frame[p + 8..p + 12].try_into().unwrap()) as usize,
            u32::from_le_bytes(frame[p + 12..p + 16].try_into().unwrap()) as usize,
        );
        p += 16;
        for _ in 0..rects {
            p = skip_v7_rect(&frame, p);
        }
        for _ in 0..texts {
            p += 8 * 4; // fixed fields through letter_spacing
            let family_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + family_len; // v14 family string
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        for _ in 0..links {
            let len = u32::from_le_bytes(frame[p + 16..p + 20].try_into().unwrap()) as usize;
            p += 16 + 4 + len;
        }
        for _ in 0..images {
            let src = u32::from_le_bytes(frame[p + 20..p + 24].try_into().unwrap()) as usize;
            let alt =
                u32::from_le_bytes(frame[p + 24 + src..p + 28 + src].try_into().unwrap()) as usize;
            p += 24 + src + alt + 8;
        }
        // focused + selection strings.
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        // Media section.
        assert_eq!(frame[p], 1, "audio page flags media present");
        p += 1;
        let kind = frame[p];
        p += 1;
        assert_eq!(kind, 0, "kind audio");
        let duration = f32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
        p += 4;
        let rate = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
        p += 4;
        let channels = frame[p];
        p += 1;
        let playing = frame[p];
        let playing_at = p;
        p += 1;
        let position = f32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
        p += 4;
        assert!(
            (duration - 1.0).abs() < 0.01,
            "fixture duration, got {duration}"
        );
        assert_eq!(rate, 22050, "fixture rate on the wire");
        assert_eq!(channels, 1, "mono on the wire");
        assert_eq!(playing, 0, "starts paused");
        assert_eq!(position, 0.0);
        let media_url_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        let media_url = String::from_utf8(frame[p..p + media_url_len].to_vec()).unwrap();
        assert!(media_url.ends_with("tone.qtsq"), "media url: {media_url}");
        p += media_url_len;
        let caf_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        let caf = String::from_utf8(frame[p..p + caf_len].to_vec()).unwrap();
        assert!(caf.ends_with(".caf"), "CAF path on the wire: {caf}");
        assert!(std::fs::metadata(&caf).is_ok(), "CAF file exists");
        // v6: the media-items count follows the media section (0 for an
        // audio page — the audio viewer has no `<video>` element); v7
        // appends the shadow count (0 here); v8 appends the find section
        // (no query → 0 matches, inactive) and the AX tree, which ends the
        // frame.
        p += caf_len;
        let items = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
        assert_eq!(items, 0, "audio page carries no media items");
        p += 4;
        let shadows = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
        assert_eq!(shadows, 0, "audio page carries no shadows");
        p += 4;
        assert_eq!(
            &frame[p..p + 4],
            &[0, 0, 0, 0],
            "no find matches without a query"
        );
        p += 4;
        assert_eq!(&frame[p..p + 4], &[0xFF, 0xFF, 0xFF, 0xFF], "find inactive");
        p += 4;
        p = skip_ax_section(&frame, p);
        // v15 sticky tail (empty here) follows the AX + transform sections.
        p += 4; // sticky count = 0
                // v16 filter tail (empty here).
        p = skip_filter_section(&frame, p);
        // v17: scroll-container tail follows the filter section.
        let scroll_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(scroll_count, 0, "no scroll containers on this page");
        p += 4 + scroll_count * 24;
        p = skip_clip_section(&frame, p);
        // v19: per-text blur tail — none on this page.
        let blurs = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(blurs, 0, "no blurred runs on this page");
        p += 4 + blurs * 8;
        p = skip_ax_semantics_tail(&frame, p);
        assert_eq!(frame.len(), p, "blur section ends the frame");

        // MEDIA_PLAY(1, 0) → next frame mirrors playing=true. The layout

        // before the media section is byte-identical, so the playing flag
        // sits at the same offset as in the first frame (a backwards walk
        // would have to skip the variable-length v8 find + AX tail).
        kitae_engine_command(engine, KITAE_CMD_MEDIA_PLAY, 1.0, 0.0, std::ptr::null());
        let frame = wait_for_frame(unsafe { &*engine });
        assert_eq!(
            frame[playing_at], 1,
            "playing flag mirrored after MEDIA_PLAY"
        );

        // MEDIA_STATE replies with a fresh frame (query path).
        kitae_engine_command(engine, KITAE_CMD_MEDIA_STATE, 0.0, 0.0, std::ptr::null());
        wait_for_frame(unsafe { &*engine });

        kitae_engine_destroy(engine);
        crate::clear_qtsq_rasters();
        crate::clear_qtsq_media();
        crate::clear_qtsq_videos();
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&caf).ok();
    }

    #[test]
    fn ffi_video_round_trip_carries_media_items() {
        let _store = crate::qtsq_test_lock();
        crate::clear_qtsq_rasters();
        crate::clear_qtsq_media();
        crate::clear_qtsq_videos();
        let dir = std::env::temp_dir().join(format!("kitae_j5ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::copy(
            format!("{FIXTURE_DIR}/fix_video.qtsq"),
            dir.join("clip.qtsq"),
        )
        .expect("copy fixture");
        let page = std::ffi::CString::new(format!("file://{}/clip.qtsq", dir.display())).unwrap();

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "clip.qtsq");
        assert_eq!(
            u32::from_le_bytes(frame[0..4].try_into().unwrap()),
            20,
            "frame format version 20"
        );

        // Front walk to the media items (mirror of the wire doc; cursor `p`).
        let mut p = 20usize;
        let url_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + url_len;
        let title_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + title_len;
        let counts: Vec<usize> = (0..4)
            .map(|i| {
                u32::from_le_bytes(frame[p + i * 4..p + i * 4 + 4].try_into().unwrap()) as usize
            })
            .collect();
        let (rects, texts, links, images) = (counts[0], counts[1], counts[2], counts[3]);
        p += 16;
        for _ in 0..rects {
            p = skip_v7_rect(&frame, p);
        }
        for _ in 0..texts {
            p += 8 * 4; // fixed fields through letter_spacing
            let family_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + family_len; // v14 family string
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        for _ in 0..links {
            let len = u32::from_le_bytes(frame[p + 16..p + 20].try_into().unwrap()) as usize;
            p += 16 + 4 + len;
        }
        for _ in 0..images {
            let src = u32::from_le_bytes(frame[p + 20..p + 24].try_into().unwrap()) as usize;
            let alt =
                u32::from_le_bytes(frame[p + 24 + src..p + 28 + src].try_into().unwrap()) as usize;
            p += 20 + 4 + src + 4 + alt + 8 + 1; // +1: v13 animated byte
        }
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        // Media section: video → kind 1, empty caf, then items count 1.
        assert_eq!(frame[p], 1, "video page flags media present");
        p += 1;
        assert_eq!(frame[p], 1, "kind video");
        p += 1 + 4 + 4 + 1 + 1 + 4; // duration, rate, channels, playing, position
        let murl_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + murl_len;
        let caf_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + caf_len;
        assert_eq!(caf_len, 0, "video has no CAF handoff");
        let item_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(item_count, 1, "one <video> media item");
        p += 4;

        // Item: id u64, x/y/w/h f32, src str, kind u8, playing u8, pos f32.
        let id = u64::from_le_bytes(frame[p..p + 8].try_into().unwrap());
        p += 8;
        let _x = f32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
        let y = f32::from_le_bytes(frame[p + 4..p + 8].try_into().unwrap());
        let w = f32::from_le_bytes(frame[p + 8..p + 12].try_into().unwrap());
        let h = f32::from_le_bytes(frame[p + 12..p + 16].try_into().unwrap());
        p += 16;
        let src_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        let src = String::from_utf8(frame[p..p + src_len].to_vec()).unwrap();
        assert_eq!(
            src,
            format!("file://{}/clip.qtsq", dir.display()),
            "item src is absolute"
        );
        p += src_len;
        assert_eq!(frame[p], 1, "item kind video");
        assert_eq!(frame[p + 1], 0, "item starts paused");
        let pos = f32::from_le_bytes(frame[p + 2..p + 6].try_into().unwrap());
        assert_eq!(pos, 0.0);
        p += 6;
        // v20: per-item controls flag byte. The QTSQ viewer's <video> has no
        // controls attribute — its transport lives in the host bar (v5 media
        // section) — so the flag is clear here. See
        // ffi_web_video_controls_flag for the web-video case.
        assert_eq!(frame[p], 0, "QTSQ viewer video carries no controls flag");
        p += 1;
        let shadow_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(shadow_count, 0, "no shadows on this page");
        p += 4;
        p = skip_v8_tail(&frame, p);
        // v15 sticky tail (empty here) + v16 filter tail.
        p += 4; // sticky count = 0
        p = skip_filter_section(&frame, p);
        // v17: scroll-container tail follows the filter section.
        let scroll_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(scroll_count, 0, "no scroll containers on this page");
        p += 4 + scroll_count * 24;
        p = skip_clip_section(&frame, p);
        // v19: per-text blur tail — none on this page.
        let blurs = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(blurs, 0, "no blurred runs on this page");
        p += 4 + blurs * 8;
        p = skip_ax_semantics_tail(&frame, p);
        assert_eq!(frame.len(), p, "blur section ends the frame");
        assert!(w > 0.0 && h > 0.0, "video has layout size");

        let _ = (y, id);

        // MEDIA_PLAY seek to the end → frame image payload steps to frame 5
        // (red-dominant in the lossy fixture), pixel-swapped under the same id.
        kitae_engine_command(engine, KITAE_CMD_MEDIA_PLAY, 0.0, 0.5, std::ptr::null());
        let _ = wait_for_frame(unsafe { &*engine });

        kitae_engine_destroy(engine);
        crate::clear_qtsq_rasters();
        crate::clear_qtsq_media();
        crate::clear_qtsq_videos();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// F2/F4/F6 wire: a web page's `<video controls>` demuxes through the
    /// global web-video store and surfaces via the FFI accessors (codec /
    /// dims / fps / extradata / samples), the media item carries the v20
    /// controls flag, and a frame fed back through `kitae_web_video_feed`
    /// rides the wire as the item's sibling image payload (registered
    /// raster under the media URL).
    #[test]
    fn ffi_web_video_controls_and_feed_round_trip() {
        let _store = crate::qtsq_test_lock();
        crate::clear_qtsq_rasters();
        crate::clear_qtsq_media();
        crate::clear_qtsq_videos();
        crate::clear_web_media();
        let dir = std::env::temp_dir().join(format!("kitae_f2ffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let media_path = dir.join("clip.mp4");
        std::fs::copy(
            format!(
                "{}/../kitae_media/tests/fixtures/clip.mp4",
                env!("CARGO_MANIFEST_DIR")
            ),
            &media_path,
        )
        .expect("copy fixture");
        let media_url = format!("file://{}", media_path.display());
        std::fs::write(
            dir.join("page.html"),
            "<html><body><video src=\"clip.mp4\" controls width=\"160\" height=\"90\">\
             <track kind=\"captions\" src=\"cap.vtt\" srclang=\"en\"></video></body></html>",
        )
        .expect("write page");
        std::fs::write(
            dir.join("cap.vtt"),
            "WEBVTT\n\n00:00:00.000 --> 00:00:30.000\nhello captions\n",
        )
        .expect("write captions");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();
        let media = std::ffi::CString::new(media_url.clone()).unwrap();

        // Feed a placeholder decoded frame before load so the first
        // relayout's stamping pass attaches it as the video's image payload.
        let px: Vec<u8> = (0..16u32).flat_map(|i| [i as u8, 0, 0, 255]).collect();
        assert_eq!(
            kitae_web_video_feed(media.as_ptr(), 4, 4, px.as_ptr(), px.len()),
            1,
            "feed accepts a full frame"
        );
        assert_eq!(
            kitae_web_video_feed(media.as_ptr(), 4, 4, px.as_ptr(), px.len() - 1),
            0,
            "short buffer rejected"
        );

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "page.html");

        // C-API decode metadata after the lazy ensure pass.
        assert_eq!(
            kitae_web_video_codec(media.as_ptr()),
            1,
            "MP4 fixture is H264"
        );
        let (mut vw, mut vh) = (0u32, 0u32);
        assert_eq!(
            kitae_web_video_dims(media.as_ptr(), &mut vw, &mut vh),
            1,
            "dims"
        );
        assert!(vw > 0 && vh > 0, "video has track dimensions");
        assert!(kitae_web_video_fps(media.as_ptr()) > 0.0, "fps known");
        let ex_len = kitae_web_video_extradata(media.as_ptr(), std::ptr::null_mut(), 0);
        assert!(ex_len > 0, "avcC extradata present");
        let mut ex = vec![0u8; ex_len];
        assert_eq!(
            kitae_web_video_extradata(media.as_ptr(), ex.as_mut_ptr(), ex.len()),
            ex_len,
            "extradata fully copied"
        );
        let n = kitae_web_video_sample_count(media.as_ptr());
        assert!(n > 0, "samples demuxed");
        let s0_len = kitae_web_video_sample(media.as_ptr(), 0, std::ptr::null_mut(), 0);
        assert!(s0_len > 0, "first sample sized");
        let mut s0 = vec![0u8; s0_len];
        assert_eq!(
            kitae_web_video_sample(media.as_ptr(), 0, s0.as_mut_ptr(), s0.len()),
            s0_len,
            "first sample fully copied"
        );
        assert_eq!(
            kitae_web_video_sample_keyframe(media.as_ptr(), 0),
            1,
            "first sample is a sync frame"
        );
        assert_eq!(
            kitae_web_video_sample(media.as_ptr(), n, std::ptr::null_mut(), 0),
            0,
            "index past the end misses"
        );
        assert_eq!(
            kitae_web_video_codec(b"file:///nope.mp4\0".as_ptr() as *const c_char),
            0
        );

        // Front walk to the media items (mirror of the wire doc; cursor p).
        let mut p = 20usize;
        let url_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + url_len;
        let title_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + title_len;
        let counts: Vec<usize> = (0..4)
            .map(|i| {
                u32::from_le_bytes(frame[p + i * 4..p + i * 4 + 4].try_into().unwrap()) as usize
            })
            .collect();
        let (rects, texts, links, images) = (counts[0], counts[1], counts[2], counts[3]);
        p += 16;
        for _ in 0..rects {
            p = skip_v7_rect(&frame, p);
        }
        for _ in 0..texts {
            p += 8 * 4;
            let family_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + family_len;
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        for _ in 0..links {
            let len = u32::from_le_bytes(frame[p + 16..p + 20].try_into().unwrap()) as usize;
            p += 16 + 4 + len;
        }
        // Image entries: collect payload ids while skipping (payload is a
        // u64 side-table id; the fed frame's raster carries our 4×4 pixels).
        let mut payloads = Vec::new();
        for _ in 0..images {
            let src_len = u32::from_le_bytes(frame[p + 20..p + 24].try_into().unwrap()) as usize;
            let alt_off = p + 24 + src_len;
            let alt_len =
                u32::from_le_bytes(frame[alt_off..alt_off + 4].try_into().unwrap()) as usize;
            let pay_off = alt_off + 4 + alt_len;
            payloads.push(u64::from_le_bytes(
                frame[pay_off..pay_off + 8].try_into().unwrap(),
            ));
            p += 20 + 4 + src_len + 4 + alt_len + 8 + 1;
        }
        // focused/selection strings.
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        // Media section: video → kind 1, empty caf, then items count 1.
        assert_eq!(frame[p], 1, "video page flags media present");
        p += 1 + 1 + 4 + 4 + 1 + 1 + 4; // present, kind, duration, rate, channels, playing, position
        let murl_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        let murl = String::from_utf8(frame[p + 4..p + 4 + murl_len].to_vec()).unwrap();
        assert!(!murl.is_empty(), "media section carries the page media url");
        p += 4 + murl_len;
        let caf_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + caf_len;
        assert_eq!(caf_len, 0, "video has no CAF handoff");
        let item_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(item_count, 1, "one <video> media item");
        p += 4;
        // Item: id u64, x/y/w/h f32, src str, kind u8, playing u8, pos f32.
        p += 8 + 16;
        let src_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        let src = String::from_utf8(frame[p..p + src_len].to_vec()).unwrap();
        p += src_len;
        assert_eq!(src, media_url, "item src is absolute");
        assert_eq!(frame[p], 1, "item kind video");
        assert_eq!(frame[p + 1], 0, "item starts paused");
        p += 2 + 4; // playing, position
        assert_eq!(frame[p], 1, "v20 controls flag set for <video controls>");
        p += 1;

        // The fed frame rides as the sibling image's payload and resolves
        // through the raster side-table to a 4×4 frame.
        let fed = payloads
            .iter()
            .copied()
            .find(|id| kitae_qtsq_raster_size(*id) == 8 + 4 * 4 * 4);
        assert!(fed.is_some(), "fed frame raster on the wire: {payloads:?}");

        kitae_engine_destroy(engine);
        crate::clear_qtsq_rasters();
        crate::clear_qtsq_media();
        crate::clear_qtsq_videos();
        crate::clear_web_media();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// v10: each text run ends with an f32 letter_spacing before its length.
    #[test]
    fn frame_v10_carries_letter_spacing() {
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        assert!(!engine.is_null());
        let dir = std::env::temp_dir().join(format!("kitae_q3wire_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("spaced.html"),
            "<html><body><p style=\"letter-spacing:2px\">AB CD</p></body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/spaced.html", dir.display())).unwrap();
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let frame = wait_for_frame_url(unsafe { &*engine }, "spaced.html");
        assert_eq!(
            u32::from_le_bytes(frame[0..4].try_into().unwrap()),
            20,
            "frame format version 20"
        );

        // Walk to the single text run.
        let mut p = 20usize;
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        let counts: Vec<usize> = (0..4)
            .map(|i| {
                u32::from_le_bytes(frame[p + i * 4..p + i * 4 + 4].try_into().unwrap()) as usize
            })
            .collect();
        p += 16;
        for _ in 0..counts[0] {
            p = skip_v7_rect(&frame, p);
        }
        // Text entry: 8 f32/u32 fields, then the v14 family string, then len,
        // then bytes. Field 8 is letter_spacing.
        let spacing = f32::from_le_bytes(frame[p + 7 * 4..p + 8 * 4].try_into().unwrap());
        let family_len =
            u32::from_le_bytes(frame[p + 8 * 4..p + 9 * 4].try_into().unwrap()) as usize;
        p += 9 * 4 + family_len;
        let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        let text = String::from_utf8(frame[p + 4..p + 4 + len].to_vec()).unwrap();
        assert_eq!(text, "AB", "first word of the letter-spaced run");
        assert_eq!(spacing, 2.0, "letter-spacing reaches the wire");
        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Front-walk a frame to the v8 find section: counts, rects (v7-aware),
    /// texts, links, images, focused/selection strings, media flag, media
    /// items, shadows. Returns the offset of the find match count.
    fn walk_to_find(frame: &[u8]) -> usize {
        let mut p = 20usize; // version, flags, scroll, content_h, bg
        let url_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + url_len;
        let title_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4 + title_len;
        let counts: Vec<usize> = (0..4)
            .map(|i| {
                u32::from_le_bytes(frame[p + i * 4..p + i * 4 + 4].try_into().unwrap()) as usize
            })
            .collect();
        p += 16;
        assert_eq!(counts[2], 0, "no anchor on this page");
        for _ in 0..counts[0] {
            p = skip_v7_rect(&frame, p);
        }
        for _ in 0..counts[1] {
            p += 8 * 4; // fixed fields through letter_spacing
            let family_len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + family_len; // v14 family string
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        for _ in 0..counts[2] {
            let len = u32::from_le_bytes(frame[p + 16..p + 20].try_into().unwrap()) as usize;
            p += 16 + 4 + len;
        }
        for _ in 0..counts[3] {
            let src = u32::from_le_bytes(frame[p + 20..p + 24].try_into().unwrap()) as usize;
            let alt =
                u32::from_le_bytes(frame[p + 24 + src..p + 28 + src].try_into().unwrap()) as usize;
            p += 20 + 4 + src + 4 + alt + 8;
        }
        for _ in 0..2 {
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len;
        }
        // Media section: absent flag, or the full present payload.
        if frame[p] == 1 {
            p += 1 + 1 + 4 + 4 + 1 + 1 + 4;
            for _ in 0..2 {
                let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
                p += 4 + len;
            }
        } else {
            p += 1;
        }
        let items = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        for _ in 0..items {
            p += 8 + 16;
            let len = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + len + 1 + 1 + 4;
        }
        let shadows = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        for _ in 0..shadows {
            p += 16 + 16 + 4 + 16 + 1;
        }
        p
    }

    #[test]
    fn ffi_find_query_next_and_clear_round_trip() {
        let dir = std::env::temp_dir().join(format!("kitae_findffi_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("page.html"),
            "<html><head><title>find me</title></head>\
             <body><p>alpha beta alpha gamma alpha</p></body></html>",
        )
        .expect("write page");
        let page = std::ffi::CString::new(format!("file://{}/page.html", dir.display())).unwrap();

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let _ = wait_for_frame_url(unsafe { &*engine }, "page.html");

        let query = std::ffi::CString::new("alpha").unwrap();
        kitae_engine_command(engine, KITAE_CMD_FIND, 0.0, 0.0, query.as_ptr());
        let frame = wait_for_frame(unsafe { &*engine });
        let mut p = walk_to_find(&frame);
        let count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
        assert_eq!(count, 3, "three occurrences of alpha");
        p += 4;
        let active = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
        assert_eq!(active, 0, "first match active after FIND");
        p += 4;
        // Each occurrence is one word-run → one rect.
        for _ in 0..3 {
            let n = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap());
            assert_eq!(n, 1, "single-run match carries one rect");
            let w = f32::from_le_bytes(frame[p + 4 + 8..p + 4 + 12].try_into().unwrap());
            let h = f32::from_le_bytes(frame[p + 4 + 12..p + 4 + 16].try_into().unwrap());
            assert!(w > 0.0 && h > 0.0, "non-degenerate highlight rect");
            p += 4 + 16;
        }
        p = skip_ax_section(&frame, p);
        // v15 sticky tail (empty here) follows the AX + transform sections.
        p += 4; // sticky count = 0
                // v16 filter tail (empty here).
        p = skip_filter_section(&frame, p);
        // v17: scroll-container tail follows the filter section.
        let scroll_count = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(scroll_count, 0, "no scroll containers on this page");
        p += 4 + scroll_count * 24;
        p = skip_clip_section(&frame, p);
        // v19: per-text blur tail — none on this page.
        let blurs = u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(blurs, 0, "no blurred runs on this page");
        p += 4 + blurs * 8;
        assert_eq!(frame.len(), p, "blur section ends the frame");

        // FIND_NEXT steps the active index (wrapping); FIND_CLEAR empties.
        kitae_engine_command(engine, KITAE_CMD_FIND_NEXT, 0.0, 0.0, std::ptr::null());
        let frame = wait_for_frame(unsafe { &*engine });
        let p = walk_to_find(&frame) + 4;
        assert_eq!(u32::from_le_bytes(frame[p..p + 4].try_into().unwrap()), 1);

        let clear = std::ffi::CString::new("").unwrap();
        kitae_engine_command(engine, KITAE_CMD_FIND, 0.0, 0.0, clear.as_ptr());
        let frame = wait_for_frame(unsafe { &*engine });
        let p = walk_to_find(&frame);
        assert_eq!(
            &frame[p..p + 4],
            &[0, 0, 0, 0],
            "empty query clears matches"
        );
        assert_eq!(
            &frame[p + 4..p + 8],
            &[0xFF, 0xFF, 0xFF, 0xFF],
            "cleared query deactivates"
        );

        kitae_engine_destroy(engine);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Slice ② over the raw C ABI: command 26 saves the page, the outcome
    /// lands on the notification queue, and take_notification hands back
    /// "1\t{path}" / "0\t{message}". Key dir redirected (lock-guarded) so no
    /// real device keys are generated.
    #[test]
    fn ffi_save_page_command_reports_through_notifications() {
        let _lock = crate::qtsq_test_lock();
        let keys = std::env::temp_dir().join(format!("kitae_ffisave_keys_{}", std::process::id()));
        std::fs::create_dir_all(&keys).expect("key dir");
        let saved_key_dir = std::env::var("KITAE_QTSQ_KEY_DIR").ok();
        std::env::set_var("KITAE_QTSQ_KEY_DIR", &keys);
        let out_path =
            std::env::temp_dir().join(format!("kitae_ffisave_{}.qtsq", std::process::id()));

        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };

        let engine = kitae_engine_new(400, 600, &vt);
        let eng = unsafe { &*engine };
        wait_for_frame(eng); // boot page frame

        // Load a real page (the raw ABI has no load_html: write a file),
        // then command 26 with the destination path.
        let page_path =
            std::env::temp_dir().join(format!("kitae_ffisave_page_{}.html", std::process::id()));
        std::fs::write(
            &page_path,
            "<html><head><title>ffi save</title></head><body><p>ffi body</p></body></html>",
        )
        .expect("write page");
        let page = CString::new(format!("file://{}", page_path.display())).unwrap();
        kitae_engine_command(engine, KITAE_CMD_NAVIGATE, 0.0, 0.0, page.as_ptr());
        let _ = wait_for_frame_url(
            eng,
            &format!("kitae_ffisave_page_{}.html", std::process::id()),
        );
        let dest = CString::new(out_path.to_str().unwrap()).unwrap();
        kitae_engine_command(engine, KITAE_CMD_SAVE_PAGE, 0.0, 0.0, dest.as_ptr());

        // Poll the notification queue (drain any empty reads first).
        let mut buf = vec![0 as c_char; 4096];
        let msg = loop {
            let got = kitae_engine_take_notification(engine, buf.as_mut_ptr(), buf.len());
            if got == 1 {
                break unsafe { CStr::from_ptr(buf.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        assert_eq!(
            msg,
            format!("1\t{}", out_path.display()),
            "success notification carries the path"
        );
        let bytes = std::fs::read(&out_path).expect("archive written");
        let info = kitae_qtsq::probe(&bytes).expect("probe");
        assert!(info.encrypted && info.signed, "encrypted + signed archive");

        // Failure: unwritable destination → "0\t{message}".
        let bad = CString::new("/nonexistent_dir_for_kitae_save_test/archive.qtsq").unwrap();
        kitae_engine_command(engine, KITAE_CMD_SAVE_PAGE, 0.0, 0.0, bad.as_ptr());
        let msg = loop {
            let got = kitae_engine_take_notification(engine, buf.as_mut_ptr(), buf.len());
            if got == 1 {
                break unsafe { CStr::from_ptr(buf.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        assert!(
            msg.starts_with("0\twrite: "),
            "failure notification: {msg:?}"
        );

        match saved_key_dir {
            Some(v) => std::env::set_var("KITAE_QTSQ_KEY_DIR", v),
            None => std::env::remove_var("KITAE_QTSQ_KEY_DIR"),
        }
        std::fs::remove_file(&out_path).ok();
        std::fs::remove_file(&page_path).ok();
        std::fs::remove_dir_all(&keys).ok();
        kitae_engine_destroy(engine);
    }

    /// Phase 2 (`live`): the raw ABI answers LIVE_STATE through the
    /// notifications queue; LIVE_POLL is a safe no-op off a Live page.
    /// (Feature-gated: without `live` the state reply is compiled out.)
    #[cfg(feature = "live")]
    #[test]
    fn ffi_live_commands_report_state() {
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        let engine = kitae_engine_new(400, 600, &vt);
        assert!(!engine.is_null());
        let eng = unsafe { &*engine };
        wait_for_frame(eng); // boot page frame

        kitae_engine_command(engine, KITAE_CMD_LIVE_POLL, 0.0, 0.0, std::ptr::null());
        kitae_engine_command(engine, KITAE_CMD_LIVE_STATE, 0.0, 0.0, std::ptr::null());
        let mut buf = vec![0 as c_char; 4096];
        let msg = loop {
            let got = kitae_engine_take_notification(engine, buf.as_mut_ptr(), buf.len());
            if got == 1 {
                break unsafe { CStr::from_ptr(buf.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        assert_eq!(msg, "live\t0", "about:start is not a live session");

        kitae_engine_destroy(engine);
    }

    // ── H8: poison recovery ───────────────────────────────────────────────

    #[test]
    fn poisoned_mailbox_lock_still_yields_its_data() {
        let m = Arc::new(Mutex::new(7u32));
        let m2 = m.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = m2.lock().unwrap();
            panic!("poison the lock");
        }));
        assert_eq!(
            *lock_poison_ok(&m),
            7,
            "a panicking holder must not take the mailbox down with it"
        );
    }

    // ── B4: the agent plane on the app's own engine ───────────────────────

    /// Pop queued notifications until one of `prefix` arrives (dropping the
    /// rest, exactly as a host that only handles the kinds it knows does).
    fn wait_for_notification(engine: &Engine, prefix: &str) -> Option<String> {
        wait_for_notification_where(engine, prefix, "")
    }

    /// The same, but only a message that also contains `needle` — several
    /// policy snapshots can be queued, and a test that stops at the first one
    /// reads a stale answer.
    fn wait_for_notification_where(engine: &Engine, prefix: &str, needle: &str) -> Option<String> {
        for _ in 0..600 {
            let mut len = 0usize;
            let queued = kitae_engine_notification_available(
                engine as *const Engine as *mut Engine,
                &mut len,
            );
            if queued == 1 {
                // Size from the peek: `take_notification` drops an oversized
                // message rather than putting it back.
                let mut buf = vec![0 as c_char; len + 1];
                let got = kitae_engine_take_notification(
                    engine as *const Engine as *mut Engine,
                    buf.as_mut_ptr(),
                    buf.len(),
                );
                if got == 1 {
                    let message = unsafe { CStr::from_ptr(buf.as_ptr()) }
                        .to_string_lossy()
                        .into_owned();
                    if let Some(rest) = message.strip_prefix(prefix) {
                        if rest.contains(needle) {
                            return Some(rest.to_string());
                        }
                    }
                }
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        None
    }

    fn test_engine(width: c_int, height: c_int) -> *mut Engine {
        let vt = KitaeMeasurerVtable {
            ctx: std::ptr::null_mut(),
            measure: test_measure,
            metrics: test_metrics,
            x_height: None,
        };
        kitae_engine_new(width, height, &vt)
    }

    fn send_agent_act(engine: *mut Engine, request: &str) {
        let json = CString::new(request).unwrap();
        kitae_engine_command(engine, KITAE_CMD_AGENT_ACT, 0.0, 0.0, json.as_ptr());
    }

    /// The two-phase permission flow the app's panel is built on: a verb
    /// outside the session's rule comes back `pending` (and journaled as a
    /// refusal), the host asks the user, grants, and the *same* request then
    /// runs.
    #[test]
    fn agent_acts_run_on_the_engines_own_browser_behind_a_permission_prompt() {
        let engine = test_engine(800, 600);
        assert!(!engine.is_null());
        // Let the startup navigation land, so `read` has a page to read.
        let _ = wait_for_frame(unsafe { &*engine });

        // 1. A read-only verb runs straight away — and the reply carries the
        //    journal rows this act added.
        send_agent_act(engine, r#"{"verb":"read"}"#);
        let reply = wait_for_notification(unsafe { &*engine }, "agent-result\t").expect("reply");
        assert!(reply.contains(r#""ok":true"#), "{reply}");
        assert!(reply.contains(r#""verb":"read""#), "{reply}");
        assert!(
            reply.contains(r#""journal""#),
            "the act's journal rows travel with it: {reply}"
        );

        // 2. `navigate` is not in the default (read-only) policy: the engine
        //    refuses, names the verb, and marks the reply pending — that is
        //    what tells a host to prompt instead of giving up.
        send_agent_act(engine, r#"{"verb":"navigate","url":"about:blank"}"#);
        let reply = wait_for_notification(unsafe { &*engine }, "agent-result\t").expect("reply");
        assert!(reply.contains(r#""ok":false"#), "{reply}");
        assert!(reply.contains(r#""kind":"permission""#), "{reply}");
        assert!(reply.contains(r#""pending":true"#), "{reply}");
        assert!(reply.contains(r#""verb":"navigate""#), "{reply}");

        // 3. The user says yes: the host records the grant…
        let verb = CString::new("navigate").unwrap();
        kitae_engine_command(engine, KITAE_CMD_AGENT_GRANT, 0.0, 0.0, verb.as_ptr());
        let policy = wait_for_notification_where(unsafe { &*engine }, "agent-policy\t", "navigate")
            .expect("policy");
        assert!(policy.contains(r#""navigate""#), "{policy}");

        // …and the very same request now runs.
        send_agent_act(engine, r#"{"verb":"navigate","url":"about:blank"}"#);
        let reply = wait_for_notification(unsafe { &*engine }, "agent-result\t").expect("reply");
        assert!(reply.contains(r#""ok":true"#), "{reply}");
        assert!(reply.contains("about:blank"), "{reply}");

        // 4. The journal is the audit the panel renders: the refusal is in it
        //    (an audit that only records successes is not an audit).
        kitae_engine_command(engine, KITAE_CMD_AGENT_JOURNAL, 0.0, 0.0, std::ptr::null());
        let journal =
            wait_for_notification(unsafe { &*engine }, "agent-journal\t").expect("journal");
        assert!(journal.contains(r#""denied""#), "{journal}");
        assert!(journal.contains(r#""verb":"navigate""#), "{journal}");
        assert!(journal.contains(r#""outcome""#), "{journal}");

        // 5. Revoking forgets the grant: the next attempt prompts again.
        kitae_engine_command(
            engine,
            KITAE_CMD_AGENT_REVOKE_GRANTS,
            0.0,
            0.0,
            std::ptr::null(),
        );
        let policy =
            wait_for_notification_where(unsafe { &*engine }, "agent-policy\t", r#""grants":[]"#)
                .expect("policy");
        assert!(policy.contains(r#""grants":[]"#), "{policy}");
        send_agent_act(engine, r#"{"verb":"navigate","url":"about:blank"}"#);
        let reply = wait_for_notification(unsafe { &*engine }, "agent-result\t").expect("reply");
        assert!(reply.contains(r#""pending":true"#), "{reply}");

        kitae_engine_destroy(engine);
    }

    // The host rasterizer the app installs, faked: it reads the browser it is
    // handed (proving the pointer is the live one) and answers with bytes a
    // test can recognise.
    static RASTER_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static RASTER_SAW: Mutex<Option<String>> = Mutex::new(None);

    unsafe extern "C" fn fake_raster_png(
        _ctx: *mut c_void,
        browser: *const Browser,
        buf: *mut u8,
        cap: usize,
    ) -> isize {
        RASTER_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let Some(browser) = browser.as_ref() else {
            return -1;
        };
        let url = browser.state().url;
        *lock_poison_ok(&RASTER_SAW) = Some(url.clone());
        let bytes = format!("PNGOF {url}").into_bytes();
        if !buf.is_null() && cap > 0 {
            let n = bytes.len().min(cap);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
        }
        bytes.len() as isize
    }

    /// `screenshot` on the app's engine: the host installs a rasterizer
    /// through the C ABI, and the verb reaches it with the engine's own
    /// browser — the whole point of the vtable.
    #[test]
    fn a_host_rasterizer_serves_screenshot_on_the_engines_browser() {
        let engine = test_engine(400, 300);
        assert!(!engine.is_null());
        let _ = wait_for_frame(unsafe { &*engine });

        kitae_engine_set_raster(
            engine,
            std::ptr::null_mut(),
            Some(fake_raster_png),
            None,
            None,
        );
        assert!(
            wait_for_notification(unsafe { &*engine }, "agent-raster\t").is_some(),
            "the engine confirms the rasterizer is installed"
        );

        send_agent_act(engine, r#"{"verb":"screenshot"}"#);
        let reply = wait_for_notification(unsafe { &*engine }, "agent-result\t").expect("reply");
        assert!(reply.contains(r#""ok":true"#), "{reply}");
        assert!(reply.contains(r#""mime":"image/png""#), "{reply}");
        // The bytes are the fake's, carried as base64 of "PNGOF <url>" — so
        // the verb, the reply and the browser pointer all line up.
        let seen = lock_poison_ok(&RASTER_SAW).clone().unwrap_or_default();
        assert!(
            !seen.is_empty(),
            "the rasterizer saw the engine's live browser (url {seen:?})"
        );
        let expected = crate::b64_encode(format!("PNGOF {seen}").as_bytes());
        assert!(
            reply.contains(&expected),
            "the reply carries the rasterizer's bytes for {seen:?}"
        );
        assert!(
            RASTER_CALLS.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "two-pass sizing"
        );

        // `pdf` had no function pointer installed, and says so by name.
        send_agent_act(engine, r#"{"verb":"pdf"}"#);
        let reply = wait_for_notification(unsafe { &*engine }, "agent-result\t").expect("reply");
        assert!(reply.contains(r#""ok":false"#), "{reply}");
        assert!(reply.contains("no rasterizer for `pdf`"), "{reply}");

        kitae_engine_destroy(engine);
    }
}
