//! The public embedder API: `Engine::builder()…build()`, frames, events.
//!
//! This is the facade an *outside* program embeds — the same shape the Swift
//! app consumes through the C ABI, expressed natively in Rust. One
//! [`Engine`] owns one engine thread (fetch/parse/style/layout never block
//! the caller); the embedder drives it with navigation methods and reads
//! back [`RenderedFrame`]s and [`EngineEvent`]s. Nothing in here executes
//! scripts — the fetch/parse/style/layout pipeline is the whole engine.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kitae_layout::DisplayList;
use kitae_net::FetchError;

use crate::{Browser, Host, PublicState, START_URL};

/// How long [`Engine::wait_for_frame`] callers should budget for a full
/// navigation (fetch + parse + layout) before giving up. The embedder cannot
/// currently change the underlying HTTP timeout (the client is a process-wide
/// singleton) — this constant is the documented contract.
pub const DEFAULT_LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// A finished frame plus everything the chrome needs around it.
///
/// Unlike the C-ABI frame (binary, versioned separately) this is the native
/// Rust surface: borrowed field access, no parsing.
#[derive(Debug, Clone)]
pub struct RenderedFrame {
    /// The paint plan (contract version [`DISPLAY_LIST_VERSION`]).
    pub display_list: DisplayList,
    /// Resolved URL after redirects.
    pub url: String,
    pub resource_base_url: String,
    /// Page title, whitespace-collapsed.
    pub title: String,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    /// Total document height (also on the display list; mirrored for
    /// convenience).
    pub content_height: f32,
    /// Focused element label (id or tag) and selection text (slice H).
    pub focused_id: Option<String>,
    pub selection: Option<String>,
    /// Media attached to the page (slice J4): `Some` on QTSQ audio/video
    /// viewer pages, carrying the player inputs (and the audio CAF path).
    pub media: Option<crate::MediaInfo>,
    /// Find-in-page snapshot (Phase P1): matches in page order + the active
    /// index (`None` when the query is empty or matched nothing).
    pub find_matches: Vec<crate::FindMatch>,
    pub find_active: Option<usize>,
    /// Accessibility tree (Phase P2): Document-rooted nodes with layout
    /// bounds, plus the focused node's AX id when it made the tree.
    pub ax_tree: Vec<crate::AxNode>,
    pub ax_focused: Option<String>,
    /// True when the page is a QTSQ that failed to decode because it is
    /// encrypted — hosts prompt for a password and call
    /// [`Engine::set_password`].
    pub qtsq_locked: bool,
    /// Signature verdict for the page's QTSQ payload (slice ④): hosts show
    /// the banner from this instead of scraping page text.
    pub qtsq_sig: crate::QtsqSigState,
    /// H3: the frame was laid out for print media (see
    /// [`Engine::set_media_type`]), so `@media print` applied and
    /// `@media screen` did not. Same bit the C-ABI frame carries.
    pub print_media: bool,
}

/// Something the embedder must react to outside the frame stream.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// A navigation started or ended. `true` = spinner on.
    Loading(bool),
    /// The page URL changed without the embedder navigating — redirect or
    /// base-URL change. Update the address bar.
    LocationChanged(String),
    /// Open this URL outside the engine (mailto:, kitae:open-external:).
    OpenExternal(String),
    /// The last load failed; the current frame shows the error page. The
    /// typed reason is also reachable via [`Engine::last_error`].
    LoadFailed(FetchError),
    /// A Save-Page-as-QTSQ request finished (see [`Engine::save_page_qtsq`]).
    /// `error` is `None` and `path` names the archive written, or
    /// `Some(message)` explains the failure.
    QtsqSaved { path: String, error: Option<String> },
}

/// What can go wrong while creating or driving an engine. The load path
/// itself never errors — failures become error pages + [`EngineEvent::LoadFailed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// `navigate("")` and friends: nothing to do, state unchanged.
    EmptyUrl,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::EmptyUrl => write!(f, "navigation target was empty"),
        }
    }
}

impl std::error::Error for EngineError {}

// ---------------------------------------------------------------------------
// Internal plumbing: the Host implementation that feeds the mailbox
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Shared {
    /// Draining mailbox of new frames.
    frame: Mutex<Option<RenderedFrame>>,
    /// Latest known chrome snapshot — survives `take_frame`, so the embedder
    /// can read url/title any time.
    latest: Mutex<Option<Chrome>>,
    loading: Mutex<bool>,
    events: Mutex<Vec<EngineEvent>>,
    last_error: Mutex<Option<FetchError>>,
}

/// The chrome-relevant summary of the newest frame.
#[derive(Debug, Clone)]
struct Chrome {
    url: String,
    title: String,
    can_go_back: bool,
    can_go_forward: bool,
    focused_id: Option<String>,
    selection: Option<String>,
}

impl Shared {
    fn push_event(&self, ev: EngineEvent) {
        self.events.lock().unwrap().push(ev);
    }
}

struct MailboxHost {
    shared: Arc<Shared>,
}

impl Host for MailboxHost {
    fn publish(&mut self, state: &PublicState, dl: &DisplayList) {
        let frame = RenderedFrame {
            display_list: dl.clone(),
            url: state.url.clone(),
            resource_base_url: state.resource_base_url.clone(),
            title: state.title.clone(),
            can_go_back: state.can_go_back,
            can_go_forward: state.can_go_forward,
            content_height: state.content_height,
            focused_id: state.focused_id.clone(),
            selection: state.selection.clone(),
            media: state.media.clone(),
            find_matches: state.find_matches.clone(),
            find_active: state.find_active,
            ax_tree: state.ax_tree.clone(),
            ax_focused: state.ax_focused.clone(),
            qtsq_locked: state.qtsq_locked,
            qtsq_sig: state.qtsq_sig,
            print_media: state.print_media,
        };
        *self.shared.frame.lock().unwrap() = Some(frame);
        *self.shared.latest.lock().unwrap() = Some(Chrome {
            url: state.url.clone(),
            title: state.title.clone(),
            can_go_back: state.can_go_back,
            can_go_forward: state.can_go_forward,
            focused_id: state.focused_id.clone(),
            selection: state.selection.clone(),
        });
    }

    fn scroll_updated(&mut self, _state: &PublicState) {
        // Embedded hosts scroll their view locally; scroll state is read
        // through `Engine::scroll_y` when needed.
    }

    fn set_loading(&mut self, loading: bool) {
        *self.shared.loading.lock().unwrap() = loading;
        self.shared.push_event(EngineEvent::Loading(loading));
    }

    fn open_external(&mut self, url: &str) {
        self.shared
            .push_event(EngineEvent::OpenExternal(url.to_string()));
    }

    fn location_changed(&mut self, url: &str) {
        self.shared
            .push_event(EngineEvent::LocationChanged(url.to_string()));
    }

    fn load_failed(&mut self, url: &str, error: &FetchError) {
        *self.shared.last_error.lock().unwrap() = Some(error.clone());
        self.shared
            .push_event(EngineEvent::LoadFailed(error.clone()));
        let _ = url;
    }

    fn save_finished(&mut self, path: &str, error: Option<&str>) {
        self.shared.push_event(EngineEvent::QtsqSaved {
            path: path.to_string(),
            error: error.map(|e| e.to_string()),
        });
    }
}

/// [`crate::ApproxMeasurer`] backs `EngineBuilder::build` — the documented
/// zero-dependency measurer; use `build_with_measurer` for real fonts.

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
    MediaOpen,
    MediaPlay(bool, f32),
    MediaState,
    LoadHtml(String, String),
    /// Store the session QTSQ password and reload the current page.
    SetPassword(String),
    /// Environment media state (Q6): what `@media (prefers-…)` sees.
    SetEnvironment(kitae_style::EnvironmentState),
    /// Only the `prefers-*` half of the environment (Q6 over the wire, where
    /// the command carries the scheme and the two feature bits and nothing
    /// else). The media type is [`Command::SetMediaType`]'s business, so an
    /// appearance change never knocks a print run back to screen.
    SetPreferences(kitae_style::EnvironmentState),
    /// H3: the media environment the page is laid out in (screen / print).
    SetMediaType(kitae_style::MediaType),
    /// Save the current page as a device-encrypted QTSQ archive at `path`.
    /// The outcome arrives later as [`EngineEvent::QtsqSaved`].
    SavePage(String),
    /// A6: Esc — close the topmost open modal dialog.
    Escape,
    /// A7: step the focused control's value by `dir` (±1) — range/number
    /// move one `step`, select moves the selection one option.
    StepFocus(i32),
    /// A11a: insert `text` at the focused editable control's caret.
    TypeText(String),
    /// A11a: delete the char before the caret.
    Backspace,
    /// A11a: delete the char after the caret.
    DeleteForward,
    /// A11a: move the caret by `delta` chars (arrows: ±1).
    MoveCaret(isize),
    /// A11a: place the caret at an absolute char offset (Home/End).
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
}

/// The embeddable engine. Cheap to clone (all handles are shared); every
/// clone drives the same underlying session.
pub struct Engine {
    tx: mpsc::Sender<Command>,
    shared: Arc<Shared>,
}

impl Clone for Engine {
    fn clone(&self) -> Self {
        Engine {
            tx: self.tx.clone(),
            shared: self.shared.clone(),
        }
    }
}

/// Builder for [`Engine`]: `Engine::builder().viewport(800., 600.).build()`.
pub struct EngineBuilder {
    viewport: (f32, f32),
    start_url: String,
    page_js: bool,
}

impl Engine {
    /// Begin configuring an engine.
    pub fn builder() -> EngineBuilder {
        EngineBuilder {
            viewport: (800.0, 600.0),
            start_url: START_URL.to_string(),
            page_js: false,
        }
    }

    // -- navigation ---------------------------------------------------------

    /// Address-bar navigation: normalize, push into history, load.
    /// Empty input is a no-op error (`EngineError::EmptyUrl`).
    pub fn navigate(&self, input: &str) -> Result<(), EngineError> {
        if input.trim().is_empty() {
            return Err(EngineError::EmptyUrl);
        }
        self.tx.send(Command::Navigate(input.to_string())).ok();
        Ok(())
    }

    /// Plain navigation (returns `()`); for `let _ = engine.navigate(url)?`
    /// ergonomics where an error is genuinely unexpected.
    pub fn navigate_lossy(&self, input: &str) {
        let _ = self.navigate(input);
    }

    pub fn go_back(&self) {
        self.tx.send(Command::Back).ok();
    }

    pub fn go_forward(&self) {
        self.tx.send(Command::Forward).ok();
    }

    pub fn reload(&self) {
        self.tx.send(Command::Reload).ok();
    }

    /// Scroll the page view by `dy` points (positive = down).
    pub fn scroll_by(&self, dy: f32) {
        self.tx.send(Command::Scroll(dy)).ok();
    }

    /// Resize the page viewport; triggers a relayout at the new width.
    pub fn resize(&self, width: f32, height: f32) {
        self.tx.send(Command::Resize(width, height)).ok();
    }

    /// Click at page-space coordinates (embedder adds its scroll offset).
    pub fn click(&self, x: f32, y: f32) {
        self.tx.send(Command::Click(x, y)).ok();
    }

    /// Focus the next / previous focusable (Tab / Shift+Tab order).
    pub fn focus_next(&self) {
        self.tx.send(Command::FocusNext).ok();
    }

    pub fn focus_prev(&self) {
        self.tx.send(Command::FocusPrev).ok();
    }

    /// Activate the focused element (follow link, toggle checkbox, submit).
    pub fn activate_focused(&self) {
        self.tx.send(Command::Activate).ok();
    }

    /// A6: Esc — close the topmost open modal dialog (no-op otherwise).
    pub fn escape(&self) {
        self.tx.send(Command::Escape).ok();
    }

    /// A7: arrow-key style value step for the focused control — range and
    /// number move one `step` (clamped), select moves the selection.
    pub fn step_focus(&self, dir: i32) {
        self.tx.send(Command::StepFocus(dir)).ok();
    }

    /// A11a: insert `text` at the focused editable control's caret.
    pub fn type_text(&self, text: &str) {
        self.tx.send(Command::TypeText(text.to_string())).ok();
    }

    /// A11a: delete the char before the caret.
    pub fn backspace(&self) {
        self.tx.send(Command::Backspace).ok();
    }

    /// A11a: delete the char after the caret.
    pub fn delete_forward(&self) {
        self.tx.send(Command::DeleteForward).ok();
    }

    /// A11a: move the caret by `delta` chars (left/right arrows: ∓1).
    pub fn move_caret(&self, delta: isize) {
        self.tx.send(Command::MoveCaret(delta)).ok();
    }

    /// A11a: place the caret at an absolute char offset (Home: 0).
    pub fn set_caret(&self, offset: usize) {
        self.tx.send(Command::SetCaret(offset)).ok();
    }

    pub fn extend_caret(&self, delta: isize) {
        self.tx.send(Command::ExtendCaret(delta)).ok();
    }

    pub fn begin_selection(&self, offset: usize) {
        self.tx.send(Command::SelectionBegin(offset)).ok();
    }

    pub fn extend_selection(&self, offset: usize) {
        self.tx.send(Command::SelectionExtend(offset)).ok();
    }

    pub fn begin_selection_at_point(&self, x: f32, y: f32) {
        self.tx.send(Command::SelectionBeginPoint(x, y)).ok();
    }

    pub fn extend_selection_at_point(&self, x: f32, y: f32) {
        self.tx.send(Command::SelectionExtendPoint(x, y)).ok();
    }

    pub fn end_selection(&self) {
        self.tx.send(Command::SelectionEnd).ok();
    }

    pub fn selection_begin(&self, offset: usize) {
        self.begin_selection(offset);
    }

    pub fn selection_extend(&self, offset: usize) {
        self.extend_selection(offset);
    }

    pub fn selection_end(&self) {
        self.end_selection();
    }

    pub fn set_marked_text(&self, text: &str) {
        self.tx
            .send(Command::CompositionUpdate(text.to_string()))
            .ok();
    }

    pub fn update_marked_text(&self, text: &str) {
        self.set_marked_text(text);
    }

    pub fn commit_composition(&self) {
        self.tx.send(Command::CompositionCommit(None)).ok();
    }

    pub fn commit_composition_text(&self, text: &str) {
        self.tx
            .send(Command::CompositionCommit(Some(text.to_string())))
            .ok();
    }

    pub fn cancel_composition(&self) {
        self.tx.send(Command::CompositionCancel).ok();
    }

    pub fn composition_update(&self, text: &str) {
        self.set_marked_text(text);
    }

    pub fn composition_commit(&self) {
        self.commit_composition();
    }

    pub fn composition_cancel(&self) {
        self.cancel_composition();
    }

    /// Set a form control's value by element id.
    pub fn set_input(&self, element_id: &str, value: &str) {
        self.tx
            .send(Command::SetInput(element_id.to_string(), value.to_string()))
            .ok();
    }

    /// Submit a form by element id (GET navigation).
    pub fn submit_form(&self, form_id: &str) {
        self.tx.send(Command::Submit(form_id.to_string())).ok();
    }

    pub fn select_all(&self) {
        self.tx.send(Command::SelectAll).ok();
    }

    pub fn clear_selection(&self) {
        self.tx.send(Command::ClearSelection).ok();
    }

    /// Hover node by element id for `:hover` restyle (`None`/empty clears).
    /// The embedder resolves mouse points to ids (accessibility tree,
    /// Phase P); unknown ids are ignored engine-side.
    pub fn set_hover(&self, element_id: Option<&str>) {
        self.tx
            .send(Command::SetHover(
                element_id.filter(|s| !s.is_empty()).map(str::to_string),
            ))
            .ok();
    }

    /// Active (pressed) node by element id (`None`/empty clears).
    pub fn set_active(&self, element_id: Option<&str>) {
        self.tx
            .send(Command::SetActive(
                element_id.filter(|s| !s.is_empty()).map(str::to_string),
            ))
            .ok();
    }

    /// The embedder's player opened the page's media. The reply is a fresh
    /// frame carrying the media state (mirrors [`crate::Browser::media_open`]).
    pub fn media_open(&self) {
        self.tx.send(Command::MediaOpen).ok();
    }

    /// Transport mirror update from the embedder's player: playing flag +
    /// position seconds. Republishes so the next frame carries the state.
    pub fn media_play(&self, playing: bool, position: f32) {
        self.tx.send(Command::MediaPlay(playing, position)).ok();
    }

    /// Force a fresh frame carrying the current media state (query path).
    pub fn media_state(&self) {
        self.tx.send(Command::MediaState).ok();
    }

    /// Load an HTML string directly (tests, custom protocols).
    pub fn load_html(&self, html: &str, base_url: &str) {
        self.tx
            .send(Command::LoadHtml(html.to_string(), base_url.to_string()))
            .ok();
    }

    /// Set the session QTSQ password. When a page is already open it is
    /// reloaded, so a locked viewer page unlocks with the next frame.
    pub fn set_password(&self, password: &str) {
        self.tx
            .send(Command::SetPassword(password.to_string()))
            .ok();
    }

    /// Update the environment for `prefers-*` media features (Q6). Pages
    /// styled with `@media (prefers-color-scheme: …)` re-render in place,
    /// no reload.
    pub fn set_environment(&self, env: kitae_style::EnvironmentState) {
        self.tx.send(Command::SetEnvironment(env)).ok();
    }

    /// H3: lay the page out for `media` (screen or print) instead. Print media
    /// applies `@media print` rules and drops `@media screen` ones, then
    /// publishes a frame carrying the print layout — which is what a host
    /// hands to `NSPrintOperation`. Call it back with
    /// [`kitae_style::MediaType::Screen`] when the paper run is done; the
    /// screen frame is republished. No reload either way.
    pub fn set_media_type(&self, media: kitae_style::MediaType) {
        self.tx.send(Command::SetMediaType(media)).ok();
    }

    /// Save the current page as a device-encrypted, device-signed QTSQ
    /// archive at `path` (Save-Page-as-QTSQ). Fire-and-forget: the outcome
    /// arrives as [`EngineEvent::QtsqSaved`] through [`Self::poll_events`].
    pub fn save_page_qtsq(&self, path: &str) {
        self.tx.send(Command::SavePage(path.to_string())).ok();
    }

    // -- reading state ------------------------------------------------------

    /// The newest published frame, if any has arrived since the last call.
    /// Draining: a frame returned here is not returned again.
    pub fn take_frame(&self) -> Option<RenderedFrame> {
        self.shared.frame.lock().unwrap().take()
    }

    /// Drain pending events (loading, location, external, load-failed).
    pub fn poll_events(&self) -> Vec<EngineEvent> {
        let mut evs = self.shared.events.lock().unwrap();
        std::mem::take(&mut *evs)
    }

    /// Typed failure behind the most recent [`EngineEvent::LoadFailed`],
    /// `None` while the current page is a successful load.
    pub fn last_error(&self) -> Option<FetchError> {
        self.shared.last_error.lock().unwrap().clone()
    }

    /// Snapshot of navigation state from the newest published frame (url,
    /// title, back/forward availability). `None` before the first frame;
    /// unlike [`Self::take_frame`] this survives being read.
    pub fn chrome_state(&self) -> Option<(String, String, bool, bool)> {
        self.shared.latest.lock().unwrap().as_ref().map(|c| {
            (
                c.url.clone(),
                c.title.clone(),
                c.can_go_back,
                c.can_go_forward,
            )
        })
    }

    /// Snapshot of interaction state (focused element label, selection text)
    /// from the newest published frame. `None` before the first frame.
    pub fn interaction_state(&self) -> Option<(Option<String>, Option<String>)> {
        self.shared
            .latest
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| (c.focused_id.clone(), c.selection.clone()))
    }

    /// Whether the latest event stream said a load is in flight (snapshot).
    pub fn is_loading(&self) -> bool {
        *self.shared.loading.lock().unwrap()
    }

    /// Block until the next frame arrives or `timeout` elapses. The one
    /// sanctioned wait in the API — everything else is polling-free by
    /// design. Returns `None` on timeout; budget [`DEFAULT_LOAD_TIMEOUT`]
    /// for a cold network navigation.
    pub fn wait_for_frame(&self, timeout: Duration) -> Option<RenderedFrame> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(f) = self.take_frame() {
                return Some(f);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
    }

    /// [`Self::wait_for_frame`] with the documented default budget — the
    /// shorthand for "block until this navigation settles".
    pub fn wait_for_load(&self) -> Option<RenderedFrame> {
        self.wait_for_frame(DEFAULT_LOAD_TIMEOUT)
    }
}

impl EngineBuilder {
    /// Page area the embedder paints into, in logical points.
    pub fn viewport(mut self, width: f32, height: f32) -> Self {
        self.viewport = (width, height);
        self
    }

    /// First page loaded when the thread starts (default `about:start`).
    pub fn start_url(mut self, url: &str) -> Self {
        self.start_url = url.to_string();
        self
    }

    /// SCRIPTING_DECISION.md: enable page JavaScript (feature `page-js`).
    /// Off by default; only built engines with the feature can execute
    /// page scripts at all.
    pub fn page_js(mut self, on: bool) -> Self {
        self.page_js = on;
        self
    }

    /// Spawn the engine thread. The thread navigates to the start URL
    /// immediately; `wait_for_frame`/`take_frame` will see it shortly.
    pub fn build(self) -> Engine {
        self.build_with_measurer(Box::new(crate::ApproxMeasurer))
    }

    /// Like [`EngineBuilder::build`] but with a host-supplied measurer (real
    /// fonts). The measurer must be `Send` — it is called from the engine
    /// thread.
    pub fn build_with_measurer(
        self,
        measurer: Box<dyn kitae_layout::TextMeasurer + Send>,
    ) -> Engine {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel::<Command>();

        let host = MailboxHost {
            shared: shared.clone(),
        };
        let mut inner = Browser::new(Box::new(host), measurer, self.viewport);
        // Phase 0: the facade is a product host — top-level http(s) loads
        // ride the shared fetch pool and finish on the loop's idle tick
        // below. Local schemes (about:, file:, data:) still load inline,
        // and so do POST submits.
        inner.set_background_fetch(true);
        inner.set_page_js(self.page_js);
        let browser = SendBrowser(inner);
        let start = self.start_url;
        std::thread::Builder::new()
            .name("kitae-engine".into())
            .spawn(move || {
                let mut browser = browser;
                browser.0.navigate(&start);
                loop {
                    // The facade's idle tick, mirroring the C-ABI engine:
                    // wake regularly so pooled loads apply, transitions
                    // advance and downloads narrate even when no command
                    // arrives. A 2 ms idle tick costs nothing.
                    let cmd = match rx.recv_timeout(Duration::from_millis(2)) {
                        Ok(cmd) => cmd,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let b = &mut browser.0;
                            b.poll_fetches();
                            b.poll_page_js();
                            b.tick_transitions();
                            b.poll_downloads();
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    match cmd {
                        Command::Navigate(url) => browser.0.navigate(&url),
                        Command::Back => browser.0.go(-1),
                        Command::Forward => browser.0.go(1),
                        Command::Reload => browser.0.reload(),
                        Command::Scroll(dy) => browser.0.scroll_by(dy),
                        Command::Resize(w, h) => browser.0.resize(w, h),
                        Command::Click(x, y) => browser.0.click(x, y),
                        Command::FocusNext => {
                            browser.0.focus_next();
                        }
                        Command::FocusPrev => {
                            browser.0.focus_prev();
                        }
                        Command::Activate => {
                            browser.0.activate_focused();
                        }
                        Command::SetInput(id, v) => {
                            browser.0.set_input_value(&id, &v);
                        }
                        Command::Submit(id) => {
                            browser.0.submit_form(&id);
                        }
                        Command::SelectAll => {
                            browser.0.select_all();
                        }
                        Command::ClearSelection => browser.0.clear_selection(),
                        Command::SetHover(id) => {
                            browser.0.set_hover_id(id.as_deref());
                        }
                        Command::SetActive(id) => {
                            browser.0.set_active_id(id.as_deref());
                        }
                        Command::MediaOpen | Command::MediaState => browser.0.media_open(),
                        Command::MediaPlay(playing, position) => {
                            browser.0.media_play(playing, position);
                        }
                        Command::LoadHtml(html, base) => browser.0.load_html(&html, &base),
                        Command::SetPassword(pw) => browser.0.set_qtsq_password(&pw),
                        Command::SetEnvironment(env) => {
                            browser.0.set_environment(env);
                        }
                        Command::SetPreferences(env) => {
                            browser.0.set_preferences(&env);
                        }
                        Command::SetMediaType(media) => {
                            browser.0.set_media_type(media);
                        }
                        Command::SavePage(path) => {
                            // Outcomes surface as EngineEvent::QtsqSaved,
                            // not through this send's return.
                            let _ = browser.0.save_page_qtsq(&path);
                        }
                        Command::Escape => browser.0.escape_key(),
                        Command::StepFocus(d) => {
                            browser.0.step_focused(d);
                        }
                        // A11a: caret + text editing for focused form controls.
                        Command::TypeText(t) => {
                            browser.0.type_into_focused(&t);
                        }
                        Command::Backspace => {
                            browser.0.backspace_focused();
                        }
                        Command::DeleteForward => {
                            browser.0.delete_forward();
                        }
                        Command::MoveCaret(d) => {
                            browser.0.move_caret(d);
                        }
                        Command::SetCaret(o) => {
                            browser.0.set_caret(o);
                        }
                        Command::ExtendCaret(d) => {
                            browser.0.extend_caret(d);
                        }
                        Command::SelectionBegin(o) => {
                            browser.0.begin_selection(o);
                        }
                        Command::SelectionExtend(o) => {
                            browser.0.extend_selection(o);
                        }
                        Command::SelectionBeginPoint(x, y) => {
                            browser.0.begin_selection_at_point(x, y);
                        }
                        Command::SelectionExtendPoint(x, y) => {
                            browser.0.extend_selection_at_point(x, y);
                        }
                        Command::SelectionEnd => {
                            browser.0.end_selection();
                        }
                        Command::CompositionUpdate(text) => {
                            browser.0.update_marked_text(&text);
                        }
                        Command::CompositionCommit(text) => match text {
                            Some(text) => {
                                browser.0.commit_composition_text(&text);
                            }
                            None => {
                                browser.0.commit_composition();
                            }
                        },
                        Command::CompositionCancel => {
                            browser.0.cancel_composition();
                        }
                    }
                }
            })
            .expect("kitae_core: could not spawn the engine thread");

        Engine { tx, shared }
    }
}

/// The `Browser` (and its boxed host + measurer) is moved to the engine
/// thread and only that thread touches it afterwards; the channel and the
/// mutex mailboxes are the only cross-thread surfaces. Same arrangement as
/// the C-ABI engine (`ffi::Engine`).
struct SendBrowser(Browser);

// Safety: see the comment above. Confined here; nothing else relies on it.
unsafe impl Send for SendBrowser {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_navigate_is_rejected_without_touching_the_engine() {
        let engine = Engine::builder()
            .viewport(320.0, 240.0)
            .start_url("about:blank")
            .build();
        // Consume the start-page frame the thread publishes on boot.
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("start frame");
        engine.poll_events();
        assert_eq!(engine.navigate("   "), Err(EngineError::EmptyUrl));
        assert_eq!(engine.navigate(""), Err(EngineError::EmptyUrl));
        // The rejected navigation must not have produced frames or events.
        // Boot events race the rejection under parallel-suite scheduling,
        // so require a demonstrable quiescence window: no events and no
        // frames across two drains a beat apart. Anything the rejection
        // produced would keep the stream busy past the deadline.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let quiet = engine.poll_events().is_empty() && engine.take_frame().is_none();
            if quiet {
                std::thread::sleep(Duration::from_millis(100));
                if engine.poll_events().is_empty() && engine.take_frame().is_none() {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "event stream never went quiet after the rejected navigation"
            );
        }
        assert!(!engine.is_loading());
    }

    #[test]
    fn start_page_is_published_with_loading_events() {
        let engine = Engine::builder()
            .viewport(640.0, 480.0)
            .start_url("about:start")
            .build();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("start page frame within 5s");
        assert_eq!(frame.url, START_URL);
        assert_eq!(frame.title, "Start");
        assert!(!frame.can_go_back);
        assert!(!frame.can_go_forward);
        // The start page paints its <h1> and links with real geometry.
        assert!(!frame.display_list.texts.is_empty());
        assert!(!frame.display_list.links.is_empty());
        assert!(frame.content_height > 0.0);

        // Event stream: Loading(true) then Loading(false), plus the
        // location change for the start page. Loading(false) is pushed by
        // the engine thread right after the frame publishes — poll until it
        // lands rather than trusting one snapshot (full-suite scheduling
        // can pause that thread past the first poll).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut events = engine.poll_events();
        while !events.contains(&EngineEvent::Loading(false)) && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
            events.extend(engine.poll_events());
        }
        assert!(
            matches!(events.first(), Some(EngineEvent::Loading(true))),
            "first event is Loading(true), got {events:?}"
        );
        assert!(events.contains(&EngineEvent::Loading(false)));
        assert!(events
            .iter()
            .any(|e| matches!(e, EngineEvent::LocationChanged(u) if u == START_URL)));
        // Everything was drained.
        assert!(engine.poll_events().is_empty());
        assert!(!engine.is_loading());
        assert_eq!(engine.last_error(), None);
    }

    /// H3 print media on the public Rust facade: the same switch the C ABI
    /// exposes, so an in-process embedder can lay a page out for paper,
    /// take the print frame, and put the screen back.
    #[test]
    fn set_media_type_republishes_a_print_frame() {
        let engine = Engine::builder()
            .viewport(480.0, 320.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("boot frame");
        engine.load_html(
            "<html><head><style>\
             @media screen { p { color: #020202 } }\
             @media print { p { color: #010101 } }\
             </style></head><body><p>onpaper</p></body></html>",
            "https://example.com/print",
        );
        let color = |frame: &RenderedFrame| {
            frame
                .display_list
                .texts
                .iter()
                .find(|t| t.text == "onpaper")
                .map(|t| t.color)
        };
        let screen = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("page frame");
        assert!(!screen.print_media);
        assert_eq!(color(&screen), Some(0x0202_02FF));

        engine.set_media_type(kitae_style::MediaType::Print);
        let printed = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("print frame");
        assert!(printed.print_media, "the frame says it is for paper");
        assert_eq!(color(&printed), Some(0x0101_01FF));

        engine.set_media_type(kitae_style::MediaType::Screen);
        let restored = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("screen frame");
        assert!(!restored.print_media);
        assert_eq!(color(&restored), Some(0x0202_02FF));
    }

    #[test]
    fn history_commands_drive_chrome_state() {
        let engine = Engine::builder()
            .viewport(480.0, 320.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("first frame");
        engine.navigate_lossy("about:start");
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("second frame");
        assert_eq!(frame.url, START_URL);
        assert!(frame.can_go_back);
        assert!(!frame.can_go_forward);

        engine.go_back();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("back frame");
        assert_eq!(frame.url, "about:blank");
        assert!(!frame.can_go_back);
        assert!(frame.can_go_forward);

        engine.go_forward();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("forward frame");
        assert_eq!(frame.url, START_URL);
        assert!(frame.can_go_back && !frame.can_go_forward);
    }

    #[test]
    fn clones_share_one_session() {
        let engine = Engine::builder().start_url("about:start").build();
        let clone = engine.clone();
        clone
            .wait_for_frame(Duration::from_secs(5))
            .expect("frame through the clone");
        // One draining mailbox: what the clone consumed, the original
        // cannot see again.
        assert!(engine.take_frame().is_none());
        // And chrome state is observable from either handle while no new
        // frame is pending.
        assert_eq!(
            clone.chrome_state().map(|(url, ..)| url),
            Some(START_URL.into())
        );
        assert_eq!(
            engine.chrome_state().map(|(url, ..)| url),
            Some(START_URL.into())
        );
    }

    #[test]
    fn resize_republishes_at_the_new_width() {
        let engine = Engine::builder()
            .viewport(300.0, 200.0)
            .start_url("about:start")
            .build();
        let narrow = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("narrow frame");
        engine.resize(800.0, 200.0);
        let wide = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("wide frame");
        // The height stays pinned: every start-page section is
        // min-height:100vh, so a taller viewport makes the document
        // taller by design. Widening alone must never add height.
        assert!(
            wide.display_list.content_height <= narrow.display_list.content_height,
            "wider viewport must not make the document taller ({wide:?} vs {narrow:?})"
        );
    }

    /// SCRIPTING_DECISION.md: the facade's page-js opt-in — builder flag
    /// to script-phase to republished frame, drained on the loop tick.
    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_builder_runs_scripts_and_publishes() {
        let engine = Engine::builder()
            .viewport(400.0, 600.0)
            .start_url("about:blank")
            .page_js(true)
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("blank frame");
        engine.load_html(
            "<html><body><p id=\"x\"></p><script>\
             document.getElementById('x').textContent = 'facade words';\
             </script></body></html>",
            "https://example.test/facade",
        );
        // load_html processes + publishes on the engine thread; the next
        // frame after the boot frame is the scripted page.
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("scripted page frame");
        let text = frame
            .display_list
            .texts
            .iter()
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.contains("facade words"),
            "scripted content reaches the frame: {text:?}; url: {}",
            frame.url
        );
    }

    #[test]
    fn rendered_frames_expose_the_effective_resource_base() {
        let engine = Engine::builder().start_url("about:blank").build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("blank frame");
        engine.load_html(
            "<html><head><base href='https://cdn.example/assets/'></head>\
             <body><a href='next.html'>next</a></body></html>",
            "https://example.test/docs/page.html",
        );
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("html frame");
        assert_eq!(frame.resource_base_url, "https://cdn.example/assets/");
        assert_eq!(frame.display_list.links[0].href, "next.html");
    }

    #[test]
    fn interaction_focus_input_and_selection_flow() {
        let engine = Engine::builder()
            .viewport(400.0, 600.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("blank frame");
        engine.load_html(
            "<html><body><a href='/a' id='l1'>one</a><form id='f' action='/s'><input id='q' name='q' value='hi'></form></body></html>",
            "https://example.com/",
        );
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("html frame");
        assert_eq!(frame.focused_id, None);
        assert_eq!(frame.selection, None);

        engine.focus_next();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("focus frame");
        assert_eq!(frame.focused_id.as_deref(), Some("l1"));
        assert_eq!(
            engine.interaction_state(),
            Some((Some("l1".to_string()), None))
        );

        engine.set_input("q", "hello");
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("input frame");
        assert!(frame.display_list.texts.iter().any(|t| t.text == "hello"));

        engine.select_all();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("selection frame");
        assert!(frame
            .selection
            .as_deref()
            .map(|s| s.contains("one"))
            .unwrap_or(false));
        engine.clear_selection();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("clear frame");
        assert_eq!(frame.selection, None);
    }

    // ── QTSQ viewer pages through the Engine facade (J2) ──

    fn qtsq_fixture_url(name: &str) -> String {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("kitae_qtsq")
            .join("testdata")
            .join(name);
        // Unique per call: tests run in parallel and one name in the shared
        // temp dir would race the engine's fetch of a re-copied file.
        static CALL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dst = std::env::temp_dir().join(format!(
            "kitae_j2_engine_{}_{}_{}",
            std::process::id(),
            CALL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            name
        ));
        std::fs::copy(&src, &dst).expect("copy fixture");
        format!("file://{}", dst.display())
    }

    #[test]
    fn engine_navigates_to_qtsq_viewers_and_history_works() {
        let engine = Engine::builder()
            .viewport(480.0, 320.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("first frame");

        // Text payload → decoded text page, filename as title.
        let text_url = qtsq_fixture_url("fix_text.qtsq");
        engine.navigate_lossy(&text_url);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("text viewer frame");
        assert!(
            frame.title.ends_with("fix_text.qtsq"),
            "title: {}",
            frame.title
        );
        // Display-list text runs are per-word (layout contract); the decoded
        // sentence reads back joined.
        let page_text = frame
            .display_list
            .texts
            .iter()
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            page_text.contains("Hello from QTSQ!"),
            "decoded text in the display list: {page_text:?}"
        );

        // Tensor payload → viewer page: heat-map strip image + values table.
        let tensor_url = qtsq_fixture_url("fix_tensor.qtsq");
        engine.navigate_lossy(&tensor_url);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("tensor viewer frame");
        assert!(
            frame.title.ends_with("fix_tensor.qtsq"),
            "title: {}",
            frame.title
        );
        assert!(
            frame
                .display_list
                .texts
                .iter()
                .any(|t| t.text.contains("Tensor")),
            "strategy metadata in the display list"
        );
        assert!(
            !frame.display_list.images.is_empty(),
            "heat-map strip reaches the display list"
        );
        assert!(
            frame
                .display_list
                .images
                .iter()
                .all(|im| im.payload.is_some()),
            "strip raster payload id stamped"
        );

        // Audio payload → media viewer page.
        let audio_url = qtsq_fixture_url("fix_audio.qtsq");
        engine.navigate_lossy(&audio_url);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("audio viewer frame");
        assert!(
            frame
                .display_list
                .texts
                .iter()
                .any(|t| t.text.contains("22050")),
            "audio metadata in the display list"
        );

        // History: blank → text → tensor → audio, one step back at a time.
        engine.go_back();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("back frame");
        assert!(frame.title.ends_with("fix_tensor.qtsq"));
        engine.go_back();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("back frame 2");
        assert!(frame.title.ends_with("fix_text.qtsq"));
        engine.go_back();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("back to blank frame");
        assert_eq!(frame.url, "about:blank");
        engine.go_forward();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("forward frame");
        assert!(frame.title.ends_with("fix_text.qtsq"));
    }

    #[test]
    fn engine_media_frames_carry_media_info_and_transport() {
        let _lock = crate::qtsq_test_lock();
        crate::clear_qtsq_rasters();
        crate::clear_qtsq_media();
        let engine = Engine::builder()
            .viewport(480.0, 320.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("first frame");

        let audio_url = qtsq_fixture_url("fix_audio.qtsq");
        engine.navigate_lossy(&audio_url);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("audio viewer frame");
        let media = frame.media.as_ref().expect("frame carries MediaInfo");
        assert_eq!(media.kind, crate::MediaKind::Audio);
        assert_eq!(media.rate, 22050);
        assert_eq!(media.channels, 1);
        assert!((media.duration - 1.0).abs() < 0.01);
        assert!(!media.playing);
        assert!(
            media.caf_path.ends_with(".caf"),
            "CAF handoff: {}",
            media.caf_path
        );
        assert!(
            std::fs::metadata(&media.caf_path).is_ok(),
            "CAF file written"
        );

        // Transport mirrors through frames after media_play.
        engine.media_open();
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("open reply frame");
        let media = frame.media.as_ref().expect("media after open");
        assert!(!media.playing, "open alone does not start playback");

        engine.media_play(true, 0.25);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("play reply frame");
        let media = frame.media.as_ref().expect("media after play");
        assert!(media.playing);
        assert!((media.position - 0.25).abs() < 1e-4, "position mirrored");

        // Plain HTML clears the media on the next frame.
        engine.load_html("<h1>plain</h1>", "about:blank");
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("plain frame");
        assert!(frame.media.is_none(), "plain page has no media");

        crate::clear_qtsq_media();
        std::fs::remove_file(&media.caf_path).ok();
    }

    // ── QTSQ password unlock through the Engine facade ──
    // fix_text_locked.qtsq is a vault-password-protected text payload
    // (placeholder passphrase "hunter2", set by
    // kitae_qtsq/examples/make_locked_fixture.rs — no real credentials).

    fn page_text_of(frame: &RenderedFrame) -> String {
        frame
            .display_list
            .texts
            .iter()
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn engine_locked_qtsq_page_flags_and_notes_encryption() {
        let engine = Engine::builder()
            .viewport(480.0, 320.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("first frame");

        let url = qtsq_fixture_url("fix_text_locked.qtsq");
        engine.navigate_lossy(&url);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("locked frame");
        assert!(frame.qtsq_locked, "locked flag on the frame");
        let page_text = page_text_of(&frame);
        assert!(
            page_text.contains("encrypted"),
            "locked note in the page: {page_text:?}"
        );
        assert!(
            !page_text.contains("Secret"),
            "payload must not leak on the locked page"
        );
    }

    #[test]
    fn engine_set_password_reloads_and_unlocks_locked_qtsq() {
        let engine = Engine::builder()
            .viewport(480.0, 320.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("first frame");

        let url = qtsq_fixture_url("fix_text_locked.qtsq");
        engine.navigate_lossy(&url);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("locked frame");
        assert!(frame.qtsq_locked, "locked before the password");

        engine.set_password("hunter2");
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("unlocked frame");
        assert!(!frame.qtsq_locked, "flag clears after a successful unlock");
        let page_text = page_text_of(&frame);
        assert!(
            page_text.contains("Secret words behind the password."),
            "decoded payload in the page: {page_text:?}"
        );
    }

    #[test]
    fn engine_wrong_qtsq_password_keeps_locked_flag_with_rejection_note() {
        let engine = Engine::builder()
            .viewport(480.0, 320.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("first frame");

        let url = qtsq_fixture_url("fix_text_locked.qtsq");
        engine.navigate_lossy(&url);
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("locked frame");

        engine.set_password("not-the-passphrase");
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("wrong-password frame");
        assert!(frame.qtsq_locked, "still locked after a wrong password");
        let page_text = page_text_of(&frame);
        assert!(
            page_text.contains("did not open"),
            "rejection note in the page: {page_text:?}"
        );
        assert!(
            !page_text.contains("Secret"),
            "payload must not leak on a wrong password"
        );
    }

    /// Slice ④: signature verdicts ride the frame (`qtsq_sig`) and signed
    /// viewer pages lead with the banner. The test installs its own device
    /// sign key through `KITAE_QTSQ_KEY_DIR` (lock-guarded) so verdicts are
    /// hermetic — the real machine's key must never decide a test.
    #[test]
    fn engine_signature_verdicts_on_signed_qtsq_pages() {
        let _lock = crate::qtsq_test_lock();
        let keys = std::env::temp_dir().join(format!("kitae_sig_keys_{}", std::process::id()));
        std::fs::create_dir_all(&keys).expect("key dir");
        let pages = std::env::temp_dir().join(format!("kitae_sig_pages_{}", std::process::id()));
        std::fs::create_dir_all(&pages).expect("page dir");
        let saved_key_dir = std::env::var("KITAE_QTSQ_KEY_DIR").ok();
        std::env::set_var("KITAE_QTSQ_KEY_DIR", &keys);

        let (pk, sk) = kitae_qtsq::sign_keygen().expect("sign keygen");
        std::fs::write(keys.join("device_sign.pk"), pk).expect("device sign pk");
        let signed = kitae_qtsq::encode_text_secure(
            b"# Signed note\n\nBody words behind the signature.",
            Some("test-placeholder-pw"),
            None,
            Some(&sk),
        )
        .expect("encode signed payload");
        let signed_path = pages.join("signed.qtsq");
        std::fs::write(&signed_path, &signed).expect("write signed file");
        // Tampered copy: one flipped byte mid-file (lib-level tests prove
        // this lands past the header, in checksum/verify territory).
        let mut tampered = signed.clone();
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0x5a;
        let tampered_path = pages.join("tampered.qtsq");
        std::fs::write(&tampered_path, &tampered).expect("write tampered file");

        let engine = Engine::builder()
            .viewport(480.0, 320.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("first frame");
        engine.set_password("test-placeholder-pw");
        // set_password republishes about:blank when a page is open — drain
        // that frame so the signed navigation's frame isn't confused with it.
        let _ = engine.wait_for_frame(Duration::from_secs(5));

        // Valid: signed with the device key this "machine" holds.
        let url = format!("file://{}", signed_path.display());
        engine.navigate_lossy(&url);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("signed frame");
        assert_eq!(
            frame.qtsq_sig,
            crate::QtsqSigState::Valid,
            "device-signed payload verifies"
        );
        assert!(!frame.qtsq_locked, "session password unlocks the payload");
        let page_text = page_text_of(&frame);
        assert!(
            page_text.contains("Signature verified"),
            "valid banner in the page: {page_text:?}"
        );
        assert!(
            page_text.contains("Body words"),
            "decoded body alongside the banner: {page_text:?}"
        );

        // Invalid: the same payload with one byte flipped.
        let url = format!("file://{}", tampered_path.display());
        engine.navigate_lossy(&url);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("tampered frame");
        assert_eq!(
            frame.qtsq_sig,
            crate::QtsqSigState::Invalid,
            "flipped byte fails the verdict"
        );
        let page_text = page_text_of(&frame);
        assert!(
            page_text.contains("Signature check failed"),
            "invalid banner in the page: {page_text:?}"
        );

        // Unverified: signed payload, but the key dir no longer holds the
        // device sign key.
        std::fs::rename(keys.join("device_sign.pk"), keys.join("sign.pk.off"))
            .expect("hide device key");
        let url = format!("file://{}", signed_path.display());
        engine.navigate_lossy(&url);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("unverified frame");
        assert_eq!(
            frame.qtsq_sig,
            crate::QtsqSigState::Unverified,
            "no local key leaves the signature unchecked"
        );
        let page_text = page_text_of(&frame);
        assert!(
            page_text.contains("no key to verify"),
            "unverified banner in the page: {page_text:?}"
        );
        std::fs::rename(keys.join("sign.pk.off"), keys.join("device_sign.pk"))
            .expect("restore device key");

        // Unsigned: plain payloads (and every non-QTSQ page) stay Unsigned.
        engine.navigate_lossy(&qtsq_fixture_url("fix_text.qtsq"));
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("plain frame");
        assert_eq!(frame.qtsq_sig, crate::QtsqSigState::Unsigned);
        assert!(
            !page_text_of(&frame).contains("Signature"),
            "no banner on unsigned pages"
        );

        match saved_key_dir {
            Some(v) => std::env::set_var("KITAE_QTSQ_KEY_DIR", v),
            None => std::env::remove_var("KITAE_QTSQ_KEY_DIR"),
        }
        std::fs::remove_dir_all(&pages).ok();
        std::fs::remove_dir_all(&keys).ok();
    }

    /// Slice ②: the embedder-level Save-Page command writes a device
    /// archive and reports the outcome as an [`EngineEvent::QtsqSaved`].
    /// The key dir is redirected (lock-guarded) so no real device keys are
    /// generated by the test run.
    #[test]
    fn engine_save_page_qtsq_emits_qtsq_saved_event() {
        let _lock = crate::qtsq_test_lock();
        let keys = std::env::temp_dir().join(format!("kitae_save_keys_{}", std::process::id()));
        std::fs::create_dir_all(&keys).expect("key dir");
        let saved_key_dir = std::env::var("KITAE_QTSQ_KEY_DIR").ok();
        std::env::set_var("KITAE_QTSQ_KEY_DIR", &keys);

        let out_path =
            std::env::temp_dir().join(format!("kitae_save_page_{}.qtsq", std::process::id()));

        let engine = Engine::builder()
            .viewport(480.0, 320.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("first frame");

        // Success path: a real page, saved to a real path.
        engine.load_html(
            "<html><head><title>Save me</title></head><body><p>archive body</p></body></html>",
            "https://example.com/save/page.html",
        );
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("html frame");
        engine.save_page_qtsq(out_path.to_str().expect("utf8 path"));

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let saved = loop {
            let Some(ev) = engine.poll_events().into_iter().find_map(|ev| match ev {
                EngineEvent::QtsqSaved { path, error } => Some((path, error)),
                _ => None,
            }) else {
                assert!(std::time::Instant::now() < deadline, "no QtsqSaved event");
                std::thread::sleep(Duration::from_millis(20));
                continue;
            };
            break ev;
        };
        assert_eq!(saved.0, out_path.to_str().unwrap());
        assert_eq!(saved.1, None, "no error on the success path: {:?}", saved.1);

        // The archive exists, is device-encrypted + signed, and decodes
        // passwordless through the device key path.
        let bytes = std::fs::read(&out_path).expect("archive written");
        let info = kitae_qtsq::probe(&bytes).expect("probe");
        assert!(info.encrypted && info.signed, "encrypted + signed archive");
        let text = kitae_qtsq::decode_text(&bytes, None).expect("passwordless decode");
        assert!(
            text.starts_with("<!--kitae-archive v1\nbase=https://example.com/save/page.html\n"),
            "archive envelope: {text:?}"
        );

        // Failure path: an unwritable destination — the event must carry a
        // message instead of a path-side success.
        engine.save_page_qtsq("/nonexistent_dir_for_kitae_save_test/archive.qtsq");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let failed = loop {
            let Some(ev) = engine.poll_events().into_iter().find_map(|ev| match ev {
                EngineEvent::QtsqSaved { path, error } => Some((path, error)),
                _ => None,
            }) else {
                assert!(std::time::Instant::now() < deadline, "no failure event");
                std::thread::sleep(Duration::from_millis(20));
                continue;
            };
            break ev;
        };
        assert_eq!(
            failed.1.as_deref().map(|e| e.contains("write:")),
            Some(true),
            "failure message: {:?}",
            failed.1
        );

        match saved_key_dir {
            Some(v) => std::env::set_var("KITAE_QTSQ_KEY_DIR", v),
            None => std::env::remove_var("KITAE_QTSQ_KEY_DIR"),
        }
        std::fs::remove_file(&out_path).ok();
        std::fs::remove_dir_all(&keys).ok();
    }

    #[test]
    fn hover_restyle_publishes_recolored_frame() {
        let engine = Engine::builder()
            .viewport(400.0, 600.0)
            .start_url("about:blank")
            .build();
        engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("blank frame");
        engine.load_html(
            "<html><head><style>a:hover { color: #ff0000 }</style></head>\
             <body><a href='/x' id='l'>go</a></body></html>",
            "https://example.com/",
        );
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("html frame");
        let resting = frame
            .display_list
            .texts
            .iter()
            .find(|t| t.text == "go")
            .unwrap()
            .color;
        assert_eq!(resting, 0x0B57D0FF);

        engine.set_hover(Some("l"));
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("hover frame");
        let hovered = frame
            .display_list
            .texts
            .iter()
            .find(|t| t.text == "go")
            .unwrap()
            .color;
        assert_eq!(hovered, 0xFF0000FF);

        engine.set_hover(None);
        let frame = engine
            .wait_for_frame(Duration::from_secs(5))
            .expect("clear frame");
        let cleared = frame
            .display_list
            .texts
            .iter()
            .find(|t| t.text == "go")
            .unwrap()
            .color;
        assert_eq!(cleared, 0x0B57D0FF);
    }
}
