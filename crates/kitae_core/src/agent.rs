//! The agent host — a real `Browser` behind the verb vocabulary.
//!
//! This is where the agent plane's verbs get their *meaning*: `navigate`
//! loads and settles a document, `click` resolves a selector to the box the
//! engine painted and hit-tests it, `extract` returns the structured
//! document, `archive` writes signed evidence. One implementation serves
//! every transport — the headless CLI, the MCP server, the HTTP server and
//! the app through FFI — so a verb cannot mean one thing over stdio and
//! another in the app.
//!
//! Three seams keep this honest about what is *not* installed:
//!
//! - [`PermissionPolicy`] — consulted before every act. [`AllowList`]'s
//!   default is read-only: a session nobody configured cannot mutate
//!   anything, and every refusal names the flag that would allow it.
//! - [`PageRaster`] — pixels and print. `kitae_core` owns no font stack
//!   (glyph rasterization lives above it, in the app's Core Text or in
//!   `kitae_headless`'s pure-Rust stack), so the default refuses with an
//!   error that says exactly that rather than returning an empty PNG.
//! - The engine's own budgets — [`AgentLimits`] bounds settling, watching
//!   and every text result, so a hostile page cannot hang an agent or
//!   balloon its context.
//!
//! Every act is recorded in a bounded journal ([`AgentSession::journal`]):
//! what an agent did to a page is itself evidence, and the journal is what
//! the app shows in its activity panel.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kitae_agent::{AgentApi, Decision, Target, Verb};
use kitae_json::Value;

use crate::extract;
use crate::Browser;

/// A host for headless engines: it counts frames, remembers the loading
/// flag and keeps engine notices, and does nothing else. Everything the
/// agent plane needs is answered by the engine itself (text, links,
/// geometry, extraction), so a headless session needs no window — but a
/// `Browser` always needs *a* host, which is this one.
#[derive(Default)]
pub struct HeadlessHost {
    frames: usize,
    loading: bool,
    notices: Vec<String>,
}

impl HeadlessHost {
    /// Frames published so far (the engine's own repaint accounting).
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// The loading flag the engine last reported.
    pub fn loading(&self) -> bool {
        self.loading
    }

    /// Engine notices this host received (`auth`, `insecure`, `download`,
    /// `inspect` …) — the headless stand-in for the app's prompts.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }
}

impl crate::Host for HeadlessHost {
    fn publish(&mut self, _state: &crate::PublicState, _dl: &kitae_layout::DisplayList) {
        self.frames += 1;
    }

    fn scroll_updated(&mut self, _state: &crate::PublicState) {}

    fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    fn open_external(&mut self, url: &str) {
        self.notices.push(format!("open-external\t{url}"));
    }

    fn notify(&mut self, message: &str) {
        self.notices.push(message.to_string());
    }
}

/// Settling, watching and result budgets. One struct so a host has one
/// place to reason about how long an act may take.
#[derive(Debug, Clone, Copy)]
pub struct AgentLimits {
    /// How long a navigation may take to settle (fetch + scripts + timers).
    pub settle_ms: u64,
    /// How long a `watch` may wait for its selector.
    pub watch_ms: u64,
    /// Padding added to a `watch` timeout before the session gives up.
    pub watch_slack_ms: u64,
    /// Poll interval while waiting.
    pub poll_interval_ms: u64,
    /// Cap on `read`'s text.
    pub max_text_chars: usize,
    /// Cap on journal entries kept (the newest win).
    pub journal_max: usize,
    /// How long `click`/`submit` wait for the click's consequences.
    pub act_settle_ms: u64,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            settle_ms: 5_000,
            watch_ms: 10_000,
            watch_slack_ms: 1_000,
            poll_interval_ms: 10,
            max_text_chars: 128 * 1024,
            journal_max: 256,
            act_settle_ms: 3_000,
        }
    }
}

/// Who may do what. Implemented by [`AllowList`] for headless hosts and by
/// the app for interactive prompts. Single-threaded like the engine it
/// guards — a policy answers on the thread that owns the browser.
pub trait PermissionPolicy {
    fn check(&self, verb: &Verb) -> Decision;
    /// Verbs a script declared. Declarations inform a policy (an app can
    /// pre-answer a prompt; a headless list can auto-extend); they never
    /// grant by themselves.
    fn declare(&self, _verbs: &[String]) {}
}

/// A static policy: named verbs, plus optional blanket read-only access.
#[derive(Debug, Clone, Default)]
pub struct AllowList {
    verbs: Vec<String>,
    read_only: bool,
}

impl AllowList {
    /// Nothing is allowed (the strictest policy).
    pub fn none() -> Self {
        Self::default()
    }

    /// Read-only verbs only — what a session nobody configured gets.
    pub fn read_only() -> Self {
        Self {
            verbs: Vec::new(),
            read_only: true,
        }
    }

    /// Everything (a host that already gated the process, e.g. a container).
    pub fn all() -> Self {
        Self {
            verbs: vec!["*".to_string()],
            read_only: true,
        }
    }

    /// The named verbs, plus read-only access.
    pub fn new(verbs: &[&str]) -> Self {
        Self {
            verbs: verbs.iter().map(|v| v.to_string()).collect(),
            read_only: true,
        }
    }

    /// Whether read-only verbs are auto-allowed (default: yes — a policy
    /// that has to name `read` is a policy nobody writes correctly).
    pub fn with_read_only(mut self, on: bool) -> Self {
        self.read_only = on;
        self
    }

    pub fn allows(&self, verb: &Verb) -> bool {
        self.verbs.iter().any(|v| v == "*" || v == verb.name())
            || (self.read_only && verb.is_read_only())
    }
}

impl PermissionPolicy for AllowList {
    fn check(&self, verb: &Verb) -> Decision {
        if self.allows(verb) {
            return Decision::Allow;
        }
        Decision::Deny(format!(
            "`{}` is not in this session's allow-list (start with --allow {} \
             or --allow \"*\" to permit it)",
            verb.name(),
            verb.name()
        ))
    }
}

/// Every permission refusal starts with this, so a host can tell "the user
/// must be asked" from "the verb failed" without parsing prose.
pub const PERMISSION_PREFIX: &str = "permission denied for ";

/// Pixels and print for the verbs that need them.
pub trait PageRaster: Send + Sync {
    /// PNG bytes of the painted page, device pixels.
    fn png(&self, browser: &Browser) -> Result<Vec<u8>, String>;
    /// Print-styled PDF bytes of the page.
    fn pdf(&self, browser: &Browser) -> Result<Vec<u8>, String>;
}

/// The default: no rasterizer was installed, and the error says where one
/// comes from. Never a blank image — an agent that gets empty pixels
/// believes them.
struct NoRaster;

impl PageRaster for NoRaster {
    fn png(&self, _browser: &Browser) -> Result<Vec<u8>, String> {
        Err("no page rasterizer is installed in this session; link \
             kitae_headless (pure Rust) or install a host rasterizer to use \
             `screenshot`"
            .to_string())
    }

    fn pdf(&self, _browser: &Browser) -> Result<Vec<u8>, String> {
        Err("no page rasterizer is installed in this session; link \
             kitae_headless (pure Rust) or install a host rasterizer to use \
             `pdf`"
            .to_string())
    }
}

/// A [`PageRaster`] the *host* implements, reached through C function
/// pointers — the same shape the app already uses to inject Core Text
/// measurement (`KitaeMeasurerVtable`), and for the same reason.
///
/// The engine cannot call the pure-Rust rasterizer directly: the CPU
/// compositor it paints with lives *above* `kitae_core` in the crate graph
/// (`kitae_render_gpu` depends on the engine), so depending on it here would
/// be a cycle. A host that links a rasterizer hands over an opaque context
/// plus two functions instead, and the engine calls them on its own thread
/// with the browser it owns.
///
/// Wire contract:
/// * Both functions run on one thread at a time, with read-only access to the
///   browser they are handed.
/// * Each writes up to `cap` bytes into `buf` and returns the **full** length
///   of its output — so the caller asks with a null buffer for the size, then
///   calls again. A returned length above `cap` means "truncated, call
///   again"; the caller never reads past `cap`.
/// * A negative return is a **failure**: `-n` means the first `n` bytes of
///   `buf` are the UTF-8 error message.
/// * `free` (when present) runs once, when the session drops.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PageRasterVtable {
    pub ctx: *mut std::ffi::c_void,
    pub png: Option<
        unsafe extern "C" fn(*mut std::ffi::c_void, *const Browser, *mut u8, usize) -> isize,
    >,
    pub pdf: Option<
        unsafe extern "C" fn(*mut std::ffi::c_void, *const Browser, *mut u8, usize) -> isize,
    >,
    pub free: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
}

/// A [`PageRasterVtable`] used as a [`PageRaster`].
pub struct HostRaster {
    vt: PageRasterVtable,
}

impl HostRaster {
    /// # Safety
    /// The caller promises the [`PageRasterVtable`] contract: `ctx` stays
    /// valid until `free` runs, and both functions may be called from any
    /// thread with a `*const Browser` the engine owns.
    pub unsafe fn new(vt: PageRasterVtable) -> Self {
        Self { vt }
    }

    fn call(
        &self,
        f: Option<
            unsafe extern "C" fn(*mut std::ffi::c_void, *const Browser, *mut u8, usize) -> isize,
        >,
        browser: &Browser,
        verb: &str,
    ) -> Result<Vec<u8>, String> {
        let Some(f) = f else {
            return Err(format!("this host installed no rasterizer for `{verb}`"));
        };
        let ptr = browser as *const Browser;
        let mut buf: Vec<u8> = Vec::new();
        // Two passes: the first asks for the size, the second copies. A page
        // that somehow grew between them is retried once, after which the
        // bytes are simply truncated to what fit.
        for _ in 0..2 {
            let len = unsafe { f(self.vt.ctx, ptr, buf.as_mut_ptr(), buf.len()) };
            if len < 0 {
                let n = len.unsigned_abs().min(buf.len());
                return Err(String::from_utf8_lossy(&buf[..n]).into_owned());
            }
            let len = len as usize;
            if len <= buf.len() {
                buf.truncate(len);
                return Ok(buf);
            }
            buf = vec![0; len];
        }
        let len = unsafe { f(self.vt.ctx, ptr, buf.as_mut_ptr(), buf.len()) };
        if len < 0 {
            return Err(format!("rasterizing the page failed ({len})"));
        }
        buf.truncate((len as usize).min(buf.len()));
        Ok(buf)
    }
}

impl Drop for HostRaster {
    fn drop(&mut self) {
        if let Some(free) = self.vt.free {
            unsafe { free(self.vt.ctx) };
        }
    }
}

// Safety: the vtable contract requires both functions to be callable from any
// thread and to touch only the context they were handed.
unsafe impl Send for HostRaster {}
unsafe impl Sync for HostRaster {}

impl PageRaster for HostRaster {
    fn png(&self, browser: &Browser) -> Result<Vec<u8>, String> {
        self.call(self.vt.png, browser, "screenshot")
    }

    fn pdf(&self, browser: &Browser) -> Result<Vec<u8>, String> {
        self.call(self.vt.pdf, browser, "pdf")
    }
}

/// One journal row: what was asked, whether it ran, what came back.
#[derive(Debug, Clone)]
pub struct JournalRow {
    pub seq: u64,
    /// The verb's wire form (the audit record).
    pub verb: String,
    /// `None` when the verb ran; `Some(reason)` when the policy refused.
    pub denied: Option<String>,
    /// A one-line outcome for the activity panel.
    pub outcome: String,
    pub elapsed_ms: u64,
}

/// A `Browser` plus the [`SessionState`] the verbs need: the pairing every
/// transport uses when it owns a browser outright (`kitae_mcp`,
/// `kitae_server`, the tests). The app does not — its engine owns the window's
/// browser — so it keeps a `SessionState` beside that `Browser` and calls the
/// verbs on the state instead.
pub struct AgentSession {
    browser: Browser,
    state: SessionState,
}

impl AgentSession {
    /// A session over a headless browser at `viewport`: the constructor
    /// every transport uses unless a host supplies its own `Browser` (the
    /// app does — it has one with a window already). The policy starts
    /// read-only, so a session nobody configured cannot mutate anything.
    pub fn headless(viewport: (f32, f32)) -> Self {
        let browser = Browser::new(
            Box::new(HeadlessHost::default()),
            Box::new(crate::ApproxMeasurer),
            viewport,
        );
        Self::new(browser)
    }

    pub fn new(browser: Browser) -> Self {
        Self {
            browser,
            state: SessionState::new(),
        }
    }

    pub fn with_policy(mut self, policy: Box<dyn PermissionPolicy>) -> Self {
        self.state.policy = policy;
        self
    }

    pub fn with_raster(mut self, raster: Arc<dyn PageRaster>) -> Self {
        self.state.raster = raster;
        self
    }

    pub fn with_limits(mut self, limits: AgentLimits) -> Self {
        self.state.limits = limits;
        self
    }

    pub fn policy(&self) -> &dyn PermissionPolicy {
        self.state.policy()
    }

    pub fn set_policy(&mut self, policy: Box<dyn PermissionPolicy>) {
        self.state.set_policy(policy);
    }

    /// The browser behind this session — the app reads frames through it.
    pub fn browser(&self) -> &Browser {
        &self.browser
    }

    pub fn browser_mut(&mut self) -> &mut Browser {
        &mut self.browser
    }

    pub fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut SessionState {
        &mut self.state
    }

    pub fn journal(&self) -> &[JournalRow] {
        self.state.journal()
    }

    /// Verbs scripts declared through this session.
    pub fn declared(&self) -> &[String] {
        self.state.declared()
    }

    /// Run one verb against this session's own browser.
    pub fn permit_and_run(&mut self, verb: &Verb) -> Result<String, String> {
        self.state.permit_and_run(&mut self.browser, verb)
    }

    /// Run one verb, skipping the policy — the path taken when a `Decision::
    /// Allow` was already handed out by [`AgentApi::permit`].
    pub fn act_unchecked(&mut self, verb: &Verb) -> Result<String, String> {
        self.state.act_unchecked(&mut self.browser, verb)
    }
}

/// Everything a session carries *besides* the browser: the policy, the
/// rasterizer, the limits, the journal and the transition clock.
///
/// The split is what lets the **app** run the very same verbs over the browser
/// it already owns — the one painting its window, with the user's scroll
/// position, focus and page scripts — instead of a second, invisible browser
/// the user cannot see. The engine keeps a `SessionState` beside its
/// `Browser` and calls `state.permit_and_run(&mut browser, &verb)`;
/// [`AgentSession`] is those two fields bundled, for hosts that own a browser
/// outright.
pub struct SessionState {
    policy: Box<dyn PermissionPolicy>,
    raster: Arc<dyn PageRaster>,
    limits: AgentLimits,
    journal: Vec<JournalRow>,
    seq: u64,
    /// Verbs scripts declared, newest last (bounded).
    declared: Vec<String>,
    /// Accumulated virtual clock for transition ticks — the agent plane has
    /// no frame loop, so it advances CSS animations itself.
    clock: f32,
    /// The host's standing approvals (see [`Self::grant`]).
    grants: Vec<String>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    /// The state a session nobody configured gets: **read-only** policy, no
    /// rasterizer (the verbs that need one refuse), default limits.
    pub fn new() -> Self {
        Self {
            policy: Box::new(AllowList::read_only()),
            raster: Arc::new(NoRaster),
            limits: AgentLimits::default(),
            journal: Vec::new(),
            seq: 0,
            declared: Vec::new(),
            clock: 0.0,
            grants: Vec::new(),
        }
    }

    pub fn with_policy(mut self, policy: Box<dyn PermissionPolicy>) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_raster(mut self, raster: Arc<dyn PageRaster>) -> Self {
        self.raster = raster;
        self
    }

    pub fn with_limits(mut self, limits: AgentLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn policy(&self) -> &dyn PermissionPolicy {
        self.policy.as_ref()
    }

    pub fn set_policy(&mut self, policy: Box<dyn PermissionPolicy>) {
        self.policy = policy;
    }

    pub fn raster(&self) -> &Arc<dyn PageRaster> {
        &self.raster
    }

    pub fn set_raster(&mut self, raster: Arc<dyn PageRaster>) {
        self.raster = raster;
    }

    pub fn limits(&self) -> &AgentLimits {
        &self.limits
    }

    /// Verbs a script declared. Bounded, and deduplicated: a script that
    /// declares in a loop must not grow the session without limit.
    pub fn declare(&mut self, verbs: &[String]) {
        for verb in verbs {
            if self.declared.len() < 64 && !self.declared.contains(verb) {
                self.declared.push(verb.clone());
            }
        }
    }

    pub fn journal(&self) -> &[JournalRow] {
        &self.journal
    }

    pub fn declared(&self) -> &[String] {
        &self.declared
    }

    /// The verbs this host has been told "yes" for, plus `"*"` when the host
    /// approved everything. Consulted before the policy in
    /// [`Self::permit_and_run`].
    pub fn grants(&self) -> &[String] {
        &self.grants
    }

    /// Record the user's approval for one verb name (or `"*"`). Idempotent.
    pub fn grant(&mut self, verb: &str) {
        let verb = verb.trim();
        if verb.is_empty() || self.grants.iter().any(|g| g == verb) {
            return;
        }
        if self.grants.len() < 64 {
            self.grants.push(verb.to_string());
        }
    }

    /// Forget every grant — what a "reset this session's permissions" button
    /// and a navigation to a new origin both do.
    pub fn revoke_grants(&mut self) {
        self.grants.clear();
    }

    /// What this session currently allows: the host's grants plus the verbs
    /// scripts declared. Read by the app's panel; the transports ignore it.
    pub fn policy_json(&self) -> Value {
        let list =
            |items: &[String]| Value::arr(items.iter().map(|v| Value::s(v.clone())).collect());
        Value::obj(vec![
            ("grants", list(&self.grants)),
            ("declared", list(&self.declared)),
        ])
    }

    fn granted(&self, verb: &Verb) -> bool {
        self.grants.iter().any(|g| g == "*" || g == verb.name())
    }
    // ── acting ────────────────────────────────────────────────────────────

    /// The one call every transport makes: consult the policy, then act,
    /// journaling either outcome. Denials are journaled too — "the agent
    /// asked and was refused" is what an activity panel must show, and an
    /// audit that only records successes is not an audit.
    pub fn permit_and_run(&mut self, browser: &mut Browser, verb: &Verb) -> Result<String, String> {
        let started = Instant::now();
        // A grant outranks the policy: the policy is the session's standing
        // rule, a grant is the user's answer to it — "yes, this one, now".
        // Only a host that *asks* ever records one (the app's prompt), so no
        // headless policy can be loosened from here by accident.
        if self.granted(verb) {
            let outcome = self.act(browser, verb);
            let line = match &outcome {
                Ok(result) => summarize(result),
                Err(message) => format!("error: {message}"),
            };
            self.record(verb, None, line, started.elapsed());
            return outcome;
        }
        match self.policy.check(verb) {
            Decision::Deny(reason) => {
                self.record(
                    verb,
                    Some(reason.clone()),
                    format!("denied: {reason}"),
                    started.elapsed(),
                );
                Err(format!("{PERMISSION_PREFIX}`{}`: {reason}", verb.name()))
            }
            Decision::Allow => {
                let outcome = self.act(browser, verb);
                let line = match &outcome {
                    Ok(result) => summarize(result),
                    Err(message) => format!("error: {message}"),
                };
                self.record(verb, None, line, started.elapsed());
                outcome
            }
        }
    }

    /// Act *without* the policy — the path the JS surface takes when it has
    /// already been handed a `Decision::Allow` by [`AgentApi::permit`], and
    /// the path tests take to exercise a verb's mechanics directly.
    pub fn act_unchecked(&mut self, browser: &mut Browser, verb: &Verb) -> Result<String, String> {
        let started = Instant::now();
        let outcome = self.act(browser, verb);
        let line = match &outcome {
            Ok(result) => summarize(result),
            Err(message) => format!("error: {message}"),
        };
        self.record(verb, None, line, started.elapsed());
        outcome
    }

    fn record(&mut self, verb: &Verb, denied: Option<String>, outcome: String, elapsed: Duration) {
        self.seq += 1;
        let row = JournalRow {
            seq: self.seq,
            verb: verb.to_json().to_json(),
            denied,
            outcome,
            elapsed_ms: elapsed.as_millis() as u64,
        };
        if self.journal.len() >= self.limits.journal_max {
            self.journal.remove(0);
        }
        self.journal.push(row);
    }

    /// One act. `Err` becomes a `KitaeError` in a script (and a JSON-RPC
    /// error over the transports).
    fn act(&mut self, browser: &mut Browser, verb: &Verb) -> Result<String, String> {
        match verb {
            Verb::Navigate { url } => self.navigate(browser, url),
            Verb::Click { target } => self.click(browser, target),
            Verb::Fill { target, text } => self.fill(browser, target, text),
            Verb::Submit { form } => self.submit(browser, form.as_deref()),
            Verb::Back => self.back(browser),
            Verb::Read => Ok(self.read(browser)),
            Verb::Links => Ok(self.links(browser)),
            Verb::Find { query } => Ok(self.find(browser, query)),
            Verb::Extract => Ok(extract::document_json(browser).to_json()),
            Verb::Watch {
                selector,
                timeout_ms,
            } => self.watch(browser, selector, *timeout_ms),
            Verb::Dispatch {
                target,
                event,
                detail,
            } => self.dispatch(browser, target, event, detail.as_deref()),
            Verb::Screenshot { path } => self.screenshot(browser, path.as_deref()),
            Verb::Pdf { path } => self.pdf(browser, path.as_deref()),
            Verb::Archive { dir } => self.archive(browser, dir.as_deref()),
            Verb::Verify { path } => self.verify(path),
        }
    }

    fn navigate(&mut self, browser: &mut Browser, url: &str) -> Result<String, String> {
        if url.trim().is_empty() {
            return Err("navigate needs a URL".to_string());
        }
        browser.navigate(url);
        self.settle(browser, self.limits.settle_ms);
        Ok(self.page_summary(browser).to_json())
    }

    fn click(&mut self, browser: &mut Browser, target: &Target) -> Result<String, String> {
        let (point, node) = self.resolve_target(browser, target)?;
        let before = browser.page_url.clone();
        browser.click(point.0, point.1);
        self.settle(browser, self.limits.act_settle_ms);
        let mut pairs = vec![
            ("clicked", target.to_json()),
            ("node", Value::Num(node as f64)),
            ("x", Value::Num(round2(point.0))),
            ("y", Value::Num(round2(point.1))),
            ("url", Value::s(browser.page_url.clone())),
            ("navigated", Value::Bool(browser.page_url != before)),
        ];
        if let Some(title) = self.title_json(browser) {
            pairs.push(("title", title));
        }
        Ok(Value::obj(pairs).to_json())
    }

    fn fill(
        &mut self,
        browser: &mut Browser,
        target: &Target,
        text: &str,
    ) -> Result<String, String> {
        let (_, node) = self.resolve_target(browser, target)?;
        let tag = browser
            .page_dom()
            .map(|dom| dom.tag(kitae_dom::NodeId(node)).to_string())
            .unwrap_or_default();
        browser.set_focus(kitae_dom::NodeId(node));
        if !browser.set_control_value_for_node(kitae_dom::NodeId(node), text) {
            return Err(format!(
                "`{}` ({tag}) is not a fillable control",
                target.describe()
            ));
        }
        Ok(Value::obj(vec![
            ("filled", target.to_json()),
            ("node", Value::Num(node as f64)),
            ("tag", Value::s(tag)),
            ("value", Value::s(text.to_string())),
        ])
        .to_json())
    }

    fn submit(&mut self, browser: &mut Browser, form: Option<&str>) -> Result<String, String> {
        let (node, described) = match form {
            Some(selector) => {
                let node = self.resolve_one(browser, selector)?;
                (node, selector.to_string())
            }
            None => {
                // The form owning the focused control, else the first form.
                let focused = browser.focused().map(|n| n.0);
                let node = focused
                    .and_then(|f| self.form_ancestor(browser, kitae_dom::NodeId(f)))
                    .or_else(|| self.first_form(browser));
                let Some(node) = node else {
                    return Err(
                        "no form to submit (focus a control or pass a selector)".to_string()
                    );
                };
                (node, "the focused form".to_string())
            }
        };
        // Constraint validation is read *before* submitting: a successful
        // submit navigates, which replaces the document and makes this node
        // id meaningless (node ids are per-document arena indices).
        let failures = self.validation_failures(browser, node);
        let before = browser.page_url.clone();
        let target = browser.submit_form_for_node(kitae_dom::NodeId(node));
        self.settle(browser, self.limits.act_settle_ms);
        let navigated = browser.page_url != before;
        let mut pairs = vec![
            ("submitted", Value::s(described)),
            ("node", Value::Num(node as f64)),
            ("url", Value::s(browser.page_url.clone())),
            ("navigated", Value::Bool(navigated)),
        ];
        if let Some(target) = target {
            pairs.push(("target", Value::s(target)));
        }
        // Reported only when nothing navigated — that is when a failure
        // means the submit was blocked.
        if !navigated {
            if let Some(failures) = failures {
                pairs.push(("validationFailed", Value::Bool(true)));
                pairs.push(("invalidControls", Value::arr(failures)));
            }
        }
        Ok(Value::obj(pairs).to_json())
    }

    fn back(&mut self, browser: &mut Browser) -> Result<String, String> {
        let before = browser.page_url.clone();
        browser.go(-1);
        self.settle(browser, self.limits.settle_ms);
        Ok(Value::obj(vec![
            ("from", Value::s(before.clone())),
            ("url", Value::s(browser.page_url.clone())),
            ("moved", Value::Bool(browser.page_url != before)),
            ("canGoBack", Value::Bool(browser.state().can_go_back)),
        ])
        .to_json())
    }

    fn read(&self, browser: &Browser) -> String {
        let text = browser.page_text();
        let mut pairs = vec![
            ("url", Value::s(browser.page_url.clone())),
            ("title", Value::s(browser.page_title.clone())),
        ];
        let mut body = text;
        let capped = body.chars().count() > self.limits.max_text_chars;
        if capped {
            body = body.chars().take(self.limits.max_text_chars).collect();
        }
        pairs.push(("text", Value::s(body.clone())));
        pairs.push(("words", Value::Num(count_words(&body) as f64)));
        if capped {
            pairs.push(("truncated", Value::Bool(true)));
        }
        Value::obj(pairs).to_json()
    }

    fn links(&self, browser: &Browser) -> String {
        let node_text = |node: u32| -> String {
            browser
                .page_dom()
                .map(|dom| {
                    let mut out = String::new();
                    dom.walk(kitae_dom::NodeId(node), &mut |n| {
                        if matches!(dom.node(n).kind, kitae_dom::NodeKind::Text) {
                            out.push_str(&dom.node(n).text);
                            out.push(' ');
                        }
                    });
                    out.split_whitespace().collect::<Vec<_>>().join(" ")
                })
                .unwrap_or_default()
        };
        // One entry per *anchor*, not per painted run. Two engine facts
        // make that a small join: link areas are emitted per text run (so a
        // two-word anchor arrives twice), and their provenance names the
        // block that laid the text out, not the `<a>` inside it. The
        // anchor is found by href within that block, and its areas' boxes
        // are unioned into the one box an agent should click.
        let mut merged: Vec<(u32, String, (f32, f32, f32, f32))> = Vec::new();
        for link in browser.links() {
            let rect = (link.x, link.y, link.w, link.h);
            let anchor = self
                .anchor_node_for(browser, kitae_dom::NodeId(link.node), &link.href)
                .unwrap_or(link.node);
            match merged.iter_mut().find(|(node, _, _)| *node == anchor) {
                Some((_, _, box2)) => {
                    let x0 = box2.0.min(rect.0);
                    let y0 = box2.1.min(rect.1);
                    let x1 = (box2.0 + box2.2).max(rect.0 + rect.2);
                    let y1 = (box2.1 + box2.3).max(rect.1 + rect.3);
                    *box2 = (x0, y0, x1 - x0, y1 - y0);
                }
                None => merged.push((anchor, link.href.clone(), rect)),
            }
        }
        // The engine stores the *raw* href and resolves it when a click
        // follows it; an agent needs the URL a click would actually load,
        // so resolution happens here against the same base the click path
        // uses.
        let base = browser
            .page
            .as_ref()
            .map(|p| {
                if p.resource_base_url.is_empty() {
                    p.base_url.clone()
                } else {
                    p.resource_base_url.clone()
                }
            })
            .unwrap_or_else(|| browser.page_url.clone());
        let items: Vec<Value> = merged
            .into_iter()
            .map(|(node, href, (x, y, w, h))| {
                let resolved = kitae_net::resolve(&base, &href).unwrap_or_else(|| href.clone());
                Value::obj(vec![
                    ("href", Value::s(resolved)),
                    ("rawHref", Value::s(href)),
                    ("text", Value::s(node_text(node))),
                    ("node", Value::Num(node as f64)),
                    (
                        "rect",
                        Value::arr(vec![
                            Value::Num(round2(x)),
                            Value::Num(round2(y)),
                            Value::Num(round2(w)),
                            Value::Num(round2(h)),
                        ]),
                    ),
                ])
            })
            .collect();
        Value::obj(vec![
            ("url", Value::s(browser.page_url.clone())),
            ("count", Value::Num(items.len() as f64)),
            ("links", Value::arr(items)),
        ])
        .to_json()
    }

    fn find(&self, browser: &mut Browser, query: &str) -> String {
        let matches: Vec<Value> = browser
            .find_in_page(query)
            .iter()
            .map(|m| {
                let rects: Vec<Value> = m
                    .rects
                    .iter()
                    .map(|r| {
                        Value::arr(vec![
                            Value::Num(round2(r.x)),
                            Value::Num(round2(r.y)),
                            Value::Num(round2(r.w)),
                            Value::Num(round2(r.h)),
                        ])
                    })
                    .collect();
                Value::obj(vec![
                    ("index", Value::Num(m.index as f64)),
                    ("rects", Value::arr(rects)),
                ])
            })
            .collect();
        Value::obj(vec![
            ("query", Value::s(query.to_string())),
            ("count", Value::Num(matches.len() as f64)),
            ("matches", Value::arr(matches)),
            (
                "truncated",
                Value::Bool(browser.find_matches().len() >= crate::FIND_MAX_MATCHES),
            ),
        ])
        .to_json()
    }

    /// Wait for a selector to appear — the verb that makes a multi-step task
    /// possible without a sleeping agent. Polls the page's own machinery
    /// (timers, fetches, scripts) between checks, so a selector that only
    /// appears after a `setTimeout` or an XHR still arrives.
    fn watch(
        &mut self,
        browser: &mut Browser,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<String, String> {
        let parsed = kitae_style_query(selector)
            .ok_or_else(|| format!("`{selector}` is not a valid CSS selector"))?;
        let budget = timeout_ms.min(self.limits.watch_ms) + self.limits.watch_slack_ms;
        let deadline = Instant::now() + Duration::from_millis(budget);
        let started = Instant::now();
        let mut node = self.query_first(browser, &parsed);
        while node.is_none() && Instant::now() < deadline {
            self.settle(browser, self.limits.poll_interval_ms);
            std::thread::sleep(Duration::from_millis(self.limits.poll_interval_ms));
            node = self.query_first(browser, &parsed);
        }
        let mut pairs = vec![
            ("selector", Value::s(selector.to_string())),
            ("matched", Value::Bool(node.is_some())),
            (
                "elapsedMs",
                Value::Num(started.elapsed().as_millis() as f64),
            ),
        ];
        if let Some(node) = node {
            pairs.push(("node", Value::Num(node as f64)));
            if let Some(box2) = extract::element_box(browser, kitae_dom::NodeId(node)) {
                pairs.push((
                    "rect",
                    Value::arr(vec![
                        Value::Num(round2(box2.0)),
                        Value::Num(round2(box2.1)),
                        Value::Num(round2(box2.2)),
                        Value::Num(round2(box2.3)),
                    ]),
                ));
            }
        }
        Ok(Value::obj(pairs).to_json())
    }

    fn dispatch(
        &mut self,
        browser: &mut Browser,
        target: &Target,
        event: &str,
        detail: Option<&str>,
    ) -> Result<String, String> {
        if event.is_empty() {
            return Err("dispatch needs an event type".to_string());
        }
        let (node, point) = self.resolve_target_with_node(browser, target)?;
        if !self.page_js_running(browser) {
            return Err(
                "dispatch needs page JavaScript (this session was built without the `page-js` \
                 feature, or the page has no script session)"
                    .to_string(),
            );
        }
        let delivered = self.dispatch_event_on(browser, node, event, detail);
        self.settle(browser, self.limits.act_settle_ms);
        Ok(Value::obj(vec![
            ("dispatched", Value::Bool(delivered)),
            ("event", Value::s(event.to_string())),
            ("node", Value::Num(node as f64)),
            ("x", Value::Num(round2(point.0))),
            ("y", Value::Num(round2(point.1))),
            ("url", Value::s(browser.page_url.clone())),
        ])
        .to_json())
    }

    fn screenshot(&self, browser: &Browser, path: Option<&str>) -> Result<String, String> {
        let png = self.raster.png(browser)?;
        self.binary_result(browser, "png", "image/png", &png, path)
    }

    fn pdf(&self, browser: &Browser, path: Option<&str>) -> Result<String, String> {
        let pdf = self.raster.pdf(browser)?;
        self.binary_result(browser, "pdf", "application/pdf", &pdf, path)
    }

    /// A binary verb's result: written to `path` when given, else carried
    /// inline base64 (the transports that can deliver bytes use it).
    fn binary_result(
        &self,
        browser: &Browser,
        key: &str,
        mime: &str,
        bytes: &[u8],
        path: Option<&str>,
    ) -> Result<String, String> {
        let mut pairs = vec![
            ("mime", Value::s(mime.to_string())),
            ("bytes", Value::Num(bytes.len() as f64)),
            ("url", Value::s(browser.page_url.clone())),
        ];
        match path {
            Some(path) => {
                std::fs::write(path, bytes).map_err(|e| format!("write {path}: {e}"))?;
                pairs.push(("path", Value::s(path.to_string())));
            }
            None => {
                pairs.push((key, Value::s(crate::b64_encode(bytes))));
            }
        }
        Ok(Value::obj(pairs).to_json())
    }

    // ── evidence (B6) ─────────────────────────────────────────────────────

    fn archive(&mut self, browser: &mut Browser, dir: Option<&str>) -> Result<String, String> {
        let manifest = crate::evidence::archive(browser, dir)?;
        Ok(manifest.to_json())
    }

    fn verify(&mut self, path: &str) -> Result<String, String> {
        Ok(crate::evidence::verify(path)?.to_json())
    }

    // ── helpers ───────────────────────────────────────────────────────────

    /// Run the engine's own machinery until it goes quiet or `budget_ms`
    /// elapses: the top-level load, then subresource fetches, page scripts,
    /// downloads and CSS animation ticks. This is what a host's frame loop
    /// does; an agent has no frame loop, so it stands in for one.
    fn settle(&mut self, browser: &mut Browser, budget_ms: u64) {
        let deadline = Instant::now() + Duration::from_millis(budget_ms);
        // 1. The top-level fetch (the pooled/background path).
        while browser.load_in_flight() && Instant::now() < deadline {
            browser.poll_fetches();
            std::thread::sleep(Duration::from_millis(self.limits.poll_interval_ms.min(5)));
        }
        // 2. Everything the page does afterwards. Three consecutive quiet
        //    ticks end the settle: one tick is not enough, because a fetch
        //    that finishes on tick N can queue script work for tick N+1.
        let mut quiet = 0;
        while Instant::now() < deadline && quiet < 3 {
            let mut worked = browser.poll_fetches();
            worked |= browser.poll_page_js();
            worked |= browser.poll_downloads() > 0;
            self.clock += 0.05;
            worked |= browser.tick_transitions_at(self.clock);
            if worked {
                quiet = 0;
            } else {
                quiet += 1;
                std::thread::sleep(Duration::from_millis(self.limits.poll_interval_ms.min(5)));
            }
        }
    }

    fn page_summary(&self, browser: &Browser) -> Value {
        let mut pairs = vec![
            ("url", Value::s(browser.page_url.clone())),
            ("title", Value::s(browser.page_title.clone())),
            (
                "words",
                Value::Num(count_words(&browser.page_text()) as f64),
            ),
            ("loading", Value::Bool(browser.load_in_flight())),
        ];
        if let Some(err) = browser.last_error() {
            pairs.push(("error", Value::s(err.to_string())));
        }
        Value::obj(pairs)
    }

    fn title_json(&self, browser: &Browser) -> Option<Value> {
        (!browser.page_title.is_empty()).then(|| Value::s(browser.page_title.clone()))
    }

    fn query_first(&self, browser: &Browser, selector: &kitae_css::Selector) -> Option<u32> {
        let dom = browser.page_dom()?;
        let state = browser.interaction_state();
        kitae_style::query::all(dom, selector, &state)
            .first()
            .map(|n| n.0)
    }

    fn resolve_one(&self, browser: &Browser, selector: &str) -> Result<u32, String> {
        let parsed = kitae_style_query(selector)
            .ok_or_else(|| format!("`{selector}` is not a valid CSS selector"))?;
        let dom = browser
            .page_dom()
            .ok_or_else(|| "no page is open".to_string())?;
        let state = browser.interaction_state();
        let matches = kitae_style::query::all(dom, &parsed, &state);
        let Some(node) = matches.first() else {
            return Err(format!("nothing on this page matches `{selector}`"));
        };
        Ok(node.0)
    }

    /// A target's click point (page coordinates) and its node.
    fn resolve_target(
        &self,
        browser: &Browser,
        target: &Target,
    ) -> Result<((f32, f32), u32), String> {
        match target {
            Target::Point { x, y } => {
                let node = browser.node_at_point(*x, *y).map(|n| n.0).unwrap_or(0);
                Ok(((*x, *y), node))
            }
            Target::Selector(selector) => {
                let node = self.resolve_one(browser, selector)?;
                let box2 =
                    extract::element_box(browser, kitae_dom::NodeId(node)).ok_or_else(|| {
                        format!("`{selector}` matched a node with no layout box on the last frame")
                    })?;
                Ok((center(box2), node))
            }
        }
    }

    /// Like [`Self::resolve_target`] but also hands back the node for a
    /// point target (`0` when a point hits nothing).
    fn resolve_target_with_node(
        &self,
        browser: &Browser,
        target: &Target,
    ) -> Result<(u32, (f32, f32)), String> {
        let (point, node) = self.resolve_target(browser, target)?;
        Ok((node, point))
    }

    /// The `<a>` carrying `href` at or under `from` — the join that turns
    /// the layout's block-provenance link areas back into anchors. Falls
    /// back to a document-wide search, then to `from` itself.
    fn anchor_node_for(
        &self,
        browser: &Browser,
        from: kitae_dom::NodeId,
        href: &str,
    ) -> Option<u32> {
        let dom = browser.page_dom()?;
        let matches = |n: kitae_dom::NodeId| {
            dom.tag(n) == "a"
                && dom
                    .element(n)
                    .and_then(|e| e.attr("href"))
                    .map(|h| h == href)
                    .unwrap_or(false)
        };
        let mut found = None;
        dom.walk(from, &mut |n| {
            if found.is_none() && matches(n) {
                found = Some(n.0);
            }
        });
        if found.is_some() {
            return found;
        }
        if let Some(root) = dom.root() {
            dom.walk(root, &mut |n| {
                if found.is_none() && matches(n) {
                    found = Some(n.0);
                }
            });
        }
        found
    }

    /// The nearest `<form>` ancestor of `node`, including `node` itself.
    fn form_ancestor(&self, browser: &Browser, node: kitae_dom::NodeId) -> Option<u32> {
        let dom = browser.page_dom()?;
        let mut cur = Some(node);
        while let Some(n) = cur {
            if dom.tag(n) == "form" {
                return Some(n.0);
            }
            cur = dom.node(n).parent;
        }
        None
    }

    fn first_form(&self, browser: &Browser) -> Option<u32> {
        let dom = browser.page_dom()?;
        let root = dom.root()?;
        let mut found = None;
        dom.walk(root, &mut |n| {
            if found.is_none() && dom.tag(n) == "form" {
                found = Some(n.0);
            }
        });
        found
    }

    /// Whether a node id belongs to the *current* document. Node ids are
    /// per-document arena indices, so an id captured before a navigation must
    /// never be used after one.
    fn node_alive(&self, browser: &Browser, node: u32) -> bool {
        browser
            .page_dom()
            .map(|dom| (node as usize) < dom.len())
            .unwrap_or(false)
    }

    fn validation_failures(&self, browser: &Browser, node: u32) -> Option<Vec<Value>> {
        if !self.node_alive(browser, node) {
            return None;
        }
        let failures = browser.form_failures_for_node(kitae_dom::NodeId(node));
        (!failures.is_empty()).then(|| {
            failures
                .iter()
                .map(|f| Value::Num(f.0 as f64))
                .collect::<Vec<_>>()
        })
    }

    /// Whether a page-script session is live (see
    /// [`Browser::page_js_running`]).
    fn page_js_running(&self, browser: &Browser) -> bool {
        browser.page_js_running()
    }

    /// Dispatch a DOM event at a node through the page's script session.
    /// Without the `page-js` feature there is no session to dispatch into,
    /// and the caller has already refused the verb.
    #[cfg(feature = "page-js")]
    fn dispatch_event_on(
        &mut self,
        browser: &mut Browser,
        node: u32,
        event: &str,
        detail: Option<&str>,
    ) -> bool {
        browser.dispatch_js_event_at(event, kitae_dom::NodeId(node), detail)
    }

    #[cfg(not(feature = "page-js"))]
    fn dispatch_event_on(
        &mut self,
        _browser: &mut Browser,
        _node: u32,
        _event: &str,
        _detail: Option<&str>,
    ) -> bool {
        false
    }
}

/// One verb, from its wire JSON to the reply document a host reads.
///
/// The shape matches the JS surface (`kitae_script::agent`): `{ok, verb,
/// result}` on success, and on failure `{ok:false, verb, kind, message}` with
/// `kind` one of `"argument"`, `"act"` or `"permission"`. A `"permission"`
/// reply is the one a host with a *user* must handle: it also carries
/// `"pending": true`, and the host answers by granting the verb
/// ([`SessionState::grant`]) and calling this again with the same request.
/// Nothing is lost meanwhile — the refusal is already a journal row, so the
/// activity panel shows the ask, the prompt and the outcome.
pub fn run_wire(state: &mut SessionState, browser: &mut Browser, request: &str) -> String {
    let parsed = match kitae_json::parse(request) {
        Ok(value) => value,
        Err(message) => {
            return wire_error(
                "",
                "argument",
                format!("bad request JSON: {message}"),
                false,
            )
        }
    };
    let verb = match Verb::from_json(&parsed) {
        Ok(verb) => verb,
        Err(message) => return wire_error("", "argument", message, false),
    };
    let name = verb.name().to_string();
    match state.permit_and_run(browser, &verb) {
        Ok(result) => {
            let result = kitae_json::parse(&result).unwrap_or(Value::Str(result));
            Value::obj(vec![
                ("ok", Value::Bool(true)),
                ("verb", Value::s(name)),
                ("result", result),
            ])
            .to_json()
        }
        Err(message) => {
            if let Some(rest) = message.strip_prefix(PERMISSION_PREFIX) {
                return wire_error(&name, "permission", rest.to_string(), true);
            }
            wire_error(&name, "act", message, false)
        }
    }
}

/// The journal as wire JSON, oldest first: one object per row. Each row
/// carries the verb's *name* (parsed out of the audit text kept for replay)
/// so a panel can label an entry without re-parsing a request.
pub fn journal_json(rows: &[JournalRow]) -> Value {
    let items = rows
        .iter()
        .map(|row| {
            let name = kitae_json::parse(&row.verb)
                .ok()
                .and_then(|value| {
                    value
                        .get("verb")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| row.verb.clone());
            Value::obj(vec![
                ("seq", Value::Num(row.seq as f64)),
                ("verb", Value::s(name)),
                ("request", Value::s(row.verb.clone())),
                (
                    "denied",
                    row.denied.clone().map(Value::Str).unwrap_or(Value::Null),
                ),
                ("outcome", Value::s(row.outcome.clone())),
                ("elapsedMs", Value::Num(row.elapsed_ms as f64)),
            ])
        })
        .collect();
    Value::arr(items)
}

fn wire_error(verb: &str, kind: &str, message: String, pending: bool) -> String {
    Value::obj(vec![
        ("ok", Value::Bool(false)),
        ("verb", Value::s(verb)),
        ("kind", Value::s(kind)),
        ("message", Value::s(message)),
        ("pending", Value::Bool(pending)),
    ])
    .to_json()
}

/// A script-facing [`AgentApi`] over a shared session.
///
/// A `kitae_script` runtime holds its API as `Arc<dyn AgentApi>` and calls
/// it with `&self` (the runtime outlives individual calls and the verb
/// needs `&mut Browser`), so a session that drives a browser is shared
/// behind a lock and wrapped here. This is what lets an *automation script*
/// — the Track-A scripting surface, `kitae.navigate(...)` in a user's own
/// file — act on a real page with the same verbs an MCP client uses.
pub struct SharedSession(std::rc::Rc<std::cell::RefCell<AgentSession>>);

impl SharedSession {
    pub fn new(session: AgentSession) -> Self {
        Self(std::rc::Rc::new(std::cell::RefCell::new(session)))
    }

    pub fn handle(&self) -> std::rc::Rc<std::cell::RefCell<AgentSession>> {
        self.0.clone()
    }
}

impl AgentApi for SharedSession {
    fn act(&self, verb: &Verb) -> Result<String, String> {
        // Re-entrancy guard: a script that calls a verb from inside another
        // verb's result handling would find the session borrowed. Say so
        // instead of panicking.
        let mut session = self
            .0
            .try_borrow_mut()
            .map_err(|_| "the agent session is busy (a verb is already running)".to_string())?;
        session.act_unchecked(verb)
    }

    fn permit(&self, verb: &Verb) -> Decision {
        match self.0.try_borrow() {
            Ok(session) => session.state.policy.check(verb),
            Err(_) => Decision::Deny("the agent session is busy".to_string()),
        }
    }

    fn declare(&self, verbs: &[String]) {
        if let Ok(mut session) = self.0.try_borrow_mut() {
            session.state.declare(verbs);
        }
    }
}

/// The `PermissionPolicy` a session installs when the host asks for one
/// that answers through a channel (the app's prompt): the policy blocks
/// until the UI answers or the deadline passes, and a timeout **denies** —
/// a prompt nobody answered must never become an allow.
pub struct PromptPolicy {
    prompt: Box<dyn Fn(&Verb) -> Option<bool>>,
    timeout: Duration,
}

impl PromptPolicy {
    pub fn new(prompt: Box<dyn Fn(&Verb) -> Option<bool>>) -> Self {
        Self {
            prompt,
            timeout: Duration::from_secs(120),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl PermissionPolicy for PromptPolicy {
    fn check(&self, verb: &Verb) -> Decision {
        let started = Instant::now();
        loop {
            match (self.prompt)(verb) {
                Some(true) => return Decision::Allow,
                Some(false) => return Decision::Deny("the user declined this action".to_string()),
                None => {
                    if started.elapsed() >= self.timeout {
                        return Decision::Deny(format!(
                            "no answer to the permission prompt for `{}` within {}s",
                            verb.name(),
                            self.timeout.as_secs()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
}

/// Parse a query selector once, for callers that resolve many nodes.
pub fn kitae_style_query(text: &str) -> Option<kitae_css::Selector> {
    kitae_style::query::parse(text)
}

fn center(box2: extract::Box2) -> (f32, f32) {
    (box2.0 + box2.2 / 2.0, box2.1 + box2.3 / 2.0)
}

fn round2(v: f32) -> f64 {
    (v * 100.0).round() as f64 / 100.0
}

fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}

/// A one-line summary of a result, for the journal: the first scalar field
/// that names what happened.
fn summarize(result: &str) -> String {
    let Ok(value) = kitae_json::parse(result) else {
        return "ok".to_string();
    };
    match value.get("title").and_then(Value::as_str) {
        Some(title) if !title.is_empty() => return format!("ok — {title}"),
        _ => {}
    }
    if let Some(url) = value.get("url").and_then(Value::as_str) {
        if !url.is_empty() {
            return format!("ok — {url}");
        }
    }
    if let Some(path) = value.get("path").and_then(Value::as_str) {
        return format!("ok — {path}");
    }
    if let Some(n) = value.get("count").and_then(Value::as_f64) {
        return format!("ok — {n} matches");
    }
    "ok".to_string()
}
