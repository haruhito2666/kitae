//! kitae_core — the embeddable Kitae engine.
//!
//! Everything that makes Kitae a browser (net, HTML, CSS, style, layout, the
//! JS-content policy, history, synthetic pages) lives here, host-agnostic.
//! A host provides text measurement (real fonts on macOS via Core Text, an
//! approximation in tests) and receives finished frames through [`Host`].
//!
//! The closed-API script runtime ([`kitae_script`]) drives only Kitae's own
//! automation surfaces. Page JavaScript is a separate, opt-in tier (the
//! `page-js` feature plus a per-engine enable): when on, a site's content
//! scripts run under fixed time/memory budgets; when a scripted page cannot
//! be rendered that way it still gets the noscript fallback or an
//! interstitial with an escape hatch to the system browser. The page's
//! window and its event loop live in that tier ([`pagejs`]) and are drained
//! by the host's idle tick; hosts own threading and I/O.

// B1-B4: the agent plane's host — a real `Browser` behind the verbs.
pub mod agent;
mod embedder;
// B6: signed QTSQ archives as evidence.
pub mod evidence;
// B5: page-as-data extraction — the agent's input layer.
pub mod extract;
mod ffi;
#[cfg(feature = "live")]
pub mod live;
pub mod markdown;
mod media_attrs;
#[cfg(feature = "page-js")]
pub mod pagejs;
pub mod start_page;
pub mod ui;

pub use embedder::{Engine, EngineBuilder, EngineError, EngineEvent, RenderedFrame};

use kitae_dom::{Dom, NodeId, NodeKind};
use kitae_html::parse;
use kitae_layout::{
    layout_cached_in_env, layout_in_env, DisplayList, FragmentCache, LinkArea, TextMeasurer,
    TextRun,
};
use kitae_media::{decode_image, decode_svg, sniff};
use kitae_net::{fetch_cached, fetch_with, normalize_input, resolve, FetchError, Response};
use kitae_style::{
    apply_style_value, interpolate_style_value, serialize_style_value, transition_matches,
    BackgroundPaint, ComputedStyle, ContainerInfo, EnvironmentState, MediaType, StyleEngine,
};
use kitae_ui::patch::PatchOp;
use kitae_ui::vnode::View;
use media_attrs::{media_attrs, media_candidates, MediaAttrs};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
/// The address the browser opens when no start URL was given.
pub const START_URL: &str = "about:start";

/// What a host sees after every engine state change.
#[derive(Clone, Debug)]
pub struct PublicState {
    /// Resolved URL of the current page.
    pub url: String,
    pub resource_base_url: String,
    /// Page title, whitespace-collapsed (no "— Kitae" suffix; hosts style it).
    pub title: String,
    /// History navigation availability, for enabling back/forward chrome.
    pub can_go_back: bool,
    pub can_go_forward: bool,
    /// Current scroll offset and total document height, in page space
    /// (scrollbar + click translation). Hosts that scroll their view locally
    /// may ignore these.
    pub scroll_y: f32,
    pub content_height: f32,
    /// Focused element (slice H): element `id` when present, else the tag
    /// name (`"input"`, `"a"`, …). `None` when nothing is focused.
    pub focused_id: Option<String>,
    /// Current selection text (slice H): `None` when nothing is selected.
    pub selection: Option<String>,
    /// Media attached to the current page (slice J4): `Some` when the page
    /// is an audio/video QTSQ viewer, carrying everything the embedder's
    /// player needs plus the decode handoff path.
    pub media: Option<MediaInfo>,
    /// Find-in-page snapshot (Phase P1): current matches in page order and
    /// the active index (`None` when the query is empty or matched nothing).
    /// Recomputed on every layout; frames carry the same data (v8).
    pub find_matches: Vec<FindMatch>,
    pub find_active: Option<usize>,
    /// Accessibility tree (Phase P2): Document-rooted nodes with layout
    /// bounds; `ax_focused` is the focused node's AX id, if any. Frames
    /// carry the same data as a side-table (v8).
    pub ax_tree: Vec<AxNode>,
    pub ax_focused: Option<String>,
    /// True when the current page is a QTSQ that failed to decode *because
    /// it is encrypted* (no password known, or a supplied one was wrong).
    /// Frames carry the same flag (v9) so hosts can prompt for a password.
    pub qtsq_locked: bool,
    /// Signature state of the current page's QTSQ payload (slice ④): the
    /// banner and hosts render from this. `Unsigned` everywhere that isn't a
    /// signed .qtsq viewer page. Frames carry the same state (v11).
    pub qtsq_sig: QtsqSigState,
    /// True while the page is laid out for print media (H3): `@media print`
    /// applies, `@media screen` does not. Frames carry it as `flags` bit2 so
    /// a host can tell a print-styled frame from a screen one.
    pub print_media: bool,
}

/// Verification verdict for a .qtsq viewer page's embedded ML-DSA-65
/// signature, computed at page load against this machine's device sign key
/// (files signed elsewhere — or with a stale key — report `Invalid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QtsqSigState {
    /// Not a signed QTSQ payload (covers every non-QTSQ page too).
    #[default]
    Unsigned,
    /// Signed and verified against the device sign key.
    Valid,
    /// Signed but the verification failed against the device sign key —
    /// modified after signing, or signed by another device.
    Invalid,
    /// Signed but this machine has no device sign key to check against.
    Unverified,
}

impl QtsqSigState {
    /// Wire byte for the C-ABI frame (v11).
    pub fn wire_id(self) -> u8 {
        match self {
            Self::Unsigned => 0,
            Self::Valid => 1,
            Self::Invalid => 2,
            Self::Unverified => 3,
        }
    }
}

/// What kind of media a page carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Audio,
    Video,
}

/// Media state published with frames (slice J4). For audio, `caf_path` is
/// the engine-decoded temp CAF the embedder plays with the system player
/// (the engine decodes; the embedder plays). For video it is empty — frames
/// arrive as display-list images.
#[derive(Clone, Debug)]
pub struct MediaInfo {
    /// Absolute URL of the media document.
    pub url: String,
    pub kind: MediaKind,
    /// Duration in seconds (audio: decoded length; video: frame_count/fps).
    pub duration: f32,
    /// Sample rate in Hz (audio), 0 for video.
    pub rate: u32,
    /// Channel count (audio), 0 for video.
    pub channels: u8,
    /// Transport mirror: whether the embedder's player should be playing.
    pub playing: bool,
    /// Transport mirror: position in seconds.
    pub position: f32,
    /// Temp CAF path for audio handoff ("" for video).
    pub caf_path: String,
}

// ---------------------------------------------------------------------------
// Find-in-page (Phase P1) + accessibility tree (Phase P2)
// ---------------------------------------------------------------------------

/// One find highlight box in page coordinates (top-left origin).
#[derive(Clone, Debug, PartialEq)]
pub struct FindRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// One query occurrence in page order: one rect per display-list text run
/// the match touches (matches spanning run boundaries carry several), and
/// the page ordinal. The embedder overlays these rects; the frame's active
/// index gets the strong highlight.
#[derive(Clone, Debug, PartialEq)]
pub struct FindMatch {
    pub rects: Vec<FindRect>,
    pub index: usize,
}

/// Cap on reported find matches: pathological pages still publish bounded
/// frames (matches saturate; the count the frame carries is the truth).
pub const FIND_MAX_MATCHES: usize = 1000;

/// Cap on per-tab navigation history entries (M-4, SECURITY_AUDIT.md):
/// beyond this, `push_history` evicts the oldest entries from the front so a
/// hyperactive tab cannot grow history memory monotonically. 500 matches the
/// usual desktop-browser scale; back/forward semantics are relative to the
/// current position, so the cap is invisible in use.
pub const HISTORY_MAX: usize = 500;

/// Accessibility role per element (Phase P2): the DOM tag maps to one of
/// these; anything else is a generic container. H4 adds LiveRegion — an
/// `aria-live` element of any base tag (the live role takes over, so hosts
/// can diff its label across frames and announce updates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxRole {
    Document,
    Heading,
    Link,
    Button,
    Checkbox,
    Textbox,
    ComboBox,
    Image,
    Table,
    Text,
    Group,
    LiveRegion,
    RadioButton,
    TextArea,
    Slider,
    SpinButton,
    ProgressIndicator,
    ColorWell,
    List,
    ListItem,
    Navigation,
    Main,
    Banner,
    ContentInfo,
    Complementary,
    Search,
    Status,
    Alert,
    Article,
    Form,
    Separator,
    Switch,
    RadioGroup,
    Toolbar,
    Row,
    Cell,
    ColumnHeader,
    RowHeader,
    Dialog,
    DisclosureTriangle,
    Term,
    Definition,
    Timer,
    ScrollBar,
}

impl AxRole {
    /// Stable wire id for the FFI side-table (v8) and Swift bridging.
    pub fn wire_id(self) -> u8 {
        match self {
            AxRole::Document => 0,
            AxRole::Heading => 1,
            AxRole::Link => 2,
            AxRole::Button => 3,
            AxRole::Checkbox => 4,
            AxRole::Textbox => 5,
            AxRole::ComboBox => 6,
            AxRole::Image => 7,
            AxRole::Table => 8,
            AxRole::Text => 9,
            AxRole::Group => 10,
            AxRole::LiveRegion => 11,
            AxRole::RadioButton => 12,
            AxRole::TextArea => 13,
            AxRole::Slider => 14,
            AxRole::SpinButton => 15,
            AxRole::ProgressIndicator => 16,
            AxRole::ColorWell => 17,
            AxRole::List => 18,
            AxRole::ListItem => 19,
            AxRole::Navigation => 20,
            AxRole::Main => 21,
            AxRole::Banner => 22,
            AxRole::ContentInfo => 23,
            AxRole::Complementary => 24,
            AxRole::Search => 25,
            AxRole::Status => 26,
            AxRole::Alert => 27,
            AxRole::Article => 28,
            AxRole::Form => 29,
            AxRole::Separator => 30,
            AxRole::Switch => 31,
            AxRole::RadioGroup => 32,
            AxRole::Toolbar => 33,
            AxRole::Row => 34,
            AxRole::Cell => 35,
            AxRole::ColumnHeader => 36,
            AxRole::RowHeader => 37,
            AxRole::Dialog => 38,
            AxRole::DisclosureTriangle => 39,
            AxRole::Term => 40,
            AxRole::Definition => 41,
            AxRole::Timer => 42,
            AxRole::ScrollBar => 43,
        }
    }

    pub fn from_wire_id(id: u8) -> AxRole {
        match id {
            0 => AxRole::Document,
            1 => AxRole::Heading,
            2 => AxRole::Link,
            3 => AxRole::Button,
            4 => AxRole::Checkbox,
            5 => AxRole::Textbox,
            6 => AxRole::ComboBox,
            7 => AxRole::Image,
            8 => AxRole::Table,
            9 => AxRole::Text,
            11 => AxRole::LiveRegion,
            12 => AxRole::RadioButton,
            13 => AxRole::TextArea,
            14 => AxRole::Slider,
            15 => AxRole::SpinButton,
            16 => AxRole::ProgressIndicator,
            17 => AxRole::ColorWell,
            18 => AxRole::List,
            19 => AxRole::ListItem,
            20 => AxRole::Navigation,
            21 => AxRole::Main,
            22 => AxRole::Banner,
            23 => AxRole::ContentInfo,
            24 => AxRole::Complementary,
            25 => AxRole::Search,
            26 => AxRole::Status,
            27 => AxRole::Alert,
            28 => AxRole::Article,
            29 => AxRole::Form,
            30 => AxRole::Separator,
            31 => AxRole::Switch,
            32 => AxRole::RadioGroup,
            33 => AxRole::Toolbar,
            34 => AxRole::Row,
            35 => AxRole::Cell,
            36 => AxRole::ColumnHeader,
            37 => AxRole::RowHeader,
            38 => AxRole::Dialog,
            39 => AxRole::DisclosureTriangle,
            40 => AxRole::Term,
            41 => AxRole::Definition,
            42 => AxRole::Timer,
            43 => AxRole::ScrollBar,
            _ => AxRole::Group,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AxChecked {
    #[default]
    Unspecified,
    Unchecked,
    Checked,
    Mixed,
}

impl AxChecked {
    pub fn wire_id(self) -> u8 {
        match self {
            AxChecked::Unspecified => 0,
            AxChecked::Unchecked => 1,
            AxChecked::Checked => 2,
            AxChecked::Mixed => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AxLivePoliteness {
    #[default]
    Off,
    Polite,
    Assertive,
}

impl AxLivePoliteness {
    pub fn wire_id(self) -> u8 {
        match self {
            AxLivePoliteness::Off => 0,
            AxLivePoliteness::Polite => 1,
            AxLivePoliteness::Assertive => 2,
        }
    }
}

/// One accessibility node: role, human label and layout bounds in page
/// coordinates (a zero rect means "no geometry resolved" — v1 limitation
/// for label-less form controls; see [`Browser::accessibility_tree`]).
#[derive(Clone, Debug, PartialEq)]
pub struct AxNode {
    /// Element `id` when present, else a stable generated `ax{N}`.
    pub id: String,
    pub role: AxRole,
    pub label: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub value: Option<String>,
    pub value_min: Option<f32>,
    pub value_max: Option<f32>,
    pub disabled: bool,
    pub checked: AxChecked,
    pub expanded: Option<bool>,
    pub selected: Option<bool>,
    pub heading_level: u8,
    pub live_politeness: AxLivePoliteness,
    pub children: Vec<AxNode>,
}

impl AxNode {
    pub fn has_ax_semantics(&self) -> bool {
        self.value.is_some()
            || self.value_min.is_some()
            || self.value_max.is_some()
            || self.disabled
            || self.checked != AxChecked::Unspecified
            || self.expanded.is_some()
            || self.selected.is_some()
            || self.heading_level != 0
            || self.live_politeness != AxLivePoliteness::Off
    }
}

/// The engine's side of the contract: a sink for finished frames and a door
/// to the operating system for escape hatches.
pub trait Host {
    /// A new frame is ready to paint. The display list is in page space;
    /// the host owns scrolling and clipping.
    fn publish(&mut self, state: &PublicState, dl: &DisplayList);
    /// The scroll offset changed without a relayout. Hosts that scroll their
    /// view locally may ignore this; the SDL shell uses it as the light
    /// repaint path.
    fn scroll_updated(&mut self, state: &PublicState);
    /// A navigation started (spinner/hairline) or finished. Hosts that show
    /// nothing may ignore it; the previously painted page stays up during
    /// loads either way.
    fn set_loading(&mut self, loading: bool);
    /// Open a URL outside the engine (mailto:, kitae:open-external:).
    fn open_external(&mut self, url: &str);
    /// The URL bar value changed for a reason other than an explicit
    /// `navigate` — a redirect or a fragment link. Hosts that mirror the
    /// address bar from their own navigate calls must still handle this.
    fn location_changed(&mut self, url: &str) {
        let _ = url;
    }
    /// The document at `url` could not be fetched; an error page is published
    /// as the next frame regardless. Default: ignore.
    fn load_failed(&mut self, url: &str, error: &FetchError) {
        let _ = (url, error);
    }
    /// A Save-Page-as-QTSQ request finished (command 26): `error` is `None`
    /// and `path` names the archive written, or `Some(message)` explains the
    /// failure. Default: ignore.
    fn save_finished(&mut self, path: &str, error: Option<&str>) {
        let _ = (path, error);
    }
    /// H1: engine-to-app messages that are not page state — download
    /// progress (`download\t{id}\t{state}\t{received}\t{total}\t{filename}\t{path}`,
    /// state ∈ active|done|failed:<msg>, total empty when unknown) and later
    /// buckets' prompts. Delivered from [`Browser::poll_downloads`], which
    /// the engine's idle tick calls. Default: ignore.
    fn notify(&mut self, _message: &str) {}
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StylesheetLoadStats {
    pub external_candidates: u32,
    pub external_loaded: u32,
    pub external_bytes: u64,
    pub skipped_count: u32,
    pub skipped_bytes: u64,
    pub duplicate_count: u32,
    pub failed_count: u32,
    pub mixed_content_blocked: u32,
}

impl StylesheetLoadStats {
    fn limit_exceeded(&self) -> bool {
        self.skipped_count > 0 || self.skipped_bytes > 0
    }
}

fn resource_base_url(dom: &Dom, root: Option<NodeId>, base_url: &str) -> String {
    let Some(root) = root else {
        return base_url.to_string();
    };
    let mut found = false;
    let mut selected = None;
    dom.walk(root, &mut |node| {
        if found {
            return;
        }
        if dom.tag(node) == "base" && !inside_template(dom, node) {
            if let Some(href) = dom.element(node).and_then(|el| el.attr("href")) {
                found = true;
                selected = resolve(base_url, href);
            }
        }
    });
    selected.unwrap_or_else(|| base_url.to_string())
}

struct Page {
    /// The HTML the page was prepared from (resize re-runs the cascade at
    /// the new width so `@media` responds to window changes).
    html: String,
    base_url: String,
    resource_base_url: String,
    stylesheet_stats: StylesheetLoadStats,
    dom: Dom,
    styles: Vec<(NodeId, ComputedStyle)>,
    /// I1: cached `StyleEngine::uses_interaction_state()` from the last
    /// relayout — when false, hover/active changes cannot alter any
    /// computed style and skip the full relayout.
    interact_sensitive: bool,
    /// I2: per-body-level-block fragment cache backing `layout_cached`.
    /// Lives on the Page — fresh per navigation, so arena node ids reused
    /// across documents never collide with stale entries.
    frag_cache: FragmentCache,
    /// I2: interaction + environment snapshot at the last relayout. Cached
    /// fragments bake in :hover/:active/:focus styles and environment
    /// variables, so any change here invalidates the whole cache.
    frag_state: (
        Option<NodeId>,
        Option<NodeId>,
        Option<NodeId>,
        EnvironmentState,
    ),
}

/// B1: one running CSS transition — a property on a node interpolating from
/// its previous computed value toward its new one. `from` holds the value
/// string captured at the restyle that started it.
struct RunningTransition {
    property: String,
    from: String,
    to: String,
    start: f32,
    duration: f32,
    timing: kitae_style::Timing,
    /// Discrete properties (no interpolator) jump at the 50% mark.
    discrete: bool,
}

impl RunningTransition {
    /// Current override for `property` at time `t`, or `None` when done
    /// (past `start + duration`, or before the delay starts).
    fn sample(&self, t: f32) -> Option<String> {
        let local = t - self.start;
        if local < 0.0 || self.duration <= 0.0 || local >= self.duration {
            return None;
        }
        let k = self.timing.apply(local / self.duration);
        if self.discrete {
            return Some(if k >= 0.5 {
                self.to.clone()
            } else {
                self.from.clone()
            });
        }
        kitae_style::interpolate_style_value(&self.property, &self.from, &self.to, k)
    }
}

enum PendingHistory {
    Push,
    Position(usize),
}

enum PendingNavigation {
    Get {
        url: String,
        history: PendingHistory,
    },
    Post {
        url: String,
        body: Vec<u8>,
        content_type: String,
    },
}

/// Phase 0: a top-level network navigation currently fetching on the
/// shared kitae_net pool. Exists between `begin_background_load` and the
/// matching `poll_fetches` completion. Superseded loads leave their
/// response unclaimed: the pool claims only the current job's id, and an
/// orphaned answer ages out through the pool's own cap (its data still
/// warms the response cache for back/forward).
struct PendingLoad {
    id: u64,
    /// The submitted (post-normalization) URL. Render runs against it
    /// exactly as the synchronous path would.
    url: String,
    /// QTSQ unlock password threaded to the render stage; Zeroized with
    /// the same discipline as `qtsq_password` (S-7).
    password: Option<zeroize::Zeroizing<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TextSelection {
    anchor: usize,
    head: usize,
}

impl TextSelection {
    fn bounds(self, len: usize) -> (usize, usize) {
        let anchor = self.anchor.min(len);
        let head = self.head.min(len);
        if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        }
    }
}

#[derive(Clone, Debug)]
struct CompositionState {
    node: NodeId,
    start: usize,
    base: String,
    marked: String,
}

const SELECTION_HIGHLIGHT_COLOR: u32 = 0x3B82F6B0;

fn splice_chars(value: &str, start: usize, end: usize, replacement: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let start = start.min(chars.len());
    let end = end.min(chars.len()).max(start);
    let mut out = String::new();
    out.extend(chars[..start].iter());
    out.push_str(replacement);
    out.extend(chars[end..].iter());
    out
}

/// One browser session: history, current page, scroll extent, links.
///
/// The host owns presentation (window, chrome, scrolling, painting) and
/// drives this with commands: [`Browser::navigate`], [`Browser::go`],
/// [`Browser::reload`], [`Browser::resize`], [`Browser::click`].
/// One token per `Browser`: the scope of its `sessionStorage` (the area is
/// private to the browser and survives navigation inside it).
#[cfg(feature = "page-js")]
static NEXT_STORAGE_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub struct Browser {
    host: Box<dyn Host>,
    /// Shared with the style engine (B9): `Arc` so `MeasurerMetrics` can
    /// probe ch/ex at style time without borrowing the browser.
    measurer: Arc<dyn TextMeasurer>,
    history: Vec<String>,
    pos: usize,
    page_url: String,
    page_title: String,
    viewport_w: f32,
    viewport_h: f32,
    scroll_y: f32,
    content_height: f32,
    links: Vec<LinkArea>,
    page: Option<Page>,
    /// Typed outcome of the last load: `Some(e)` after an error page, `None`
    /// after a successful one. Cleared on the next navigation.
    last_error: Option<FetchError>,
    /// Incremental-layout repaint counter (slice G): increments on every
    /// full relayout; clean resizes (same viewport) skip the relayout and
    /// leave the counter untouched — the "no full-page relayout" proof.
    repaints: u64,
    /// Interaction state (slice H): focused node and selection text.
    focused: Option<NodeId>,
    /// Dynamic pseudo-class state (slice N6): hover/active nodes from the
    /// embedder's mouse events. Cleared on navigation like focus.
    hover: Option<NodeId>,
    active: Option<NodeId>,
    selection: Option<String>,
    /// Caret position (A11a): a char offset into the focused editable
    /// control's value, or `None` when nothing editable is focused. Kept in
    /// sync with `focused` by the focus paths and the editing ops; painted
    /// as a thin vertical rect appended to the display list at publish time
    /// (never stored in `last_dl`, so it cannot accumulate).
    caret: Option<usize>,
    selection_range: Option<TextSelection>,
    composition: Option<CompositionState>,
    /// Media transport session (slice J4): mirrors the embedder's player —
    /// (playing, position in seconds) — published with every frame.
    media_session: (bool, f32),
    /// Find-in-page (Phase P1): the query survives navigation (re-resolved
    /// per layout); matches + active index ride every published frame.
    find_query: String,
    find_active: Option<usize>,
    find_matches: Vec<FindMatch>,
    /// Accessibility tree (Phase P2): rebuilt on every relayout alongside
    /// the display list whose bounds it cites.
    ax_tree: Vec<AxNode>,
    ax_focused: Option<String>,
    /// Every built element's AX id, so focus changes (which republish
    /// without a relayout) can re-resolve [`Browser::ax_focused`].
    ax_ids: std::collections::HashMap<NodeId, String>,
    /// Last published display list, for focus/selection republishes without
    /// a relayout.
    last_dl: Option<DisplayList>,
    /// QTSQ password supplied by the host (command 24 / CLI flag). Held for
    /// the session: applied on decode attempts of encrypted .qtsq pages.
    /// Zeroized on replace/drop so the unlock secret does not idle in freed
    /// heap (S-7, SECURITY_AUDIT.md).
    qtsq_password: Option<zeroize::Zeroizing<String>>,
    /// Whether the current page failed to render as an encrypted QTSQ
    /// (`qtsq_viewer_page` sets it; cleared on every other load).
    qtsq_locked: bool,
    /// Signature verdict for the current page's QTSQ payload (slice ④),
    /// set alongside `qtsq_locked` and cleared on every other load.
    qtsq_sig: QtsqSigState,
    /// Embedder environment (Q6): color scheme, reduced motion, contrast —
    /// the `prefers-*` media features evaluate against this. A change
    /// restyles + relayouts the current page.
    environment: EnvironmentState,
    /// B1: running CSS transitions keyed by node — one entry per (node,
    /// property) pair currently animating. `transition_clock` is the last
    /// `tick_transitions` timestamp (seconds, embedder-supplied).
    transitions: HashMap<NodeId, Vec<RunningTransition>>,
    transition_clock: f32,
    /// B1: monotonic anchor for [`Browser::tick_transitions`].
    transition_epoch: std::time::Instant,
    /// H1: the download-registry view already forwarded to the host —
    /// (received, state name, last emit) per download id. [`Browser::poll_downloads`]
    /// diffs against it and coalesces progress to ~100 ms.
    download_seen: HashMap<u32, (u64, String, std::time::Instant)>,
    /// H1: downloads this browser began (see [`note_begun_download`]) — the
    /// only ones `poll_downloads` narrates, so a tab never announces
    /// another tab's download.
    download_owned: std::collections::HashSet<u32>,
    /// H2: files the host picked for a clicked `input[type=file]`, keyed by
    /// the element's `id` (falling back to `name`). The submit path reads
    /// them only for `enctype="multipart/form-data"` POSTs; the paint path
    /// mirrors the selection into the control's `value` attribute.
    file_picks: HashMap<String, Vec<String>>,
    /// H6: the 401 challenge the current page presented (set from the
    /// auth-challenge handoff queue after a load; consumed by
    /// [`Browser::set_auth`]). Cleared on every navigation, so a stale
    /// prompt can never answer for a different page.
    pending_auth: Option<AuthPrompt>,
    pending_navigation: Option<PendingNavigation>,
    /// Phase 0: when on, top-level http(s) navigations submit their fetch
    /// to the shared kitae_net pool instead of blocking this thread — the
    /// displayed page (its clicks included, gated against its own origin)
    /// stays live until `poll_fetches` applies the completion. `about:`,
    /// `file:` and every other non-network scheme loads inline exactly as
    /// before; POST form submits stay synchronous. Hosts opt in at
    /// construction; the default off keeps direct-API, test and CLI
    /// callers on the fully synchronous contract.
    background_fetch: bool,
    /// Phase 0: the in-flight pooled navigation, if any (one per tab).
    pending_load: Option<PendingLoad>,
    /// SCRIPTING_DECISION.md: page JavaScript opt-in (feature `page-js`).
    /// Off by default; hosts set it before the first navigation.
    page_js_enabled: bool,
    /// The page's live script runtime, built at the load's script phase
    /// and dropped at the next navigation.
    #[cfg(feature = "page-js")]
    page_js: Option<pagejs::JsPage>,
    /// Web-storage session token, fixed for this browser's lifetime so
    /// `sessionStorage` survives same-tab navigation.
    #[cfg(feature = "page-js")]
    storage_session: u64,
    /// A back/forward move is in flight: when the loaded document's URL
    /// matches this target, `popstate` fires on it (page-js only).
    pending_popstate: Option<String>,
    /// kitae_ui application mounted on the current page (Phase 1): `Some`
    /// exactly when the page came from a component tree (`about:start` for
    /// now) instead of fetched HTML. Its patch ops drive incremental
    /// re-style/re-layout of the dirty subtree.
    app: Option<ui::LocalApp>,
    /// kitae_live mount for the current page (Phase 2, `live` feature):
    /// `Some` exactly when the page was served by a kitae_live server —
    /// detected by the `x-kitae-live: 1` response header on the fetch.
    /// The static HTML renders alone; the socket layer adds patches in
    /// and routes events out.
    #[cfg(feature = "live")]
    live: Option<live::LiveApp>,
    /// Live sockets on/off (FFI-free master switch). Off = pages render
    /// statically even when the server offers a socket.
    #[cfg(feature = "live")]
    live_enabled: bool,
}

/// B9: adapts the layout `TextMeasurer` to kitae_style's `FontMetrics` so
/// `ch`/`ex` lengths fold at style time against the page's real font.
/// `ch` is the advance of `0`; `ex` is the measurer's x-height (shells
/// without the metric keep the 0.5em default).
struct MeasurerMetrics(Arc<dyn TextMeasurer>);

impl kitae_style::FontMetrics for MeasurerMetrics {
    fn ch_px(&self, family: &str, size: f32, weight: u32, italic: bool, monospace: bool) -> f32 {
        self.0
            .measure_family("0", size, weight, italic, monospace, family)
    }
    fn ex_px(&self, family: &str, size: f32, weight: u32, italic: bool, monospace: bool) -> f32 {
        self.0.ex_px(size, weight, italic, monospace, family)
    }
}

impl Browser {
    /// Create an engine with a host sink and a text measurer. `viewport`
    /// is the page area the host can paint into, in logical points.
    pub fn new(
        host: Box<dyn Host>,
        measurer: Box<dyn TextMeasurer>,
        viewport: (f32, f32),
    ) -> Browser {
        Browser {
            host,
            measurer: Arc::from(measurer),
            history: Vec::new(),
            pos: 0,
            page_url: String::new(),
            page_title: String::new(),
            viewport_w: viewport.0,
            viewport_h: viewport.1,
            scroll_y: 0.0,
            content_height: 0.0,
            links: Vec::new(),
            page: None,
            last_error: None,
            repaints: 0,
            focused: None,
            hover: None,
            active: None,
            selection: None,
            caret: None,
            selection_range: None,
            composition: None,
            media_session: (false, 0.0),
            find_query: String::new(),
            find_active: None,
            find_matches: Vec::new(),
            ax_tree: Vec::new(),
            ax_focused: None,
            ax_ids: std::collections::HashMap::new(),
            last_dl: None,
            qtsq_password: None,
            qtsq_locked: false,
            qtsq_sig: QtsqSigState::Unsigned,
            environment: EnvironmentState::default(),
            transitions: HashMap::new(),
            transition_clock: 0.0,
            transition_epoch: std::time::Instant::now(),
            download_seen: HashMap::new(),
            download_owned: std::collections::HashSet::new(),
            file_picks: HashMap::new(),
            pending_auth: None,
            pending_navigation: None,
            background_fetch: false,
            pending_load: None,
            page_js_enabled: false,
            pending_popstate: None,
            #[cfg(feature = "page-js")]
            page_js: None,
            #[cfg(feature = "page-js")]
            storage_session: NEXT_STORAGE_SESSION.fetch_add(1, Ordering::Relaxed),
            app: None,
            #[cfg(feature = "live")]
            live: None,
            #[cfg(feature = "live")]
            live_enabled: true,
        }
    }

    /// Current public state (for hosts that poll instead of caching publish).
    /// Whether a top-level load is still in flight (the pooled/background
    /// path). The agent plane's settle loop polls this; a host's frame loop
    /// reads it through the published frame instead.
    pub fn load_in_flight(&self) -> bool {
        self.pending_load.is_some()
    }

    /// The current document's tree. `None` before the first load. Read-only:
    /// mutations go through the engine's own APIs so layout and scripts stay
    /// in step.
    pub fn page_dom(&self) -> Option<&Dom> {
        self.page.as_ref().map(|p| &p.dom)
    }

    /// Whether a page-script session is live: the `page-js` feature is on,
    /// scripting is enabled for this browser, and the current page actually
    /// produced a session. Always `false` without the feature.
    pub fn page_js_running(&self) -> bool {
        #[cfg(feature = "page-js")]
        {
            self.page_js_enabled && self.page_js.is_some()
        }
        #[cfg(not(feature = "page-js"))]
        {
            false
        }
    }

    /// The viewport this browser lays out at, in CSS pixels — what an
    /// evidence manifest must record for a replay to be meaningful.
    pub fn viewport_size(&self) -> (f32, f32) {
        (self.viewport_w, self.viewport_h)
    }

    pub fn state(&self) -> PublicState {
        PublicState {
            url: self.page_url.clone(),
            resource_base_url: self
                .page
                .as_ref()
                .map(|page| page.resource_base_url.clone())
                .filter(|base| !base.is_empty())
                .unwrap_or_else(|| self.page_url.clone()),
            title: self.page_title.clone(),
            can_go_back: self.pos > 0,
            can_go_forward: self.pos + 1 < self.history.len(),
            scroll_y: self.scroll_y,
            content_height: self.content_height,
            focused_id: self.focused_label(),
            selection: self.selection.clone(),
            media: self.media_state(),
            find_matches: self.find_matches.clone(),
            find_active: self.find_active,
            ax_tree: self.ax_tree.clone(),
            ax_focused: self.ax_focused.clone(),
            qtsq_locked: self.qtsq_locked,
            qtsq_sig: self.qtsq_sig,
            print_media: self.environment.media_type == MediaType::Print,
        }
    }

    /// Media info for the current page with the transport mirror applied —
    /// the snapshot hosts and the wire format see. The QTSQ store keys by
    /// page URL; web media (F1–F4) falls back to the page's first media
    /// item through the web stores.
    fn media_state(&self) -> Option<MediaInfo> {
        let base =
            qtsq_media_for_url(&self.page_url).or_else(|| web_media_for_page(&self.page_url))?;
        Some(MediaInfo {
            playing: self.media_session.0,
            position: self.media_session.1,
            ..base
        })
    }

    /// Total document height from the last layout.
    pub fn content_height(&self) -> f32 {
        self.content_height
    }

    // ── node handles (RP-A1 second half) ─────────────────────────────────

    /// Mount a kitae_ui component tree as the PAGE — the embedder-held app
    /// mode from the plan (`the embedder's Rust app is the "server"`):
    /// components run in-process, signals re-render on demand, and the
    /// engine renders it like any page. `node_ref:`-marked elements become
    /// reachable through [`Self::node_rect`], [`Self::focus_node`], and
    /// [`Self::scroll_node_into_view`].
    pub fn load_component(&mut self, render: std::rc::Rc<dyn Fn() -> View>) {
        // Canonical HTML copy for save/archive: one extra render of the
        // same closure (hooks are position-stable, so this is safe).
        let html = {
            kitae_ui::hooks::wrap_render(render.clone())();
            let view = render();
            kitae_ui::render::render_html(&view.nodes)
        };
        let mut app = ui::LocalApp::new(render);
        let (dom, body) = app.mount_dom();
        let styles = self.styles_for_dom(&dom, "");
        let title = page_title(&dom, body).unwrap_or_else(|| "App".into());
        self.app = Some(app);
        #[cfg(feature = "live")]
        {
            self.live = None;
        }
        let old_page_for_media = self.page_url.clone();
        self.page_url = String::new();
        self.host.location_changed("");
        self.page_title = title;
        self.focused = None;
        self.hover = None;
        self.active = None;
        self.selection = None;
        self.caret = None;
        self.selection_range = None;
        self.composition = None;
        self.page = Some(Page {
            html,
            base_url: String::new(),
            resource_base_url: String::new(),
            stylesheet_stats: StylesheetLoadStats::default(),
            dom,
            styles,
            interact_sensitive: true,
            frag_cache: FragmentCache::default(),
            frag_state: (None, None, None, Default::default()),
        });
        self.links.clear();
        self.last_error = None;
        self.media_session = (false, 0.0);
        release_qtsq_page_state(&old_page_for_media);
        clear_web_media_for(&old_page_for_media);
        self.ensure_page_fonts();
        self.relayout();
    }

    /// The most recently laid-out display list (embedders that render from
    /// Rust directly, tests). `None` before the first load.
    pub fn last_display_list(&self) -> Option<&DisplayList> {
        self.last_dl.as_ref()
    }

    pub fn links(&self) -> &[LinkArea] {
        &self.links
    }

    /// Typed outcome of the last load: `Some` when the current page is an
    /// error page, `None` when the document loaded. Cleared by the next load.
    pub fn last_error(&self) -> Option<&FetchError> {
        self.last_error.as_ref()
    }

    pub fn stylesheet_load_stats(&self) -> Option<&StylesheetLoadStats> {
        self.page.as_ref().map(|page| &page.stylesheet_stats)
    }

    /// Append `url` to navigation history (M-4, SECURITY_AUDIT.md): the
    /// forward tail beyond the current position is dropped first (standard
    /// browser behavior), then, once the history exceeds [`HISTORY_MAX`],
    /// the oldest entries are evicted from the front. The current position
    /// always lands on the new entry, so back/forward semantics are
    /// unaffected by the cap.
    fn push_history(&mut self, url: String) {
        self.history.truncate(self.pos + 1);
        self.history.push(url);
        while self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
        self.pos = self.history.len() - 1;
    }

    pub fn navigate(&mut self, input: &str) {
        let url = normalize_input(input);
        let url = if kitae_net::https_only() {
            kitae_net::apply_https_only(&url).unwrap_or(url)
        } else {
            url
        };
        self.request_navigation(url, PendingHistory::Push);
    }

    fn request_navigation(&mut self, url: String, history: PendingHistory) {
        if kitae_net::warn_insecure_http() && kitae_net::is_insecure_navigation(&url) {
            let message = url.replace('\t', "").replace('\r', "").replace('\n', "");
            self.pending_navigation = Some(PendingNavigation::Get {
                url: url.clone(),
                history,
            });
            self.host.set_loading(false);
            self.host.notify(&format!("insecure\t{message}"));
            return;
        }
        match history {
            PendingHistory::Push => self.push_history(url.clone()),
            PendingHistory::Position(index) => self.pos = index,
        }
        self.scroll_y = 0.0;
        self.load(&url);
    }

    pub fn resolve_insecure_navigation(&mut self, allow: bool) -> bool {
        let Some(pending) = self.pending_navigation.take() else {
            return false;
        };
        if !allow {
            return true;
        }
        match pending {
            PendingNavigation::Get { url, history } => {
                self.request_navigation_unchecked(url, history)
            }
            PendingNavigation::Post {
                url,
                body,
                content_type,
            } => {
                self.submit_post_bytes_unchecked(&url, &body, &content_type);
            }
        }
        true
    }

    fn request_navigation_unchecked(&mut self, url: String, history: PendingHistory) {
        match history {
            PendingHistory::Push => self.push_history(url.clone()),
            PendingHistory::Position(index) => self.pos = index,
        }
        self.scroll_y = 0.0;
        self.load(&url);
    }

    /// Load an HTML string directly (tests, embedders with custom protocols,
    /// file contents already read). Records `base_url` in history like a
    /// navigation.
    pub fn load_html(&mut self, html: &str, base_url: &str) {
        self.push_history(base_url.to_string());
        self.scroll_y = 0.0;
        self.focused = None;
        self.hover = None;
        self.active = None;
        self.selection = None;
        self.caret = None;
        self.selection_range = None;
        self.composition = None;
        self.media_session = (false, 0.0);
        let old_page_for_media = self.page_url.clone();
        self.find_active = None;
        self.find_matches.clear();
        // SCRIPTING_DECISION.md: embedder-supplied pages script only when
        // the host opted in; the mutated tree styles + lays out below.
        let (dom, root, styles, js_title) = match self.try_render_with_page_js(html, base_url) {
            Some((d, r, s, t)) => (d, r, s, Some(t)),
            #[cfg(not(feature = "page-js"))]
            None => {
                let (d, r, s) = prepare_in_env(html, base_url, self.viewport_w, &self.environment);
                (d, r, s, None)
            }
            #[cfg(feature = "page-js")]
            None => {
                let (d, r, s) = prepare_in_env(html, base_url, self.viewport_w, &self.environment);
                (d, r, s, None)
            }
        };
        let resource_base_url = resource_base_url(&dom, root, base_url);
        self.page_url = base_url.to_string();
        // Embedder-supplied HTML has no network context: its (local)
        // subresource fetches are trusted (S-1, SECURITY_AUDIT.md).
        kitae_net::set_page_origin(base_url);
        release_qtsq_page_state(&old_page_for_media);
        clear_web_media_for(&old_page_for_media);
        self.host.location_changed(&self.page_url);
        self.page_title = js_title
            .or_else(|| root.and_then(|r| page_title(&dom, r)))
            .unwrap_or_default();
        self.page = Some(Page {
            html: html.to_string(),
            base_url: base_url.to_string(),
            resource_base_url,
            stylesheet_stats: StylesheetLoadStats::default(),
            dom,
            styles,
            interact_sensitive: true,
            frag_cache: FragmentCache::default(),
            frag_state: (None, None, None, Default::default()),
        });
        self.links.clear();
        self.last_error = None;
        self.ensure_page_fonts();
        self.relayout();
    }

    pub fn go(&mut self, delta: i32) {
        let target = self.pos as i64 + delta as i64;
        if target < 0 || target as usize >= self.history.len() {
            return;
        }
        let index = target as usize;
        let url = self.history[index].clone();
        self.pending_popstate = Some(url.clone());
        self.request_navigation(url, PendingHistory::Position(index));
    }

    pub fn reload(&mut self) {
        let url = self.page_url.clone();
        self.scroll_y = 0.0;
        self.load(&url);
    }

    /// The host resized its page area; relayout at the new width.
    /// Clean resizes (same viewport within 0.5px) skip the relayout entirely
    /// — invalidation: no dirty input, no repaint.
    pub fn resize(&mut self, width: f32, height: f32) {
        if (width - self.viewport_w).abs() < 0.5 && (height - self.viewport_h).abs() < 0.5 {
            return;
        }
        self.viewport_w = width;
        self.viewport_h = height;
        self.relayout();
    }

    /// Incremental-layout repaint counter: full relayouts so far.
    pub fn repaint_count(&self) -> u64 {
        self.repaints
    }

    /// Body content width: the width a top-level body child lays out at.
    /// Resolved from the body's own margin/border/padding in the page
    /// styles — a page that resets the UA sheet's `body { padding: 16px }`
    /// must splice and probe at its real content width, not the
    /// viewport − 32 the UA padding alone would suggest.
    fn body_content_width(&self) -> f32 {
        let fallback = || (self.viewport_w - 32.0).max(40.0);
        let Some(page) = self.page.as_ref() else {
            return fallback();
        };
        let Some(body) = page.dom.root().and_then(|r| {
            page.dom
                .children(r)
                .iter()
                .find(|&&c| page.dom.tag(c) == "body")
                .copied()
        }) else {
            return fallback();
        };
        let Some((_, s)) = page.styles.iter().find(|(n, _)| *n == body) else {
            return fallback();
        };
        let m = s.margin.resolve(self.viewport_w);
        let b = s.border.resolve(self.viewport_w, s.color);
        let p = s.padding.resolve(self.viewport_w);
        (self.viewport_w - m.left - m.right - b[1].width - b[3].width - p.left - p.right).max(40.0)
    }

    /// Dirty-subtree height probe (slice G): lay out only the subtree rooted
    /// at the element with `element_id` at the body's content width and
    /// return its height. `None` when the page or element is missing.
    pub fn subtree_height(&self, element_id: &str) -> Option<f32> {
        let page = self.page.as_ref()?;
        let target = page.dom.get_element_by_id(element_id)?;
        let dl = kitae_layout::layout_subtree_in_env(
            &page.dom,
            &page.styles,
            target,
            self.body_content_width(),
            self.viewport_h,
            self.measurer.as_ref(),
            &self.environment,
        );
        Some(dl.content_height)
    }

    /// On-demand decoded pixels for an image URL (Phase K3 scroll-window
    /// hook): resolves `src` against the page base, decodes on first sight,
    /// and returns the shared raster. `None` keeps the placeholder path.
    pub fn raster_for(&mut self, src: &str) -> Option<QtsqRaster> {
        let (base, document_base) = self
            .page
            .as_ref()
            .map(|p| (p.resource_base_url.clone(), p.base_url.clone()))
            .unwrap_or_default();
        let abs = resolve(&base, src).unwrap_or_else(|| src.to_string());
        // I6: mixed-content policy — no plaintext subresource off an https page.
        if !kitae_net::mixed_content_allowed(&document_base, &abs) {
            return None;
        }
        ensure_qtsq_raster(&abs).or_else(|| ensure_web_raster(&abs))
    }

    /// Number of decoded rasters currently cached (inspection / tests).
    pub fn raster_len(&self) -> usize {
        qtsq_raster_store().lock().unwrap().by_url.len()
    }

    /// The host scrolled its view by a relative delta; the engine tracks the
    /// offset so link hit-testing (page space) and clamping agree with what
    /// is shown, and tells the host so it can repaint.
    pub fn scroll_by(&mut self, dy: f32) {
        let max = (self.content_height - self.viewport_h).max(0.0);
        self.scroll_y = (self.scroll_y + dy).clamp(0.0, max);
        self.host.scroll_updated(&self.state());
    }

    /// The host scrolled its view to an absolute offset (repaints via the
    /// host callback as with [`Browser::scroll_by`]).
    pub fn set_scroll(&mut self, y: f32) {
        let max = (self.content_height - self.viewport_h).max(0.0);
        self.scroll_y = y.clamp(0.0, max);
        self.host.scroll_updated(&self.state());
    }

    pub fn scroll_y(&self) -> f32 {
        self.scroll_y
    }

    /// Click at page-space coordinates (scroll already added by the host).
    pub fn click(&mut self, x: f32, y: f32) {
        // Phase 2 (`live`): a live page's handler-bearing elements send
        // Event frames — the authored handler runs on the SERVER; the
        // patch it produces arrives on the next poll and splices in.
        #[cfg(feature = "live")]
        if self.live.is_some() && self.live_click(x, y) {
            return;
        }
        // Phase 1 (P1c): a mounted kitae_ui app gets the click first. If a
        // component's `on:click` handler claims it, the patch stream the
        // handler's signal change produces is applied + spliced right here;
        // otherwise the click falls through to links.
        if self.app.is_some() && self.app_click(x, y) {
            return;
        }
        // SCRIPTING_DECISION.md (amended): with page JS on, the page's own
        // listeners get the click before the engine's hit paths. Browser
        // semantics: default actions proceed unless a listener called
        // preventDefault() — so listeners no longer *own* the click (one
        // delegated analytics handler must not break every link). A
        // prevented click stands the engine's paths down entirely.
        #[cfg(feature = "page-js")]
        if self.page_js_enabled && self.page_js.is_some() && self.dispatch_page_js_click(x, y) {
            return;
        }
        // A6: an open modal dialog gates the click. Outside the card, the
        // click is swallowed (spec default: the backdrop does not close the
        // dialog); inside, falls through to toggles/links as usual. Card
        // bounds come from the display list: dialog rects carry the box's
        // `translateX(-50%)` as a baked matrix with pre-transform geometry,
        // so each rect's four corners are mapped through it and unioned.
        // `node_rect` can't be reused — it unions raw coordinates.
        if let Some(page) = self.page.as_ref() {
            let dialog = page.dom.root().and_then(|r| open_dialog(&page.dom, r));
            if let Some(dialog) = dialog {
                if let Some(dl) = self.last_dl.as_ref() {
                    let mut hit = false;
                    for r in &dl.rects {
                        if r.node != dialog.0 {
                            continue;
                        }
                        match r.transform {
                            Some(m) => {
                                let corners = [
                                    (r.x, r.y),
                                    (r.x + r.w, r.y),
                                    (r.x, r.y + r.h),
                                    (r.x + r.w, r.y + r.h),
                                ];
                                let pts: Vec<(f32, f32)> = corners
                                    .iter()
                                    .map(|&(cx, cy)| {
                                        (
                                            m.0[0] * cx + m.0[1] * cy + m.0[2],
                                            m.0[3] * cx + m.0[4] * cy + m.0[5],
                                        )
                                    })
                                    .collect();
                                let min_x = pts.iter().fold(f32::INFINITY, |a, p| a.min(p.0));
                                let max_x = pts.iter().fold(f32::NEG_INFINITY, |a, p| a.max(p.0));
                                let min_y = pts.iter().fold(f32::INFINITY, |a, p| a.min(p.1));
                                let max_y = pts.iter().fold(f32::NEG_INFINITY, |a, p| a.max(p.1));
                                if x >= min_x && x <= max_x && y >= min_y && y <= max_y {
                                    hit = true;
                                    break;
                                }
                            }
                            None => continue,
                        }
                    }
                    if !hit {
                        return;
                    }
                }
            }
        }
        // A7: form controls own their clicks — focus, toggle, or value
        // adjust happens before summary toggles and links get the point.
        self.click_form_control(x, y);
        // F4: the builtin transport chrome sits on top of its media element
        // — play/pause toggle and scrubber seek happen before summary
        // toggles and links get the point.
        if self.click_media_chrome(x, y) {
            return;
        }
        // A5: `<summary>` toggles first — clicking one flips the parent
        // `<details>`'s `open` attribute and relayouts.
        if let Some(node) = self.last_dl.as_ref().and_then(|dl| {
            dl.toggles
                .iter()
                .find(|t| x >= t.x && x <= t.x + t.w && y >= t.y && y <= t.y + t.h)
        }) {
            let node = NodeId(node.node);
            let open = self
                .page
                .as_ref()
                .and_then(|p| p.dom.element(node))
                .is_some_and(|e| e.attr("open").is_some());
            if let Some(page) = self.page.as_mut() {
                if open {
                    page.dom.remove_attr(node, "open");
                } else {
                    page.dom.set_attr(node, "open", "");
                }
            }
            self.relayout();
            return;
        }
        // Q5: sticky boxes shift at paint time; hit-test their links at the
        // shifted position so the engine agrees with what the host shows.
        //
        // The shift moves the *rect*, so it must move the click point the
        // other way: `LinkArea::contains` knows the unshifted geometry, and
        // feeding it the link's own y (rather than the click's) would make
        // the vertical test vacuous — every click in the link's x-span
        // would activate it, however far above or below the link it landed.
        let sticky_shift = |i: usize| -> f32 {
            let Some(dl) = self.last_dl.as_ref() else {
                return 0.0;
            };
            for s in &dl.stickies {
                let (start, count) = (s.link.0 as usize, s.link.1 as usize);
                if i >= start && i < start + count {
                    return s.shift(self.scroll_y, self.viewport_h);
                }
            }
            0.0
        };
        let Some(link) = self
            .links
            .iter()
            .enumerate()
            .find(|(i, l)| l.contains(x, y - sticky_shift(*i)))
            .map(|(_, l)| l)
        else {
            return;
        };
        let href = link.href.clone();
        // Reopened QTSQ archives keep the .qtsq file URL as the session URL
        // but resolve links against the archived page's original base. For
        // every other page kind the two are the same value.
        let base = self
            .page
            .as_ref()
            .map(|p| p.resource_base_url.clone())
            .unwrap_or_else(|| self.page_url.clone());
        let Some(abs) = resolve(&base, &href) else {
            return;
        };

        // Escape hatches go to the host (which forwards to the OS).
        // S-2 (SECURITY_AUDIT.md): `kitae:open-external:` is engine
        // vocabulary, not page vocabulary — only the engine's own
        // interstitials (whose links always embed the page's own URL) and
        // local/about pages may open externally, and only http(s) targets
        // pass through. A denied request still reports *something* opened
        // (about:blank) so the frame protocol's click round-trip is honest.
        if let Some(target) = abs.strip_prefix("kitae:open-external:") {
            if open_external_permitted(&self.gate_origin(), &abs) {
                self.host.open_external(target);
            } else {
                self.host.open_external("about:blank");
            }
            return;
        }
        // H8: the crash page's only affordance. Engine vocabulary like
        // `kitae:open-external:` — the crash page's gate origin is
        // `kitae:crash`, so the S-2 gate can never be reached with it from a
        // network page (a network page's resolve can produce the string, but
        // reload just refetches the *page's own* URL, which the page already
        // is; harmless, and the crash-page flow is the intended user).
        if abs == "kitae:reload" {
            self.reload();
            return;
        }
        if abs.starts_with("mailto:") {
            self.host.open_external(&abs);
            return;
        }

        // S-1 (SECURITY_AUDIT.md): a network page cannot pull local files
        // in through a link — the fetch path enforces the same rule for
        // subresources (`@import`, `<link>`, `<img src>`).
        if abs.starts_with("file://") && !kitae_net::local_origin_allowed(&self.gate_origin()) {
            self.navigate_blocked_local(&abs);
            return;
        }

        // DuckDuckGo's HTML endpoint wraps real targets in ?uddg=<encoded>.
        let target = ddg_result_target(&abs).unwrap_or(abs);
        self.navigate(&target);
    }

    /// H5 (read-only inspector): hit-test the last display list at a page
    /// point and narrate the element under it as an `inspect\t{node}`
    /// notification. Paint stacks content over background, so the scan runs
    /// text runs → images → painted boxes, each taking the topmost hit; links
    /// are first because their inline geometry is exact (and an image inside
    /// a link reasonably reports the link). Nothing hit → silent.
    pub fn pick_inspect(&mut self, x: f32, y: f32) {
        if let Some(node) = self.node_at_point(x, y) {
            self.host.notify(&format!("inspect\t{}", node.0));
        }
    }

    /// The DOM node under a page point, in the inspector's priority order:
    /// links, then text runs, then images, then painted boxes. `None` when
    /// the point lands on nothing the last frame drew.
    ///
    /// The agent plane uses this to say *which* node a click acted on — the
    /// same resolution its own hit-testing performs, asked as a query.
    pub fn node_at_point(&self, x: f32, y: f32) -> Option<NodeId> {
        let dl = self.last_dl.as_ref()?;
        for link in dl.links.iter().rev() {
            if link.contains(x, y) {
                return Some(NodeId(link.node));
            }
        }
        node_at_point_in(dl, x, y)
    }

    /// H5: the inspector report for `node` — a `tag#id.classes` header, the
    /// raw attributes, then the cascade's computed properties. Multi-line
    /// text that never contains tabs (the wire frames the whole report as
    /// one notification field).
    pub fn inspect_info(&self, node: u32) -> String {
        let Some(page) = self.page.as_ref() else {
            return String::new();
        };
        let id = NodeId(node);
        let mut out = page.dom.tag(id).to_string();
        if let Some(el) = page.dom.element(id) {
            if let Some(v) = el.attr("id") {
                out.push('#');
                out.push_str(v);
            }
            if let Some(v) = el.attr("class") {
                for c in v.split_whitespace() {
                    out.push('.');
                    out.push_str(c);
                }
            }
            out.push('\n');
            for (name, value) in &el.attributes {
                out.push_str("  ");
                out.push_str(name);
                out.push_str("=\"");
                out.push_str(value);
                out.push_str("\"\n");
            }
        } else {
            out.push('\n');
        }
        // The cascade runs on every element, but a node laid out before the
        // last restyle (or never styled) can be missing from the snapshot.
        if let Some((_, style)) = page.styles.iter().find(|(nid, _)| *nid == id) {
            out.push_str("-- computed --\n");
            out.push_str(&format!(
                "  display: {}\n",
                inspect_display_name(style.display)
            ));
            out.push_str(&format!("  color: {}\n", inspect_css_color(style.color)));
            out.push_str(&format!(
                "  background: {}\n",
                inspect_css_color(style.background)
            ));
            out.push_str(&format!("  font-size: {}px\n", style.font_size));
            out.push_str(&format!("  font-weight: {}\n", style.font_weight));
            out.push_str(&format!(
                "  font-style: {}\n",
                if style.italic { "italic" } else { "normal" }
            ));
            let mut deco: Vec<&str> = Vec::new();
            if style.underline {
                deco.push("underline");
            }
            if style.overline {
                deco.push("overline");
            }
            if style.line_through {
                deco.push("line-through");
            }
            if !deco.is_empty() {
                out.push_str(&format!("  text-decoration: {}\n", deco.join(" ")));
            }
            if !style.font_family.is_empty() {
                out.push_str(&format!("  font-family: {}\n", style.font_family));
            }
            out.push_str(&format!("  text-align: {:?}\n", style.text_align).to_lowercase());
            out.push_str(&format!("  line-height: {}\n", style.line_height));
            // Percentages resolve against the containing-block width, which
            // the report doesn't know — report the px parts as-is.
            let m = style.margin.resolve(0.0);
            out.push_str(&format!(
                "  margin: {}px {}px {}px {}px\n",
                m.top, m.right, m.bottom, m.left
            ));
            let p = style.padding.resolve(0.0);
            out.push_str(&format!(
                "  padding: {}px {}px {}px {}px\n",
                p.top, p.right, p.bottom, p.left
            ));
            for (side, bs) in [
                ("top", &style.border.top),
                ("right", &style.border.right),
                ("bottom", &style.border.bottom),
                ("left", &style.border.left),
            ] {
                let w = bs.width.resolve(0.0);
                if w > 0.0 {
                    out.push_str(&format!("  border-{side}: {w}px\n"));
                }
            }
        }
        out
    }

    /// H5: command 39's answer — compute [`Browser::inspect_info`] and hand
    /// it to the host as an `inspectinfo\t{lines…}` notification.
    pub fn report_inspect_info(&mut self, node: u32) {
        let info = self.inspect_info(node);
        self.host.notify(&format!("inspectinfo\t{info}"));
    }

    /// A7/A11b: the clicked editable target, if any: (node, input type, tag,
    /// widest rect of the control containing the point). Document order,
    /// first hit. Every rect the widget paints carries the control's node
    /// stamp, so a hit on the knob or border resolves to the same control as
    /// the box. A contenteditable host with no control box (plain text)
    /// hit-tests through its painted text runs instead: one click anywhere
    /// on a line it paints focuses it.
    fn clicked_control(
        &self,
        x: f32,
        y: f32,
    ) -> Option<(NodeId, String, String, (f32, f32, f32, f32))> {
        let dl = self.last_dl.as_ref()?;
        let page = self.page.as_ref()?;
        let root = page.dom.root()?;
        let mut found: Option<(NodeId, String, String, (f32, f32, f32, f32))> = None;
        page.dom.walk(root, &mut |n| {
            if found.is_some() {
                return;
            }
            let tag = page.dom.tag(n);
            let editable = page
                .dom
                .element(n)
                .and_then(|e| e.attr("contenteditable"))
                .map(|v| v != "false")
                .unwrap_or(false);
            if !matches!(tag, "input" | "textarea" | "select" | "button") && !editable {
                return;
            }
            let mut widest: Option<(f32, f32, f32, f32)> = None;
            for r in &dl.rects {
                if r.node != n.0 {
                    continue;
                }
                if x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h {
                    let is_widest = widest.map(|b| r.w > b.2).unwrap_or(true);
                    if is_widest {
                        widest = Some((r.x, r.y, r.w, r.h));
                    }
                }
            }
            if widest.is_none() && editable {
                let mut line: Option<(f32, f32, f32, f32)> = None;
                for t in &dl.texts {
                    let tn = NodeId(t.node);
                    if tn != n && !within(&page.dom, tn, n) {
                        continue;
                    }
                    let (a, d) = self.measurer.metrics_family(t.size, t.monospace, &t.family);
                    let (top, bot) = (t.y - a, t.y + d);
                    if y < top || y > bot {
                        continue;
                    }
                    let w = self.measurer.measure_family(
                        &t.text,
                        t.size,
                        t.weight,
                        t.italic,
                        t.monospace,
                        &t.family,
                    );
                    match line {
                        Some((lx, _, rx, _)) => {
                            line = Some((lx.min(t.x), top, rx.max(t.x + w), bot));
                        }
                        None => line = Some((t.x, top, t.x + w, bot)),
                    }
                }
                if let Some((x0, top, x1, bot)) = line {
                    if x >= x0 - 2.0 && x <= x1 + 2.0 {
                        widest = Some((x0, top, x1 - x0, bot - top));
                    }
                }
            }
            if let Some(b) = widest {
                let it = page
                    .dom
                    .element(n)
                    .and_then(|e| e.attr("type"))
                    .unwrap_or("")
                    .to_ascii_lowercase();
                found = Some((n, it, tag.to_string(), b));
            }
        });
        found
    }

    /// A7: form controls own their clicks. The click focuses the control
    /// first (as in a real browser), then dispatches: checkbox/radio toggle,
    /// submit-ish buttons submit their form, a range sets its value from the
    /// click position on the track. Text-like inputs, selects, color swatches
    /// and file pills take focus only — the OS pickers are host-layer, and
    /// Enter-to-submit stays an activate_focused() behavior, not a click one.
    fn click_form_control(&mut self, x: f32, y: f32) {
        let Some((node, input_type, tag, b)) = self.clicked_control(x, y) else {
            if self.selection_range.is_some() || self.composition.is_some() {
                self.cancel_composition_state();
                self.clear_text_selection();
                self.selection = None;
                self.publish_current();
            }
            return;
        };
        let had_range = self.selection_range.is_some();
        self.cancel_composition_state();
        self.clear_text_selection();
        if had_range {
            self.selection = None;
        }
        self.focused = Some(node);
        self.sync_caret();
        // A11a: clicking inside an editable control places the caret at the
        // click position (not just the end) before the republish paints it.
        // A11b: contenteditable hosts paint their value across many word
        // runs and possibly several lines, so placement goes by point; the
        // single-run x mapping stays for form controls.
        if self.editable_focus().is_some() {
            let ce = self
                .page
                .as_ref()
                .map(|p| self.is_contenteditable_host(&p.dom, node))
                .unwrap_or(false);
            self.caret = Some(if ce {
                self.caret_offset_for_point(node, x, y)
            } else {
                self.caret_offset_for_x(node, x)
            });
        }
        self.refresh_ax_focus();
        match (tag.as_str(), input_type.as_str()) {
            ("input", "range") => {
                // Track geometry mirrors the UA paint: the track runs the
                // control box inset 2px on each side.
                let inner = (b.2 - 4.0).max(1.0);
                let frac = ((x - (b.0 + 2.0)) / inner).clamp(0.0, 1.0);
                let (min, max, step) = self
                    .page
                    .as_ref()
                    .and_then(|p| p.dom.element(node))
                    .map(|e| {
                        (
                            e.attr("min")
                                .and_then(|v| v.trim().parse::<f32>().ok())
                                .unwrap_or(0.0),
                            e.attr("max")
                                .and_then(|v| v.trim().parse::<f32>().ok())
                                .unwrap_or(100.0),
                            e.attr("step")
                                .and_then(|v| v.trim().parse::<f32>().ok())
                                .unwrap_or(1.0),
                        )
                    })
                    .unwrap_or((0.0, 100.0, 1.0));
                let span = (max - min).abs().max(0.0001);
                let mut v = min + frac * span;
                if step > 0.0 {
                    // Same quantization the knob paints with.
                    v = min + ((v - min) / step).round() * step;
                }
                v = v.clamp(min.min(max), max.max(min));
                let s = if (v - v.round()).abs() < 1e-6 {
                    format!("{}", v.round() as i64)
                } else {
                    format!("{v}")
                };
                if let Some(page) = self.page.as_mut() {
                    page.dom.set_attr(node, "value", &s);
                }
                #[cfg(feature = "page-js")]
                {
                    // A pointer on a range commits a value: input + change,
                    // like every discrete control.
                    let _ = self.dispatch_js_event_at("input", node, None);
                    let _ = self.dispatch_js_event_at("change", node, None);
                }
                self.relayout();
            }
            ("input", "checkbox" | "radio" | "submit" | "image" | "button") => {
                self.activate_focused();
            }
            ("button", _) => {
                self.activate_focused();
            }
            ("input", "file") => {
                // H2: the click opens the host's file panel — the engine asks
                // via a notification, the host answers with set_file_files.
                self.publish_current();
                self.request_file_pick(node);
            }
            _ => {
                // Text-like inputs, select, color: focus is the whole
                // click interaction.
                self.publish_current();
            }
        }
    }

    /// A11a: the char offset into the focused control's value nearest page-space
    /// `x` (click-to-place). Walks the painted label run's glyphs accumulating
    /// the run's own face widths, rounding to the nearest glyph edge. Falls
    /// back to the end of the value when the control paints no run (empty,
    /// no placeholder) or there is no display list yet.
    fn caret_offset_for_x(&self, node: NodeId, x: f32) -> usize {
        let end = self.read_control_value(node).chars().count();
        let Some(run) = self
            .last_dl
            .as_ref()
            .and_then(|dl| dl.texts.iter().find(|t| t.node == node.0))
        else {
            return end;
        };
        let target = x - run.x;
        if target <= 0.0 {
            return 0;
        }
        let value = self.read_control_value(node);
        let mut acc = 0.0;
        let mut off = 0usize;
        for c in value.chars() {
            let mut buf = [0u8; 4];
            let w = self.measurer.measure_family(
                c.encode_utf8(&mut buf),
                run.size,
                run.weight,
                run.italic,
                run.monospace,
                &run.family,
            );
            if acc + w * 0.5 > target {
                break;
            }
            acc += w;
            off += 1;
        }
        off.min(end)
    }

    /// A7: arrow-key style value stepping for the focused control — range
    /// and number move by `step` (default 1) clamped to [min,max]; select
    /// moves the `selected` marker one option. Hosts without a keyDown path
    /// call this directly (same standing arrangement as Esc). Returns whether
    /// the control changed.
    pub fn step_focused(&mut self, dir: i32) -> bool {
        let Some(f) = self.focused else {
            return false;
        };
        let Some(page) = self.page.as_ref() else {
            return false;
        };
        let tag = page.dom.tag(f).to_string();
        let input_type = page
            .dom
            .element(f)
            .and_then(|e| e.attr("type"))
            .unwrap_or("")
            .to_ascii_lowercase();
        match (tag.as_str(), input_type.as_str()) {
            ("input", "range" | "number") => {
                let el = page.dom.element(f);
                let amin = el
                    .and_then(|e| e.attr("min"))
                    .and_then(|v| v.trim().parse::<f32>().ok());
                let amax = el
                    .and_then(|e| e.attr("max"))
                    .and_then(|v| v.trim().parse::<f32>().ok());
                let (min, max) = if input_type == "range" {
                    (amin.unwrap_or(0.0), amax.unwrap_or(100.0))
                } else {
                    // Number honors its own min/max attrs; ±∞ when absent.
                    (
                        amin.unwrap_or(f32::NEG_INFINITY),
                        amax.unwrap_or(f32::INFINITY),
                    )
                };
                let step = el
                    .and_then(|e| e.attr("step"))
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0);
                let cur = el
                    .and_then(|e| e.attr("value"))
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(0.0);
                let v = (cur + dir as f32 * step).clamp(min.min(max), max.max(min));
                let s = if (v - v.round()).abs() < 1e-6 {
                    format!("{}", v.round() as i64)
                } else {
                    format!("{v}")
                };
                drop(page);
                if let Some(page) = self.page.as_mut() {
                    page.dom.set_attr(f, "value", &s);
                }
                #[cfg(feature = "page-js")]
                {
                    // Stepping commits a value: input + change.
                    let _ = self.dispatch_js_event_at("input", f, None);
                    let _ = self.dispatch_js_event_at("change", f, None);
                }
                self.relayout();
                true
            }
            ("select", _) => {
                let opts: Vec<NodeId> = page
                    .dom
                    .children(f)
                    .iter()
                    .copied()
                    .filter(|&c| page.dom.tag(c) == "option")
                    .collect();
                if opts.is_empty() {
                    return false;
                }
                let idx = opts
                    .iter()
                    .position(|&o| {
                        page.dom
                            .element(o)
                            .map(|e| e.attr("selected").is_some())
                            .unwrap_or(false)
                    })
                    .unwrap_or(0);
                let next = (idx as i32 + dir).clamp(0, opts.len() as i32 - 1) as usize;
                if next == idx {
                    return false;
                }
                drop(page);
                if let Some(page) = self.page.as_mut() {
                    for (i, &o) in opts.iter().enumerate() {
                        if i == next {
                            page.dom.set_attr(o, "selected", "");
                        } else {
                            page.dom.remove_attr(o, "selected");
                        }
                    }
                }
                #[cfg(feature = "page-js")]
                {
                    // Stepping the selection commits a value: input + change.
                    let _ = self.dispatch_js_event_at("input", f, None);
                    let _ = self.dispatch_js_event_at("change", f, None);
                }
                self.relayout();
                true
            }
            _ => false,
        }
    }

    /// The origin the S-1/S-2 gates judge this browser's requests by: the
    /// URL of the page currently displayed (session URL — for reopened QTSQ
    /// archives that is the `.qtsq` file URL, i.e. local, which is correct:
    /// the content came from local disk).
    fn gate_origin(&self) -> String {
        if self.page_url.is_empty() {
            self.history.last().cloned().unwrap_or_default()
        } else {
            self.page_url.clone()
        }
    }

    /// Publish the page a user sees when a link to a local file was
    /// blocked by the S-1 gate (SECURITY_AUDIT.md): a network document
    /// tried to reach `file://`. Goes through the normal error-page
    /// pipeline (history entry, location_changed, load_failed) and never
    /// reads the target.
    fn navigate_blocked_local(&mut self, target: &str) {
        let err = FetchError::LocalOrigin(kitae_net::ForbiddenLocalOrigin {
            target: target.to_string(),
            page_url: self.gate_origin(),
        });
        let url = normalize_input(target);
        let html = error_page_html(
            &url,
            &format!(
                "{} — Kitae blocks local file access from network pages.",
                err
            ),
        );
        kitae_net::set_page_origin(&url);
        release_qtsq_page_state(&self.page_url);
        self.load_rendered(
            html,
            &url,
            None,
            &url,
            Err(err),
            false,
            QtsqSigState::Unsigned,
        );
    }

    /// Fetch + engine pipeline + publish. The URL is already normalized.
    ///
    /// Phase 0: when the host opted into background fetches, http(s)
    /// document loads submit to the shared kitae_net pool and return at
    /// once — `poll_fetches` finishes them. Every other scheme needs no
    /// network thread (`about:`/`file:`/`data:` are local synthesis or
    /// disk reads) and keeps the inline path; POST form submits stay
    /// synchronous too.
    fn load(&mut self, url: &str) {
        let pw = self.qtsq_password.clone();
        let networked = url.starts_with("http://") || url.starts_with("https://");
        if self.background_fetch && networked {
            self.begin_background_load(url, pw.as_ref().map(|z| z.as_str()));
        } else {
            self.load_with_password(url, pw.as_ref().map(|z| z.as_str()));
        }
    }

    /// Everything a fresh top-level load resets before the pipeline runs:
    /// the loading flag plus the per-page interaction state — focus,
    /// hover, active, selection, caret, composition, the transport
    /// mirror, find matches, and any pending sign-in prompt (H6: it must
    /// never answer for a different page).
    fn begin_load_reset(&mut self) {
        self.host.set_loading(true);
        #[cfg(feature = "page-js")]
        {
            self.page_js = None; // a new page gets a fresh runtime
        }
        self.focused = None;
        self.hover = None;
        self.active = None;
        self.selection = None;
        self.caret = None;
        self.selection_range = None;
        self.composition = None;
        self.media_session = (false, 0.0);
        self.find_active = None;
        self.find_matches.clear();
        self.pending_auth = None;
    }

    /// [`Self::load`] with an explicit password override (the QTSQ unlock
    /// flow passes what the user just typed; the session default is the
    /// stored one).
    fn load_with_password(&mut self, url: &str, password: Option<&str>) {
        // Record the fetch context BEFORE the pipeline runs (S-1,
        // SECURITY_AUDIT.md): while this page's stylesheets, images and
        // fonts fetch, the local-origin gate judges them against the page
        // being loaded. Top-level loads themselves are host-trusted —
        // link-initiated navigations were already gated in `click_link`
        // (and `app_click`), and address-bar / back-forward / reload are
        // user actions, so a `file://` target typed or revisited works.
        kitae_net::set_page_origin(url);
        self.begin_load_reset();
        let old_page_for_media = self.page_url.clone();
        let ((html, base_url), doc_base, outcome, locked, sig) = fetch_result_state(url, password);
        if let Err(FetchError::InsecureRedirect(target)) = &outcome {
            let target = target.clone();
            self.request_navigation(target, PendingHistory::Push);
            return;
        }
        // H1: any download begun during the fetch belongs to this browser.
        for id in take_begun_downloads() {
            self.download_owned.insert(id);
        }
        // H6: a 401 challenge becomes a sign-in page and an `auth\t`
        // notification — the last challenge wins (one load presents one).
        if let Some(prompt) = take_auth_challenges().pop() {
            self.host.notify(&format!(
                "auth\t{}\t{}\t{}",
                prompt.scheme, prompt.realm, prompt.url
            ));
            self.pending_auth = Some(prompt);
        }
        release_qtsq_page_state(&old_page_for_media);
        clear_web_media_for(&old_page_for_media);
        self.load_rendered(
            html,
            &base_url,
            doc_base.as_deref(),
            url,
            outcome,
            locked,
            sig,
        );
        self.host.set_loading(false);
        // H6: offer credential fill when the freshly loaded page carries a
        // login form (password input + preceding text/email input). After
        // set_loading(false) — the embedder's loading-flag event must not
        // be delayed behind the autofill scan.
        self.notify_login_form();
    }

    /// Phase 0: submit an http(s) document fetch to the shared kitae_net
    /// pool and return at once. The load reset runs now — a load in
    /// flight must not keep the old page's interaction state — but the
    /// displayed page stays live until the completion replays the
    /// pipeline on [`Self::poll_fetches`]. Mid-flight link clicks gate
    /// against the page actually on screen: the fetch job carries the
    /// origin explicitly, and this thread's page-origin context still
    /// names the old page until the completion installs the new one.
    fn begin_background_load(&mut self, url: &str, password: Option<&str>) {
        self.begin_load_reset();
        let origin = self.gate_origin();
        let id = kitae_net::pool::submit(url, &origin, kitae_net::pool::FetchJobKind::CachedGet);
        self.pending_load = Some(PendingLoad {
            id,
            url: url.to_string(),
            password: password.map(|p| zeroize::Zeroizing::new(p.to_string())),
        });
    }

    /// Phase 0: apply one completed background load, if its response has
    /// landed. Hosts call this from their idle tick (the C-ABI engine
    /// every 2 ms; the Rust facade the same) so a pooled navigation
    /// finishes at the next tick after the response arrives. Returns
    /// whether a completion was applied. No-op while nothing is pending
    /// or the fetch is still in flight.
    ///
    /// From the response onward this replays the synchronous load's
    /// post-fetch sequence in full — download ownership, the 401 sign-in
    /// prompt, the S-1 origin handoff, QTSQ release, `load_rendered`,
    /// the loading flag and login-form autofill — so the visible outcome
    /// is byte-identical to a synchronous load of the same response.
    pub fn poll_fetches(&mut self) -> bool {
        let Some(pf) = self.pending_load.take() else {
            return false;
        };
        let mut landed = kitae_net::pool::take_results(&[pf.id]);
        let Some(outcome) = landed.pop() else {
            // Still fetching — leave it pending for the next tick.
            self.pending_load = Some(pf);
            return false;
        };
        // An InsecureRedirect handoff happens before everything else: the
        // HTTP answer refuses to serve and points at the secure target
        // (request_navigation re-pushes history — the one redirect case
        // that adds an entry), so nothing of the redirecting fetch is
        // applied.
        if let Err(FetchError::InsecureRedirect(target)) = &outcome.result {
            self.request_navigation(target.clone(), PendingHistory::Push);
            return true;
        }
        let old_page_for_media = self.page_url.clone();
        // H1: any download begun during the pooled fetch belongs to this
        // browser.
        for id in take_begun_downloads() {
            self.download_owned.insert(id);
        }
        // H6: a 401 challenge becomes a sign-in page and an `auth\t`
        // notification — the last challenge wins (one load presents one).
        if let Some(prompt) = take_auth_challenges().pop() {
            self.host.notify(&format!(
                "auth\t{}\t{}\t{}",
                prompt.scheme, prompt.realm, prompt.url
            ));
            self.pending_auth = Some(prompt);
        }
        // The render stage fetches this page's stylesheets, images and
        // fonts on THIS thread, so from here on the S-1 context must name
        // the page being loaded — as the synchronous path set it before
        // its fetch. Until now the thread deliberately kept the displayed
        // page's origin; the fetch ran origin-explicit on the pool worker.
        kitae_net::set_page_origin(&pf.url);
        release_qtsq_page_state(&old_page_for_media);
        clear_web_media_for(&old_page_for_media);
        let ((html, base_url), doc_base, render_outcome, locked, sig) = match outcome.result {
            Ok(resp) => {
                render_response_state(&pf.url, resp, pf.password.as_ref().map(|z| z.as_str()))
            }
            Err(e) => (
                (error_page_html(&pf.url, &e.to_string()), pf.url.clone()),
                None,
                Err(e),
                false,
                QtsqSigState::Unsigned,
            ),
        };
        self.load_rendered(
            html,
            &base_url,
            doc_base.as_deref(),
            &pf.url,
            render_outcome,
            locked,
            sig,
        );
        self.host.set_loading(false);
        self.notify_login_form();
        true
    }

    /// Phase 0: turn background fetches (pool-riding top-level http(s)
    /// loads finished on `poll_fetches`) on or off. Hosts set the mode at
    /// construction, before navigating; flipping it mid-navigation with a
    /// load already pooled leaves that load to complete on the next poll.
    pub fn set_background_fetch(&mut self, on: bool) {
        self.background_fetch = on;
    }

    /// SCRIPTING_DECISION.md: enable page JavaScript (feature `page-js`).
    /// Off by default; setting it applies to the NEXT load. The runtime
    /// lives one page: every navigation rebuilds or drops it.
    pub fn set_page_js(&mut self, on: bool) {
        self.page_js_enabled = on;
        #[cfg(feature = "page-js")]
        {
            if !on {
                self.page_js = None;
            }
        }
    }

    /// SCRIPTING_DECISION.md: hit-resolve a click against display-list
    /// rects (last-painted wins) and dispatch it to the page's JS
    /// listeners. True when a listener called `preventDefault()` — the
    /// engine's default action (links, summary toggles, form controls)
    /// must then stand down. A click that merely *fired* listeners does
    /// not own anything: default actions proceed.
    #[cfg(feature = "page-js")]
    fn dispatch_page_js_click(&mut self, x: f32, y: f32) -> bool {
        // Among every box and text run under the point, the smallest one
        // is the most intimate target (a parent's background also contains
        // the point, and rect order is not a reliable paint stack). Text
        // has no box of its own — runs contribute their line box so a
        // click on an anchor's label lands on the anchor, not the body.
        let mut target: Option<(f32, u32)> = None;
        if let Some(dl) = self.last_dl.as_ref() {
            for r in &dl.rects {
                if r.w > 0.0
                    && r.h > 0.0
                    && x >= r.x
                    && x <= r.x + r.w
                    && y >= r.y
                    && y <= r.y + r.h
                {
                    let area = r.w * r.h;
                    if target.is_none_or(|(best, _)| area <= best) {
                        target = Some((area, r.node));
                    }
                }
            }
            for t in &dl.texts {
                let (ascent, descent) =
                    self.measurer.metrics_family(t.size, t.monospace, &t.family);
                let (top, bot) = (t.y - ascent, t.y + descent);
                if y < top || y > bot {
                    continue;
                }
                let w = self.measurer.measure_family(
                    &t.text,
                    t.size,
                    t.weight,
                    t.italic,
                    t.monospace,
                    &t.family,
                );
                if x < t.x || x > t.x + w {
                    continue;
                }
                let area = w * (bot - top);
                if target.is_none_or(|(best, _)| area <= best) {
                    target = Some((area, t.node));
                }
            }
        }
        let Some(target) = target.map(|(_, node)| node) else {
            return false;
        };
        let Some(page) = self.page_js.as_mut() else {
            return false;
        };
        // Display-list rects carry NodeIds; JS dispatch speaks tokens.
        // They are different spaces (small colliding integers) — convert
        // through the page's mint, never cast.
        let token = page.token_for_node(NodeId(target));
        let mut page_struct = self.page.take();
        let (_, dirty, prevented) = match page_struct.as_mut() {
            Some(p) => page.dispatch_click(&mut p.dom, token, x, y),
            None => page.dispatch_click(&mut Dom::new(), token, x, y),
        };
        if let Some(p) = page_struct {
            self.page = Some(p);
        }
        if dirty {
            self.relayout();
        }
        prevented
    }

    /// page-js: one named event at `node`'s token through the page's
    /// capture/bubble walk; relayout when listeners mutated the DOM.
    /// True when a listener called preventDefault() (meaningful for
    /// cancelable types only).
    #[cfg(feature = "page-js")]
    fn dispatch_js_event_at(&mut self, type_name: &str, node: NodeId, data: Option<&str>) -> bool {
        let Some(js) = self.page_js.as_mut() else {
            return false;
        };
        let token = js.token_for_node(node);
        let mut page_struct = self.page.take();
        let (_, dirty, prevented) = match page_struct.as_mut() {
            Some(p) => js.dispatch_element_event(&mut p.dom, type_name, token, data),
            None => js.dispatch_element_event(&mut Dom::new(), type_name, token, data),
        };
        if let Some(p) = page_struct {
            self.page = Some(p);
        }
        if dirty {
            self.relayout();
        }
        prevented
    }

    /// page-js: an engine-side edit of `node`'s value announces itself —
    /// keydown/input/keyup reach the page's listeners. Informational: the
    /// engine owns the caret, so preventDefault cannot un-type in v0.
    #[cfg(feature = "page-js")]
    fn dispatch_js_edit_events(&mut self, node: NodeId, text: &str) {
        if !self.page_js_enabled || self.page_js.is_none() {
            return;
        }
        for (type_name, data) in [("keydown", None), ("input", Some(text)), ("keyup", None)] {
            let _ = self.dispatch_js_event_at(type_name, node, data);
        }
    }

    /// Drain the page's script event loop (timers, settled fetches,
    /// queued navigations). Hosts call this from their idle tick next to
    /// `poll_fetches`. Returns whether the DOM mutated (the page is then
    /// restyled, relaid out and republished).
    pub fn poll_page_js(&mut self) -> bool {
        #[cfg(feature = "page-js")]
        {
            if !self.page_js_enabled {
                return false;
            }
            let mut mutated = false;
            if let Some(url) = self.page_js.as_mut().and_then(|p| p.take_pending_nav()) {
                if url == "kitae:reload" {
                    self.reload();
                } else if url == "kitae:back" {
                    self.go(-1);
                } else if url == "kitae:forward" {
                    self.go(1);
                } else {
                    self.request_navigation(url, PendingHistory::Push);
                }
                return true; // navigation owns the next frames
            }
            // history.pushState/replaceState: URL-only state moves — the
            // page keeps its document, the address bar follows.
            if let Some((url, _replace)) =
                self.page_js.as_mut().and_then(|p| p.take_pending_history())
            {
                self.page_url = url.clone();
                self.host.location_changed(&url);
                return false;
            }
            // Script-requested scroll (window.scrollTo/scrollBy,
            // element.scrollIntoView) — clamped like every other scroll
            // path, and the host is told to repaint.
            if let Some(y) = self.page_js.as_ref().and_then(|p| p.take_pending_scroll()) {
                self.set_scroll(y);
                self.publish_page_js_view();
            }
            // Script-requested focus (element.focus()/blur()).
            if let Some(req) = self.page_js.as_ref().and_then(|p| p.take_pending_focus()) {
                let node =
                    req.and_then(|t| self.page_js.as_ref().and_then(|p| p.node_for_token(t)));
                self.focus_node_inner(node);
            }
            // A back/forward load completed: the NEW document's listeners
            // see `popstate` (state is null — a reload resets it).
            if let Some(target) = self.pending_popstate.clone() {
                if self.page_url == target && self.page_js.is_some() {
                    self.pending_popstate = None;
                    let mut page_struct = self.page.take();
                    if let Some(p) = page_struct.as_mut() {
                        if let Some(js) = self.page_js.as_mut() {
                            let _ = js.dispatch_doc_event(&mut p.dom, "popstate", 0.0, 0.0);
                        }
                    }
                    if let Some(p) = page_struct {
                        self.page = Some(p);
                    }
                    mutated = true;
                }
            }
            if self.page_js.is_some() && self.page.is_some() {
                let mut page = self.page.take().unwrap();
                mutated |= self.page_js.as_mut().unwrap().poll(&mut page.dom);
                self.page = Some(page);
            }
            for line in self
                .page_js
                .as_ref()
                .map(|p| p.take_console())
                .unwrap_or_default()
            {
                self.host.notify(&format!("console\t{line}"));
            }
            if mutated {
                self.relayout();
            }
            mutated
        }
        #[cfg(not(feature = "page-js"))]
        {
            false
        }
    }

    /// H6: the host's answer to an `auth\t` notification — remember the
    /// credentials for the challenged (host, realm) for this session
    /// (kitae_net's global provider then feeds them to fetch's automatic
    /// 401 retry) and re-fetch the challenged URL. `false` when no
    /// challenge is pending: nothing is stored, nothing reloads.
    pub fn set_auth(&mut self, username: &str, password: &str) -> bool {
        let Some(pending) = self.pending_auth.take() else {
            return false;
        };
        kitae_net::store_session_credentials(&pending.host, &pending.realm, username, password);
        let url = pending.url;
        self.load(&url);
        true
    }

    /// H6: the current page's forms as (form id, ordered inputs), an input
    /// being (element id, type, value). Elements without an `id` carry ""
    /// — they cannot be addressed by [`Browser::set_input_value`]. One
    /// entry per `form` element, document order.
    fn form_inventory(&self) -> Vec<(String, Vec<(String, String, String)>)> {
        let Some(page) = self.page.as_ref() else {
            return Vec::new();
        };
        let mut forms: Vec<NodeId> = Vec::new();
        if let Some(root) = page.dom.root() {
            page.dom.walk(root, &mut |n| {
                if page.dom.tag(n) == "form" {
                    forms.push(n);
                }
            });
        }
        let mut out = Vec::new();
        for form in forms {
            let fid = page
                .dom
                .element(form)
                .and_then(|e| e.attr("id"))
                .unwrap_or("")
                .to_string();
            let mut inputs: Vec<(String, String, String)> = Vec::new();
            page.dom.walk(form, &mut |n| {
                if n == form {
                    return;
                }
                if page.dom.tag(n) != "input" {
                    return;
                }
                let Some(el) = page.dom.element(n) else {
                    return;
                };
                inputs.push((
                    el.attr("id").unwrap_or("").to_string(),
                    el.attr("type").unwrap_or("text").to_ascii_lowercase(),
                    el.attr("value").unwrap_or("").to_string(),
                ));
            });
            out.push((fid, inputs));
        }
        out
    }

    /// H6: when the page carries a login form — a password input preceded
    /// by a text/email input in the same form, both carrying element ids —
    /// tell the host so it can offer to fill the pair from its store:
    /// `loginform\t{form_id}\t{user_id}\t{pass_id}`. Password-only forms
    /// (no addressable user field) stay silent.
    fn notify_login_form(&mut self) {
        for (form_id, inputs) in self.form_inventory() {
            let Some(pass_idx) = inputs.iter().position(|(_, t, _)| t == "password") else {
                continue;
            };
            let pass_id = inputs[pass_idx].0.clone();
            if pass_id.is_empty() {
                continue;
            }
            let user_id = inputs[..pass_idx]
                .iter()
                .rev()
                .find(|(i, t, _)| (t == "text" || t == "email") && !i.is_empty())
                .map(|(i, _, _)| i.clone());
            let Some(user_id) = user_id else {
                continue;
            };
            self.host
                .notify(&format!("loginform\t{form_id}\t{user_id}\t{pass_id}"));
        }
    }

    /// H6: a submitted form carrying a filled password input offers the
    /// host a save — `pwsave\t{origin}\t{user}\t{pass}` — the user name
    /// being the nearest preceding filled text/email input in the same
    /// form. Empty passwords never notify; blocked submits (validation)
    /// never reach here.
    fn notify_password_save(&mut self, form_id: &str) {
        let origin = origin_of(&self.page_url);
        for (fid, inputs) in self.form_inventory() {
            if fid != form_id {
                continue;
            }
            let Some((pass_idx, pass)) = inputs
                .iter()
                .enumerate()
                .find(|(_, (_, t, v))| t == "password" && !v.is_empty())
                .map(|(i, (_, _, v))| (i, v.clone()))
            else {
                return;
            };
            let user = inputs[..pass_idx]
                .iter()
                .rev()
                .find(|(_, t, v)| (t == "text" || t == "email") && !v.is_empty())
                .map(|(_, _, v)| v.clone())
                .unwrap_or_default();
            self.host
                .notify(&format!("pwsave\t{origin}\t{user}\t{pass}"));
            return;
        }
    }

    /// Store the session QTSQ password (FFI command 24 / embedder API) and,
    /// when a page is already open, reload it so a locked viewer page
    /// unlocks immediately. Before any navigation the password is just held
    /// for the next load.
    pub fn set_qtsq_password(&mut self, password: &str) {
        self.qtsq_password = Some(zeroize::Zeroizing::new(password.to_string()));
        if !self.page_url.is_empty() {
            let url = self.page_url.clone();
            self.load_with_password(&url, Some(password));
        }
    }

    /// H1: forward download-registry changes to the host as notifications —
    /// `download\t{id}\t{state}\t{received}\t{total}\t{filename}\t{path}`
    /// with state ∈ active|done|failed:<msg>, total empty when unknown. The
    /// engine's idle tick calls this every ~2 ms; progress for an active
    /// download is coalesced to roughly one notification per 100 ms so
    /// embedders can animate a progress bar without flooding the queue.
    /// Returns the number of notifications emitted.
    pub fn poll_downloads(&mut self) -> usize {
        let mut emitted = 0;
        for d in kitae_net::downloads_snapshot() {
            // Only narrate downloads this browser began — the registry is
            // process-global, every tab polls it.
            if !self.download_owned.contains(&d.id) {
                continue;
            }
            let state_name = match &d.state {
                kitae_net::DownloadState::Active => "active".to_string(),
                kitae_net::DownloadState::Done => "done".to_string(),
                kitae_net::DownloadState::Failed(e) => format!("failed:{e}"),
            };
            let now = std::time::Instant::now();
            let emit = match self.download_seen.get(&d.id) {
                None => true, // first sight: announce the download
                Some((last_received, last_state, last_emit)) => {
                    state_name != *last_state
                        || (d.received != *last_received
                            && now.duration_since(*last_emit).as_millis() >= 100)
                }
            };
            if emit {
                let total = d.total.map(|t| t.to_string()).unwrap_or_default();
                self.host.notify(&format!(
                    "download\t{}\t{}\t{}\t{}\t{}\t{}",
                    d.id,
                    state_name,
                    d.received,
                    total,
                    d.filename,
                    d.path.display()
                ));
                emitted += 1;
            }
            let seen = self
                .download_seen
                .entry(d.id)
                .or_insert((u64::MAX, String::new(), now));
            seen.0 = d.received;
            seen.1 = state_name;
            if emit {
                seen.2 = now;
            }
        }
        emitted
    }

    /// H1: the session's downloads (the app polls this for its panel; the
    /// engine only pushes changes through `notify`).
    pub fn downloads(&self) -> Vec<kitae_net::DownloadInfo> {
        kitae_net::downloads_snapshot()
    }

    /// H8: replace what the user sees with the crash page after a recovered
    /// panic. Deliberately minimal and history-free — `page_url` stays the
    /// crashed page's URL (the address bar and the crash page's Reload both
    /// refetch it), no network runs, and the render goes through the ordinary
    /// pipeline so hosts repaint normally. Called from the S-5 panic guard
    /// (ffi.rs), itself under a second `catch_unwind`: if the recovery render
    /// panics too, the notification already went out and the thread survives.
    pub fn show_crash_page(&mut self, what: &str) {
        let html = crash_page_html(what);
        let (dom, root, styles) =
            prepare_in_env(&html, "kitae:crash", self.viewport_w, &self.environment);
        self.page_title = "This tab crashed — Kitae".into();
        self.page = Some(Page {
            html,
            base_url: "kitae:crash".to_string(),
            resource_base_url: resource_base_url(&dom, root, "kitae:crash"),
            stylesheet_stats: StylesheetLoadStats::default(),
            dom,
            styles,
            interact_sensitive: true,
            frag_cache: FragmentCache::default(),
            frag_state: (None, None, None, Default::default()),
        });
        self.links.clear();
        self.focused = None;
        self.hover = None;
        self.active = None;
        self.selection = None;
        self.caret = None;
        self.selection_range = None;
        self.composition = None;
        self.find_active = None;
        self.find_matches.clear();
        self.relayout();
    }

    /// Save-Page-as-QTSQ (slice ②): write the current page as a
    /// device-encrypted, device-signed archive at `path`. The archive text
    /// (see `qtsq_archive_payload`) carries the page's absolute URL so a
    /// reopened snapshot resolves relative links against the original site.
    /// Reports the outcome through [`Host::save_finished`] — command 26 is
    /// fire-and-forget — and returns the archive byte length for tests.
    pub fn save_page_qtsq(&mut self, path: &str) -> Result<usize, String> {
        let result = self.save_page_qtsq_inner(path);
        self.host
            .save_finished(path, result.as_ref().err().map(|e| e.as_str()));
        result
    }

    fn save_page_qtsq_inner(&mut self, path: &str) -> Result<usize, String> {
        let Some(page) = self.page.as_ref() else {
            return Err("no page open".to_string());
        };
        let keys = kitae_qtsq::device_keys().map_err(|e| format!("device keys: {e}"))?;
        let base = if page.base_url.is_empty() {
            self.page_url.clone()
        } else {
            page.base_url.clone()
        };
        let payload = qtsq_archive_payload(&base, &self.page_title, &page.html);
        let bytes = kitae_qtsq::encode_text_secure(
            payload.as_bytes(),
            None,
            Some(&keys.kem_pk),
            Some(&keys.sign_sk),
        )
        .map_err(|e| format!("encode: {e}"))?;
        std::fs::write(path, &bytes).map_err(|e| format!("write: {e}"))?;
        Ok(bytes.len())
    }

    /// Publish already-fetched HTML: error pages, interstitial swaps, title,
    /// fonts and the relayout. Shared by GET navigation and POST submits.
    /// `qtsq_locked` marks a QTSQ viewer page that failed to decode as
    /// encrypted — published on the frame so hosts can prompt. `qtsq_sig`
    /// rides the frame too (v11); error pages force `Unsigned` regardless.
    /// `doc_base` is the saved-page archive's base override (slice ②): the
    /// document lays out and resolves against it while `url` stays the
    /// session URL (address bar, reload, history).
    fn load_rendered(
        &mut self,
        mut html: String,
        base_url: &str,
        doc_base: Option<&str>,
        url: &str,
        outcome: Result<(), FetchError>,
        qtsq_locked: bool,
        qtsq_sig: QtsqSigState,
    ) {
        self.qtsq_locked = qtsq_locked;
        self.qtsq_sig = if outcome.is_err() {
            QtsqSigState::Unsigned
        } else {
            qtsq_sig
        };
        if let Err(e) = outcome {
            let (dom, root, styles) =
                prepare_in_env(&html, url, self.viewport_w, &self.environment);
            self.page_url = url.to_string();
            self.host.location_changed(url);
            self.page_title = "Can't load this page — Kitae".into();
            self.page = Some(Page {
                html,
                base_url: url.to_string(),
                resource_base_url: resource_base_url(&dom, root, url),
                stylesheet_stats: StylesheetLoadStats::default(),
                dom,
                styles,
                interact_sensitive: true,
                frag_cache: FragmentCache::default(),
                frag_state: (None, None, None, Default::default()),
            });
            self.links.clear();
            self.last_error = Some(e.clone());
            self.media_session = (false, 0.0);
            clear_web_media_for(&self.page_url);
            self.host.load_failed(url, &e);
            self.relayout();
            self.host.set_loading(false);
            return;
        }

        let doc_base = doc_base.unwrap_or(base_url);
        // Phase 1 (P1c): `about:start` is no longer fetched HTML — it is a
        // kitae_ui component tree the engine mounts in-process. The DOM the
        // mount produces flows through the unchanged pipeline below.
        let mut app = None;
        #[cfg(feature = "live")]
        let mut live = None;
        let (mut dom, root, mut styles, mut title);
        if url == START_URL {
            let mut a = ui::LocalApp::new(start_page::start_page_app());
            let (d, body) = a.mount_dom();
            styles = self.styles_for_dom(&d, doc_base);
            dom = d;
            title = page_title(&dom, body).unwrap_or_else(|| "Start".into());
            root = Some(body);
            app = Some(a);
            html = start_page_html(); // canonical HTML copy for save/archive
        } else {
            if let Some((d, r, s, t)) = self.try_render_with_page_js(&html, doc_base) {
                dom = d;
                root = r;
                styles = s;
                title = t;
            } else {
                let (d, r, s) = prepare_in_env(&html, doc_base, self.viewport_w, &self.environment);
                dom = d;
                root = r;
                styles = s;
                title = root.and_then(|r| page_title(&dom, r)).unwrap_or_default();
                // Phase 2 (`live`): a page served by a kitae_live server marks
                // itself with <meta name="kitae-live">. The static page is
                // fully rendered above; the LiveApp opens the socket on its
                // first poll and applies hydration patches + streamed updates.
                #[cfg(feature = "live")]
                if self.live_enabled && root.is_some() && outcome.is_ok() && is_live_page(&dom) {
                    let body = find_element(&dom, root.unwrap(), "body");
                    if let Some(body) = body {
                        live = Some(live::LiveApp::new(url, body));
                    }
                }
            }
        }
        if let Some(root) = root {
            if needs_interstitial(&dom, root) {
                let interstitial = interstitial_html(url);
                let (d, r, s) =
                    prepare_in_env(&interstitial, doc_base, self.viewport_w, &self.environment);
                title = r
                    .and_then(|r| page_title(&d, r))
                    .unwrap_or_else(|| "JavaScript required — Kitae".into());
                dom = d;
                styles = s;
                html = interstitial;
            }
        }

        self.page_url = base_url.to_string();
        self.host.location_changed(&self.page_url);
        self.page_title = title;
        let resource_base_url = resource_base_url(&dom, dom.root(), doc_base);
        self.page = Some(Page {
            html,
            base_url: doc_base.to_string(),
            resource_base_url,
            stylesheet_stats: StylesheetLoadStats::default(),
            dom,
            styles,
            interact_sensitive: true,
            frag_cache: FragmentCache::default(),
            frag_state: (None, None, None, Default::default()),
        });
        self.app = app;
        #[cfg(feature = "live")]
        {
            self.live = live;
        }
        self.links.clear();
        self.last_error = None;
        self.media_session = (false, 0.0);
        clear_web_media_for(&self.page_url);
        self.ensure_page_fonts();
        self.relayout();
        self.host.set_loading(false);
    }

    /// SCRIPTING_DECISION.md: the page-js render path. Parses the page,
    /// runs its scripts (inline + external, document order) against the
    /// DOM shim, then computes styles/layout from the *mutated* tree.
    /// `None` hands control back to the no-JS path: page JS disabled, a
    /// frameless document, or a page whose scripts all failed while the
    /// body stayed empty — the honest interstitial policy then applies,
    /// exactly as with scripts declined.
    /// Without the `page-js` feature nothing can render scripts: always
    /// take the no-JS path.
    #[cfg(not(feature = "page-js"))]
    fn try_render_with_page_js(
        &mut self,
        _html: &str,
        _doc_base: &str,
    ) -> Option<(Dom, Option<NodeId>, Vec<(NodeId, ComputedStyle)>, String)> {
        None
    }

    #[cfg(feature = "page-js")]
    fn try_render_with_page_js(
        &mut self,
        html: &str,
        doc_base: &str,
    ) -> Option<(Dom, Option<NodeId>, Vec<(NodeId, ComputedStyle)>, String)> {
        if !self.page_js_enabled {
            return None;
        }
        let (mut dom, root) = parse(html);
        let root = root?;
        let mut page = pagejs::JsPage::new(doc_base, self.storage_session).ok()?;
        let stats = page.run_document_scripts(&mut dom, root, doc_base);
        // Scripts ran: with page JS on, the noscript fallback no longer
        // preempts the page (that content is for script-less engines).
        // Failed scripts + a body that stays text-thin keeps the honest
        // no-JS outcome: the caller's interstitial policy applies.
        if stats.scripts_ok + stats.modules_ok == 0 && needs_interstitial(&dom, root) {
            return None;
        }
        let styles = self.styles_for_dom(&dom, doc_base);
        let title = page_title(&dom, root).unwrap_or_default();
        self.page_js = Some(page);
        Some((dom, Some(root), styles, title))
    }

    /// Lay out the current document at the current viewport width and publish.
    /// Styles are recomputed from the live DOM (not the stored HTML) so
    /// `@media` tracks resizes and form-value mutations stay consistent.
    fn relayout(&mut self) {
        let Some(mut page) = self.page.take() else {
            return;
        };
        page.resource_base_url = resource_base_url(&page.dom, page.dom.root(), &page.base_url);
        // B3: build the engine once so the container-probe decision can
        // reuse it (styles_for_dom would construct a fresh engine).
        let (engine, stylesheet_stats) = self.style_engine_for_dom(&page.dom, &page.base_url);
        if stylesheet_stats.limit_exceeded() && page.stylesheet_stats != stylesheet_stats {
            self.host.notify(&format!(
                "stylesheet-limit\t{}\t{}\t{}\t{}",
                stylesheet_stats.external_loaded,
                stylesheet_stats.external_bytes,
                stylesheet_stats.skipped_count,
                stylesheet_stats.skipped_bytes
            ));
        }
        page.stylesheet_stats = stylesheet_stats;
        // I1: does any sheet rule key off hover/active/focus? Cached on the
        // page so set_hover/set_active can skip this whole relayout when
        // nothing could change.
        let interact_sensitive = engine.uses_interaction_state();
        let mut styles = engine.compute_with_state(&page.dom, &self.interaction_state());
        // B1: diff the fresh styles against the published ones — properties
        // with a matching `transition-*` entry start/retarget runs. Layout
        // then runs on styles stamped with eased values, but the stored
        // `page.styles` stay raw: stamping the stored ones would make every
        // tick's diff re-fire on its own animation.
        self.snapshot_transitions(&page.styles, &styles);
        let animating = !self.transitions.is_empty();
        let raw = if animating {
            Some(styles.clone())
        } else {
            None
        };
        self.apply_transition_overrides(&mut styles);
        // I2: cached fragments bake in interaction state and environment, so
        // any change since the last relayout (hover/active/focus moved,
        // prefers-* flipped) invalidates every entry. Transitions bypass and
        // clear outright — eased styles change per tick while node revisions
        // stand still — and container queries need the two-pass probe, whose
        // fragments would be styled against stale container widths.
        let state_now = (self.hover, self.active, self.focused, self.environment);
        if state_now != page.frag_state || animating {
            page.frag_cache.clear();
        }
        let uses_containers = engine.uses_containers();
        // `background-size: auto|cover|contain` needs each artwork's natural
        // size, which only the engine knows (it decodes the same URLs for
        // the image payload pass). Collect the sizes the sheets reference;
        // with no background images this is empty and the fast paths below
        // are unchanged.
        let background_sizes = background_image_sizes(&page, &styles);
        // A3/env: `<img media>` / `<picture><source media>` are resolved by
        // layout, so the page's own environment rides in — the same one the
        // sheets were filtered with, so its CSS and its images agree on which
        // medium (screen or print) they are for.
        let env = self.environment;
        let mut dl = if animating || uses_containers {
            if background_sizes.is_empty() {
                layout_in_env(
                    &page.dom,
                    &styles,
                    self.viewport_w,
                    self.viewport_h,
                    self.measurer.as_ref(),
                    &env,
                )
            } else {
                kitae_layout::layout_with_image_sizes_in_env(
                    &page.dom,
                    &styles,
                    self.viewport_w,
                    self.viewport_h,
                    self.measurer.as_ref(),
                    &background_sizes,
                    &env,
                )
            }
        } else if background_sizes.is_empty() {
            layout_cached_in_env(
                &page.dom,
                &styles,
                self.viewport_w,
                self.viewport_h,
                self.measurer.as_ref(),
                &mut page.frag_cache,
                &env,
            )
        } else {
            kitae_layout::layout_cached_with_image_sizes_in_env(
                &page.dom,
                &styles,
                self.viewport_w,
                self.viewport_h,
                self.measurer.as_ref(),
                &mut page.frag_cache,
                &background_sizes,
                &env,
            )
        };
        // B3: container queries need laid-out container widths, which only
        // a first layout can produce. When the sheets use `@container`/cq
        // units and the probe pass found containers, restyle against those
        // widths and lay out once more. Two passes, no fixpoint iteration —
        // styles that change container sizes don't re-converge (documented).
        if engine.uses_containers() && !dl.containers.is_empty() {
            let info: HashMap<u32, ContainerInfo> = dl
                .containers
                .iter()
                .map(|c| {
                    (
                        c.node,
                        ContainerInfo {
                            names: c.names.clone(),
                            width: c.w,
                            height: c.h,
                        },
                    )
                })
                .collect();
            styles = engine.compute_with_containers(&page.dom, &self.interaction_state(), &info);
            self.apply_transition_overrides(&mut styles);
            dl = layout_in_env(
                &page.dom,
                &styles,
                self.viewport_w,
                self.viewport_h,
                self.measurer.as_ref(),
                &env,
            );
        }
        // F1–F5: lazy web-media pipeline — decode audio, demux video, pull
        // captions once per media URL before the transport position reads.
        // The standard playback attributes ride along: `preload="none"` defers
        // the source fetch until playback, `autoplay` starts the transport,
        // and `poster` paints into the frame image below. `<source>` children
        // are the candidate list the resource is chosen from, and
        // `crossorigin` decides whether a fetch may happen at all.
        let base_url_for_media = page.resource_base_url.clone();
        let document_base = page.base_url.clone();
        let page_url_for_media = self.page_url.clone();
        let mut autoplay_url: Option<String> = None;
        // The URL each media item publishes, stamped back after the loop (the
        // loop borrows `dl` immutably). Normally that is the layout `src`
        // resolved, but for a driven element it is the resource it actually
        // plays — with `<source>` children that is the candidate that decoded,
        // and the frame's media section has to carry it for the host to decode
        // the same bytes the engine demuxed.
        let mut media_urls: Vec<(usize, String)> = Vec::new();
        for (index, m) in dl.media.iter().enumerate() {
            let node = NodeId(m.node);
            let is_audio = m.kind == kitae_layout::MediaKind::Audio;
            let attrs = media_attrs(page.dom.element(node));
            // The resource this element plays. It starts as the element's own
            // `src` (what an element the engine only paints publishes) and is
            // replaced below by the selection for a driven one.
            let mut url = resolve(&base_url_for_media, &m.src);
            if !attrs.fetch_allowed() {
                // `crossorigin="use-credentials"` cannot be honored and must
                // not be approximated (see `MediaAttrs::fetch_allowed`): no
                // fetch, no decode, and no URL for the host to chase.
                media_urls.push((index, String::new()));
            } else if m.controls {
                // `<source>` children first (media conditions honored, I6
                // mixed-content gated per candidate), the element's own `src`
                // last — HTML's resource selection order, minus the `type`
                // pre-filter the engine has no table for.
                let candidates: Vec<String> = media_candidates(
                    &page.dom,
                    Some(node),
                    self.viewport_w,
                    self.viewport_h,
                    &self.environment,
                )
                .into_iter()
                .filter_map(|c| resolve(&base_url_for_media, &c))
                .filter(|u| kitae_net::mixed_content_allowed(&document_base, u))
                .collect();
                // The first claimable media item is the transport, so its
                // attributes and candidates are the ones the transport path
                // honors (same first-wins rule as `web_page_media`).
                web_page_transport()
                    .lock()
                    .unwrap()
                    .entry(page_url_for_media.clone())
                    .or_insert_with(|| TransportMedia {
                        attrs: attrs.clone(),
                        is_audio,
                        candidates: candidates.clone(),
                        node: m.node,
                    });
                // `autoplay` overrides `preload`: playing needs the data.
                url = if attrs.defers_load() && !attrs.autoplay_allowed(!is_audio) {
                    // Associated but not fetched: the resource selection
                    // algorithm waits for `media_play`
                    // (`load_deferred_transport` runs it then).
                    candidates
                        .first()
                        .cloned()
                        .inspect(|u| claim_web_media(&page_url_for_media, u, m.node))
                } else {
                    select_web_media(&page_url_for_media, m.node, &candidates, is_audio)
                };
                media_urls.push((index, url.clone().unwrap_or_default()));
                if let Some(loaded) = url.as_ref() {
                    if attrs.autoplay_allowed(!is_audio) {
                        autoplay_url.get_or_insert(loaded.clone());
                    }
                }
            }
            if m.kind == kitae_layout::MediaKind::Video {
                // Captions key off the URL the element plays, so a `<track>`
                // sidecar lands on the same resource the decoder got.
                if let Some(video_url) = url.as_deref() {
                    ensure_web_captions(video_url, &page.dom, node, &base_url_for_media);
                }
            }
        }
        for (index, media_url) in media_urls {
            dl.media[index].src = media_url;
        }
        // Autoplay (muted, or silent video): start the transport once the
        // source is loaded. Only from a clean session — a user pause or seek
        // past position 0 is not autoplay.
        if let Some(target) = autoplay_url {
            let start = self
                .media_state()
                .filter(|m| m.url == target && !m.playing && m.position == 0.0)
                .map(|m| m.position);
            if let Some(position) = start {
                self.media_session = (true, position);
            }
        }
        // J5: the transport position the `<video>` frame image steps to.
        let media_pos = self.media_state().map(|m| (m.url, m.position));
        // J3: resolve QTSQ image subresources to payload ids. The renderer
        // turns the id into pixels through `qtsq_raster_for_id`; anything
        // unresolved keeps `payload: None` (placeholder box). `\0media:{id}`
        // sentinels (the engine's internal frame image of a DisplayMedia)
        // step the cached video decoder to the transport position instead,
        // and fall back to the element's `poster` while no video data
        // exists.
        for im in &mut dl.images {
            // A canvas re-stamps on every relayout: the sentinel never
            // changes, but the registered pixels do (page JS drew again).
            if im.src.starts_with("\u{0}canvas:") {
                im.payload = qtsq_raster_for_url(&im.src).map(|r| r.id);
                continue;
            }
            if im.payload.is_none() {
                if let Some(id) = im.src.strip_prefix("\u{0}media:") {
                    // The sentinel id pairs with `dl.media[id]`, whose src
                    // names the video document to decode and whose poster is
                    // the still shown until a frame arrives.
                    let item = id.parse::<usize>().ok().and_then(|i| dl.media.get(i));
                    if let Some(item) = item {
                        let src = item.src.clone();
                        let url =
                            resolve(&page.resource_base_url, &src).unwrap_or_else(|| src.clone());
                        let pos = media_pos
                            .as_ref()
                            .filter(|(u, _)| u == &url)
                            .map(|(_, p)| *p)
                            .unwrap_or(0.0);
                        if let Some(raster) = video_frame_raster(
                            &url,
                            pos,
                            &item.poster,
                            &base_url_for_media,
                            &document_base,
                        ) {
                            im.payload = Some(raster.id);
                        }
                    }
                    continue;
                }
                let abs =
                    resolve(&page.resource_base_url, &im.src).unwrap_or_else(|| im.src.clone());
                // I6: mixed-content policy — no plaintext subresource off an
                // https page; the placeholder box stays.
                if !kitae_net::mixed_content_allowed(&document_base, &abs) {
                    continue;
                }
                if let Some(raster) = ensure_qtsq_raster(&abs)
                    .or_else(|| ensure_svg_raster(&abs, im.w, im.h))
                    .or_else(|| ensure_web_raster(&abs))
                {
                    im.payload = Some(raster.id);
                    im.animated = qtsq_animation_for_id(raster.id).is_some();
                }
            }
        }
        self.content_height = dl.content_height;
        self.links = dl.links.clone();
        self.scroll_y = self
            .scroll_y
            .min((self.content_height - self.viewport_h).max(0.0));
        self.repaints += 1;
        if let Some(f) = self.focused {
            if !self.is_focusable_node(&page.dom, &styles, f) {
                self.focused = None;
                self.caret = None;
                self.selection_range = None;
                self.composition = None;
                self.selection = None;
            }
        }
        // A6: a modal dialog claims focus — on load (when nothing is
        // focused) or when focus sits outside the dialog, the first
        // focusable inside it takes focus. Runs before the AX build so the
        // published tree sees the final focus.
        if let Some(root) = page.dom.root() {
            if let Some(dialog) = open_dialog(&page.dom, root) {
                let claimed = self.focused.is_some_and(|f| within(&page.dom, f, dialog));
                if !claimed {
                    let mut first = None;
                    page.dom.walk(dialog, &mut |n| {
                        if first.is_none() && self.is_focusable_node(&page.dom, &styles, n) {
                            first = Some(n);
                        }
                    });
                    self.selection_range = None;
                    self.composition = None;
                    self.selection = None;
                    self.focused = first;
                    self.sync_caret();
                }
            }
        }
        // A10 finish: the focused control's validation bubble paints last so
        // it sits on top of everything (see `paint_error_bubble`).
        if let Some(f) = self.focused {
            paint_error_bubble(
                &mut dl,
                &page.dom,
                f,
                self.viewport_w,
                Some(self.measurer.as_ref()),
            );
        }
        // Find matches + AX tree resolve against the new list, before the
        // frame that carries them is published.
        self.refresh_find_for(&dl);
        let (ax_tree, ax_focused, ax_ids) = build_ax_tree(
            &page.dom,
            &self.page_title.clone(),
            self.focused,
            &dl,
            &styles,
            self.measurer.as_ref(),
        );
        self.ax_tree = ax_tree;
        self.ax_focused = ax_focused;
        self.ax_ids = ax_ids;
        // B1: raw styles back into the page (the layout input above was the
        // eased copy); `styles` itself still holds them when not animating.
        self.page = Some(Page {
            styles: raw.unwrap_or(styles),
            interact_sensitive,
            frag_state: state_now,
            ..page
        });
        self.sync_display_media_state(&mut dl);
        self.host.publish(&self.state(), &self.decorate_caret(&dl));
        self.last_dl = Some(dl);
        // page-js geometry: publish node boxes + viewport so scripts can
        // answer getBoundingClientRect/innerWidth from the rendered truth.
        #[cfg(feature = "page-js")]
        if let Some(js) = self.page_js.as_ref() {
            if let Some(dl) = self.last_dl.as_ref() {
                let mut rects: std::collections::HashMap<u32, (f32, f32, f32, f32)> =
                    std::collections::HashMap::new();
                for r in &dl.rects {
                    let entry = rects.entry(r.node).or_insert((r.x, r.y, r.w, r.h));
                    let x0 = entry.0.min(r.x);
                    let y0 = entry.1.min(r.y);
                    let x1 = (entry.0 + entry.2).max(r.x + r.w);
                    let y1 = (entry.1 + entry.3).max(r.y + r.h);
                    *entry = (x0, y0, x1 - x0, y1 - y0);
                }
                js.set_geometry(rects, (self.viewport_w, self.viewport_h));
                // JS-3 read paths: scroll / activeElement / selection and
                // the computed-style text ride the same publish cadence.
                self.publish_page_js_view();
                if let Some(js) = self.page_js.as_ref() {
                    let computed = self
                        .page
                        .as_ref()
                        .map(|p| {
                            p.styles
                                .iter()
                                .map(|(n, s)| (n.0, pagejs::computed_css(s)))
                                .collect::<std::collections::HashMap<u32, String>>()
                        })
                        .unwrap_or_default();
                    js.set_computed(computed);
                }
            }
        }
    }

    fn styles_for_dom(&self, dom: &Dom, base_url: &str) -> Vec<(NodeId, ComputedStyle)> {
        let (engine, _) = self.style_engine_for_dom(dom, base_url);
        engine.compute_with_state(dom, &self.interaction_state())
    }

    /// Dynamic pseudo-class state for the style engine (slice N6), and the
    /// state an element query must pass so `:hover`/`:focus` selectors
    /// answer truthfully.
    pub fn interaction_state(&self) -> kitae_style::InteractionState {
        kitae_style::InteractionState {
            hover: self.hover,
            active: self.active,
            focused: self.focused,
        }
    }

    // -- CSS transitions (B1) -------------------------------------------------

    /// The properties the engine can transition (layout-safe, interpolatable:
    /// opacity/color/background-color/transform). Everything else changes
    /// instantly even under a `transition` declaration.
    const TRANSITIONABLE: [&'static str; 4] = ["opacity", "color", "background-color", "transform"];

    /// Diff the freshly computed styles against the published ones and
    /// start/retarget transition runs for changed properties that the new
    /// element's `transition-*` list matches. Runs whose node left the
    /// document are pruned. Called from [`Browser::relayout`] with the raw
    /// (un-overridden) styles; the published `page.styles` stay raw too, so
    /// diffs between ticks see only real computed changes.
    fn snapshot_transitions(
        &mut self,
        old: &[(NodeId, ComputedStyle)],
        new: &[(NodeId, ComputedStyle)],
    ) {
        if old.is_empty()
            || self.transitions.is_empty() && new.iter().all(|(_, s)| s.transitions.is_empty())
        {
            return;
        }
        let new_index: HashMap<NodeId, &ComputedStyle> = new.iter().map(|(n, s)| (*n, s)).collect();
        let live: std::collections::HashSet<u32> = new.iter().map(|(n, _)| n.0).collect();
        // Prune runs for nodes no longer in the document.
        self.transitions.retain(|n, _| live.contains(&n.0));
        for (nid, old_style) in old {
            let Some(new_style) = new_index.get(nid) else {
                continue;
            };
            if new_style.transitions.is_empty() {
                continue;
            }
            for prop in Self::TRANSITIONABLE {
                let now = self.transition_clock;
                let (Some(a), Some(b)) = (
                    serialize_style_value(old_style, prop),
                    serialize_style_value(new_style, prop),
                ) else {
                    continue;
                };
                if a == b {
                    continue;
                }
                // The destination element's list decides; later entries win
                // (cascade order). Zero-duration entries change instantly.
                let Some(entry) = new_style
                    .transitions
                    .iter()
                    .rev()
                    .find(|t| transition_matches(&t.property, prop))
                else {
                    continue;
                };
                if entry.duration_secs <= 0.0 {
                    continue;
                }
                let interpolatable = interpolate_style_value(prop, &a, &b, 0.5).is_some();
                let live = self
                    .transitions
                    .get_mut(nid)
                    .and_then(|rs| rs.iter_mut().find(|r| r.property == prop));
                match live {
                    // Retarget mid-flight: continue from the current value.
                    Some(run) => {
                        run.from = run.sample(now).unwrap_or_else(|| run.to.clone());
                        run.to = b;
                        run.start = now;
                        run.duration = entry.duration_secs;
                        run.timing = entry.timing;
                        run.discrete = !interpolatable;
                    }
                    None => {
                        let run = RunningTransition {
                            property: prop.to_string(),
                            from: a,
                            to: b,
                            start: now,
                            duration: entry.duration_secs,
                            timing: entry.timing,
                            discrete: !interpolatable,
                        };
                        self.transitions.entry(*nid).or_default().push(run);
                    }
                }
            }
        }
    }

    /// Stamp eased transition values onto the freshly computed styles for
    /// layout. Both `page.styles` (raw) and layout input (overridden) stay
    /// separate so the next diff compares real values, not animation frames.
    fn apply_transition_overrides(&self, styles: &mut [(NodeId, ComputedStyle)]) {
        if self.transitions.is_empty() {
            return;
        }
        let now = self.transition_clock;
        for (nid, style) in styles.iter_mut() {
            let Some(runs) = self.transitions.get(nid) else {
                continue;
            };
            for run in runs {
                if let Some(value) = run.sample(now) {
                    apply_style_value(style, &run.property, &value);
                }
            }
        }
    }

    /// Advance transitions to `t_secs` and republish if anything moved (or
    /// just finished — its final relayout releases the override so the last
    /// frame carries the true end state). Returns whether transitions are
    /// still running; embedders tick while it stays true.
    pub fn tick_transitions_at(&mut self, t_secs: f32) -> bool {
        self.transition_clock = t_secs;
        if self.transitions.is_empty() {
            return false;
        }
        // Drop finished runs, then relayout once with the survivors'
        // overrides (and finished properties back at their end values).
        let before = self.transitions.len();
        self.transitions.retain(|_, runs| {
            runs.retain(|r| (t_secs - r.start) < r.duration && r.duration > 0.0);
            !runs.is_empty()
        });
        if before > 0 || !self.transitions.is_empty() {
            self.relayout();
        }
        !self.transitions.is_empty()
    }

    /// Tick against the engine's own monotonic clock — the embedder-loop
    /// convenience form. See [`Browser::tick_transitions_at`].
    pub fn tick_transitions(&mut self) -> bool {
        let t = self.transition_epoch.elapsed().as_secs_f32();
        self.tick_transitions_at(t)
    }

    /// Set the hover node (`None` clears). Restyles + relayouts when
    /// changed — skipped entirely on pages where no sheet rule keys off
    /// hover (I1); still returns true on change. Mouse-coordinate
    /// hit-testing lives with the host (it owns geometry); the id-addressed
    /// contract here is what the accessibility tree (Phase P) resolves
    /// points through.
    pub fn set_hover(&mut self, node: Option<NodeId>) -> bool {
        if self.hover == node {
            return false;
        }
        self.hover = node;
        if self
            .page
            .as_ref()
            .map(|p| p.interact_sensitive)
            .unwrap_or(false)
        {
            self.relayout();
        }
        true
    }

    /// Set the active (pressed) node (`None` clears). Restyles like hover;
    /// on pages without `:hover`/`:active`/`:focus` rules the relayout is
    /// skipped (I1).
    pub fn set_active(&mut self, node: Option<NodeId>) -> bool {
        if self.active == node {
            return false;
        }
        self.active = node;
        if self
            .page
            .as_ref()
            .map(|p| p.interact_sensitive)
            .unwrap_or(false)
        {
            self.relayout();
        }
        true
    }

    /// Hover by element id (`None`/empty clears). Returns false when the id
    /// is missing (state untouched).
    pub fn set_hover_id(&mut self, element_id: Option<&str>) -> bool {
        match element_id.filter(|s| !s.is_empty()) {
            None => self.set_hover(None),
            Some(id) => match self.page.as_ref().and_then(|p| p.dom.get_element_by_id(id)) {
                Some(n) => self.set_hover(Some(n)),
                None => false,
            },
        }
    }

    /// Active by element id (`None`/empty clears).
    pub fn set_active_id(&mut self, element_id: Option<&str>) -> bool {
        match element_id.filter(|s| !s.is_empty()) {
            None => self.set_active(None),
            Some(id) => match self.page.as_ref().and_then(|p| p.dom.get_element_by_id(id)) {
                Some(n) => self.set_active(Some(n)),
                None => false,
            },
        }
    }

    /// Update the embedder environment (Q6): what `@media (prefers-…)`
    /// conditions see. Restyles + relayouts when anything changed; returns
    /// true on change. No reload — pages re-render in place.
    pub fn set_environment(&mut self, env: EnvironmentState) -> bool {
        if self.environment == env {
            return false;
        }
        self.environment = env;
        self.relayout();
        true
    }

    /// Update only the `prefers-*` half of the environment, leaving the media
    /// type alone. This is what the wire's `KITAE_CMD_SET_ENVIRONMENT` does:
    /// the command carries the scheme and two feature bits, so an appearance
    /// change must not silently drop a page that is laid out for print back to
    /// screen. Returns true on change (and republishes, like
    /// [`Self::set_environment`]).
    pub fn set_preferences(&mut self, env: &EnvironmentState) -> bool {
        if self.environment.color_scheme == env.color_scheme
            && self.environment.reduced_motion == env.reduced_motion
            && self.environment.contrast_more == env.contrast_more
        {
            return false;
        }
        self.environment.color_scheme = env.color_scheme;
        self.environment.reduced_motion = env.reduced_motion;
        self.environment.contrast_more = env.contrast_more;
        self.relayout();
        true
    }

    /// H3: switch the page's media environment between screen and print.
    /// `@media print` rules apply (and `@media screen` ones stop applying),
    /// the `prefers-*` features answer as the target device, and the page is
    /// re-laid-out and re-published for the new environment — no reload, no
    /// refetch. Returns true on change; setting the environment it is already
    /// in is a no-op. The choice survives navigation, like
    /// [`Self::set_environment`], so a host can hold print media across the
    /// pages it renders to paper.
    pub fn set_media_type(&mut self, media: MediaType) -> bool {
        if self.environment.media_type == media {
            return false;
        }
        self.environment.media_type = media;
        self.relayout();
        true
    }

    // ── kitae_ui local runtime (Phase 1, P1c) ─────────────────────────────
    //
    // A mounted app re-renders on demand (a handler ran, the embedder asked);
    // the diff's patch ops land on the DOM and the display list is spliced
    // incrementally: only the dirty top-level box is re-laid-out, everything
    // after it shifts by the height delta. When anything is uncertain the
    // splice falls back to a full relayout — correctness first, speed when
    // provable.

    /// Run the app's `click` handlers for a hit at page coordinates. True
    /// when a handler ran (the click is consumed); false lets the normal
    /// link path take it.
    fn app_click(&mut self, x: f32, y: f32) -> bool {
        let Some(app) = self.app.as_ref() else {
            return false;
        };
        let Some(dl) = self.last_dl.as_ref() else {
            return false;
        };
        let Some(page) = self.page.as_ref() else {
            return false;
        };
        // Hit-test the painted items back-to-front: texts, then rects and
        // images (the last item covering the point wins, matching paint
        // order). Each item knows the DOM node that painted it (v7).
        let hit = |node: u32| {
            let mut n = NodeId(node);
            loop {
                if let Some(r) = app.ref_for(n) {
                    if let Some(hs) = app.handlers().get(&r) {
                        if hs.iter().any(|(ev, _)| ev == "click") {
                            return Some(r);
                        }
                    }
                }
                match page.dom.node(n).parent {
                    Some(p) => n = p,
                    None => return None,
                }
            }
        };
        let found = dl
            .texts
            .iter()
            .rev()
            .find(|t| {
                (x >= t.x
                    && x <= t.x
                        + self
                            .measurer
                            .measure(&t.text, t.size, t.weight, t.italic, t.monospace))
                    && (y >= t.y - t.size * 0.75 && y <= t.y + t.size * 0.25)
            })
            .and_then(|t| hit(t.node))
            .or_else(|| {
                dl.rects
                    .iter()
                    .rev()
                    .find(|r| x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h)
                    .and_then(|r| hit(r.node))
            })
            .or_else(|| {
                dl.images
                    .iter()
                    .rev()
                    .find(|i| x >= i.x && x <= i.x + i.w && y >= i.y && y <= i.y + i.h)
                    .and_then(|i| hit(i.node))
            });
        let Some(r) = found else {
            return false;
        };
        let Some(hs) = app.handlers().get(&r) else {
            return false;
        };
        let Some((_, handler)) = hs.iter().find(|(ev, _)| ev == "click") else {
            return false;
        };
        let handler = handler.clone();
        handler(kitae_ui::vnode::Event {
            name: "click".into(),
            value: None,
        });
        self.app_render();
        true
    }

    /// Re-render the mounted app: diff → apply → restyle when needed →
    /// splice the display list (fall back to a full relayout).
    fn app_render(&mut self) {
        let Some(mut app) = self.app.take() else {
            return;
        };
        let ops = app.update();
        let restyle_needed = !ui::LocalApp::is_text_only(&ops);
        let Some(mut page) = self.page.take() else {
            self.app = Some(app);
            return;
        };
        if !ops.is_empty() {
            app.apply(&mut page.dom, ops.clone());
        }
        page.resource_base_url = resource_base_url(&page.dom, page.dom.root(), &page.base_url);
        if restyle_needed {
            page.styles = self.styles_for_dom(&page.dom, &page.base_url);
        }
        self.app = Some(app);
        self.page = Some(page);
        self.splice_publish(&ops);
    }

    // ── kitae_live Live client (Phase 2, `live` feature) ────────────────
    //
    // The live half mirrors the local runtime: patches land on the DOM
    // through the same apply machinery, restyle when structure changed,
    // splice when provable, full relayout otherwise. The difference is
    // where the patches come from (the socket) and where handlers run
    // (the server).

    /// One non-blocking Live poll: drain ready KTLV frames, apply patch
    /// ops, publish when anything landed. Call between commands (the
    /// engine loop does; embedders may thread it into any idle path).
    #[cfg(feature = "live")]
    pub fn poll_live(&mut self) {
        let Some(mut live) = self.live.take() else {
            return;
        };
        let Some(mut page) = self.page.take() else {
            self.live = Some(live);
            return;
        };
        let ops = live.poll(&mut page.dom);
        // A server-side navigation replaces the whole page.
        if let Some(url) = live.take_redirect() {
            self.live = Some(live);
            self.page = Some(page);
            self.navigate(&url);
            return;
        }
        if ops.is_empty() {
            self.live = Some(live);
            self.page = Some(page);
            return;
        }
        // Hydration rebuilt the body wholesale: the old list's items belong
        // to detached nodes, so only a full relayout is provably right.
        let hydrated = live.take_hydrate_landed();
        let restyle_needed = !ui::LocalApp::is_text_only(&ops);
        page.resource_base_url = resource_base_url(&page.dom, page.dom.root(), &page.base_url);
        if restyle_needed {
            page.styles = self.styles_for_dom(&page.dom, &page.base_url);
        }
        self.live = Some(live);
        self.page = Some(page);
        if hydrated {
            self.relayout();
        } else {
            self.splice_publish(&ops);
        }
    }

    /// Is the current page live-mounted and connected? (Embedder UI.)
    #[cfg(feature = "live")]
    pub fn is_live_connected(&self) -> bool {
        self.live.as_ref().is_some_and(live::LiveApp::is_connected)
    }

    /// The live session id, when one was assigned (embedder UI / tests).
    #[cfg(feature = "live")]
    pub fn live_session_id(&self) -> Option<String> {
        self.live
            .as_ref()
            .and_then(live::LiveApp::session_id)
            .map(str::to_string)
    }

    /// Drop the live socket deliberately; the next poll reconnects with
    /// `resume <id>` (embedder backgrounding, network switches, tests).
    #[cfg(feature = "live")]
    pub fn live_disconnect(&mut self) {
        if let Some(l) = self.live.as_mut() {
            l.disconnect();
        }
    }

    /// Master switch for Live sockets: off = pages render statically even
    /// when the server offers a socket (the plan's "socket disabled"
    /// acceptance path). Dropping mid-session discards the mount.
    #[cfg(feature = "live")]
    pub fn set_live_enabled(&mut self, on: bool) {
        self.live_enabled = on;
        if !on {
            self.live = None;
        }
    }

    /// Whether Live sockets may open (default true).
    #[cfg(feature = "live")]
    pub fn live_enabled(&self) -> bool {
        self.live_enabled
    }

    // ── node handles: imperative element access ─────────────────────────

    /// Resolve a [`NodeHandle`](kitae_ui::vnode::NodeHandle) to its engine
    /// node (the mounted app OR a live page's hydrated tree).
    fn handle_node(&self, handle: u64) -> Option<NodeId> {
        if let Some(app) = self.app.as_ref() {
            if let Some(n) = app.node_for_handle(handle) {
                return Some(n);
            }
        }
        #[cfg(feature = "live")]
        if let Some(l) = self.live.as_ref() {
            if let Some(n) = l.app_handle_node(handle) {
                return Some(n);
            }
        }
        None
    }

    /// The measured on-screen rect of a `node_ref:`-marked element:
    /// the union of everything it paints (rects and text runs), in page
    /// space. `None` when the handle is unknown or paints nothing.
    pub fn node_rect(&self, handle: u64) -> Option<(f32, f32, f32, f32)> {
        let node = self.handle_node(handle)?.0;
        let dl = self.last_dl.as_ref()?;
        let mut bounds: Option<(f32, f32, f32, f32)> = None;
        let mut grow = |x: f32, y: f32, w: f32, h: f32| {
            let (x1, y1) = (x + w, y + h);
            match bounds.as_mut() {
                None => bounds = Some((x, y, w, h)),
                Some((bx, by, bw, bh)) => {
                    let nx0 = (*bx).min(x);
                    let ny0 = (*by).min(y);
                    let nx1 = (*bx + *bw).max(x1);
                    let ny1 = (*by + *bh).max(y1);
                    *bx = nx0;
                    *by = ny0;
                    *bw = nx1 - nx0;
                    *bh = ny1 - ny0;
                }
            }
        };
        for r in &dl.rects {
            if r.node == node {
                grow(r.x, r.y, r.w, r.h);
            }
        }
        for t in &dl.texts {
            if t.node == node {
                let w = self
                    .measurer
                    .measure(&t.text, t.size, t.weight, t.italic, t.monospace);
                grow(t.x, t.y - t.size * 0.75, w, t.size);
            }
        }
        bounds
    }

    /// Give a `node_ref:`-marked element keyboard focus (when focusable).
    /// True when focus moved.
    pub fn focus_node(&mut self, handle: u64) -> bool {
        let Some(node) = self.handle_node(handle) else {
            return false;
        };
        self.focus_node_inner(Some(node))
    }

    /// Focus `node` (None = blur), shared by the handle path above and
    /// page-js `element.focus()`/`blur()` requests. False when the node is
    /// not focusable (or there is no page) — blur always applies.
    fn focus_node_inner(&mut self, node: Option<NodeId>) -> bool {
        let Some(page) = self.page.as_ref() else {
            return false;
        };
        if let Some(n) = node {
            if !self.is_focusable_node(&page.dom, &page.styles, n) {
                return false;
            }
        }
        let canceled = self.reset_editing_for_focus();
        self.focused = node;
        self.sync_caret();
        self.refresh_ax_focus();
        if canceled {
            self.relayout();
        } else {
            self.publish_current();
        }
        true
    }

    /// Scroll a `node_ref:`-marked element into view: minimal scroll that
    /// brings its rect inside the viewport. True when the scroll moved.
    pub fn scroll_node_into_view(&mut self, handle: u64) -> bool {
        let Some((x, y, _w, h)) = self.node_rect(handle) else {
            return false;
        };
        let _ = x;
        let before = self.scroll_y;
        let max = (self.content_height - self.viewport_h).max(0.0);
        if y < self.scroll_y {
            self.set_scroll(y.min(max));
        } else if y + h > self.scroll_y + self.viewport_h {
            self.set_scroll((y + h - self.viewport_h).min(max));
        }
        (self.scroll_y - before).abs() > 0.5
    }

    /// Route a click on a live page to the server: find the hit-tested
    /// node's ref, confirm the server marked it handler-bearing, send the
    /// Event frame. True when consumed.
    #[cfg(feature = "live")]
    fn live_click(&mut self, x: f32, y: f32) -> bool {
        let Some(live) = self.live.as_ref() else {
            return false;
        };
        let Some(dl) = self.last_dl.as_ref() else {
            return false;
        };
        let Some(page) = self.page.as_ref() else {
            return false;
        };
        // Same hit-test as app_click: walk ancestors from the painted item
        // to a ref the server marked handler-bearing.
        let hit = |node: u32| -> Option<u64> {
            let mut n = NodeId(node);
            loop {
                if let Some(r) = live.ref_for(n) {
                    if live.is_handler_ref(r) {
                        return Some(r);
                    }
                }
                match page.dom.node(n).parent {
                    Some(p) => n = p,
                    None => return None,
                }
            }
        };
        let found = dl
            .texts
            .iter()
            .rev()
            .find(|t| {
                (x >= t.x
                    && x <= t.x
                        + self
                            .measurer
                            .measure(&t.text, t.size, t.weight, t.italic, t.monospace))
                    && (y >= t.y - t.size * 0.75 && y <= t.y + t.size * 0.25)
            })
            .and_then(|t| hit(t.node))
            .or_else(|| {
                dl.rects
                    .iter()
                    .rev()
                    .find(|r| x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h)
                    .and_then(|r| hit(r.node))
            });
        let Some(r) = found else {
            return false;
        };
        self.live.as_mut().unwrap().send_event(r, "click", None);
        true
    }

    /// Incremental publish for a patched page (local app OR live mount):
    /// re-lay-out only the top-level body child that contains the changed
    /// nodes, splice its items into the last display list, shift following
    /// content, and publish without touching the repaint counter. Falls
    /// back to [`Self::relayout`] whenever the structure is not provably
    /// spliceable.
    fn splice_publish(&mut self, ops: &[PatchOp]) {
        let splice = self.splice_try(ops);
        let Some((_dl, height_delta)) = splice else {
            self.relayout();
            return;
        };
        self.content_height += height_delta;
        self.scroll_y = self
            .scroll_y
            .min((self.content_height - self.viewport_h).max(0.0));
        let Some(page) = self.page.take() else {
            return;
        };
        let dl = _dl;
        self.links = dl.links.clone();
        self.refresh_find_for(&dl);
        let (ax_tree, ax_focused, ax_ids) = build_ax_tree(
            &page.dom,
            &self.page_title.clone(),
            self.focused,
            &dl,
            &page.styles,
            self.measurer.as_ref(),
        );
        self.ax_tree = ax_tree;
        self.ax_focused = ax_focused;
        self.ax_ids = ax_ids;
        self.page = Some(page);
        self.host.publish(&self.state(), &self.decorate_caret(&dl));
        self.last_dl = Some(dl);
    }

    /// The splice itself: `None` means "not provably spliceable — full
    /// relayout". Returns the new list plus the height delta for the
    /// caller's bookkeeping.
    #[allow(clippy::type_complexity)]
    fn splice_try(&mut self, ops: &[PatchOp]) -> Option<(DisplayList, f32)> {
        let app = self.app.as_ref();
        // Either mount supplies the ref bridges; both are immutable borrows.
        let node_for_ref = |r: u64| -> Option<NodeId> {
            if let Some(a) = app {
                return a.node_for(kitae_ui::patch::NodeRef(r));
            }
            #[cfg(feature = "live")]
            if let Some(l) = self.live.as_ref() {
                return l.node_for(r);
            }
            None
        };
        let ref_for = |n: NodeId| -> Option<u64> {
            if let Some(a) = app {
                return a.ref_for(n);
            }
            #[cfg(feature = "live")]
            if let Some(l) = self.live.as_ref() {
                return l.ref_for(n);
            }
            None
        };
        let mounted = {
            let mut m = app.is_some();
            #[cfg(feature = "live")]
            {
                m = m || self.live.is_some();
            }
            m
        };
        if !mounted {
            return None;
        }
        let Some(page) = self.page.as_ref() else {
            return None;
        };
        let Some(old) = self.last_dl.as_ref() else {
            return None;
        };
        // Stickies shift at paint time; splicing them is future work.
        // Toggle areas are static hit boxes like links (A5) — they ride
        // the same belongs/translate path below.
        if !old.stickies.is_empty() {
            return None;
        }
        let body = page.dom.root().and_then(|r| {
            page.dom
                .children(r)
                .iter()
                .find(|&&c| page.dom.tag(c) == "body")
                .copied()
        })?;
        // The splice root D: the top-level body child containing the ops'
        // dirty nodes. Every dirty ref must resolve into the SAME child —
        // otherwise the change spans groups and only a full relayout is
        // provably correct. (No ops → the conservative first ref-bearing
        // child, the single-mount shape.)
        let d: NodeId = if ops.is_empty() {
            let mut d = None;
            page.dom.walk(body, &mut |n| {
                if d.is_none() && ref_for(n).is_some() {
                    let mut cur = n;
                    while let Some(p) = page.dom.node(cur).parent {
                        if p == body {
                            d = Some(cur);
                            break;
                        }
                        cur = p;
                    }
                }
            });
            d?
        } else {
            // A removal detaches its subtree BEFORE the splice runs, so the
            // removed nodes' old items can no longer be proven to belong to
            // any group (their parent chain is cut) — only a full relayout
            // is provably right.
            if ops
                .iter()
                .any(|op| matches!(op, PatchOp::RemoveNode { .. }))
            {
                return None;
            }
            let mut d: Option<NodeId> = None;
            for op in ops {
                let Some(r) = dirty_ref(op) else { continue };
                let Some(n) = node_for_ref(r) else {
                    return None; // unknown ref: the old list can't be trusted
                };
                let mut top = n;
                loop {
                    match page.dom.node(top).parent {
                        Some(p) if p == body => break,
                        Some(p) => top = p,
                        None => return None, // not under body
                    }
                }
                match d {
                    None => d = Some(top),
                    Some(prev) if prev == top => {}
                    Some(_) => return None, // dirty nodes span two groups
                }
            }
            d?
        };
        // Identify D's old box: the union of what D itself paints (rects AND
        // text runs — a text-only heading paints no rect); a transparent D
        // paints nothing, so fall back to the union bounds of its
        // descendants (v7 provenance makes both exact).
        let bounds =
            |dl: &DisplayList, direct_only: bool| -> Option<(f32, f32, f32, f32)> {
                // Rects as-is; text runs become pseudo-rects the same way the
                // click hit-tester measures them.
                let items = dl.rects.iter().map(|r| (r.node, r.x, r.y, r.w, r.h)).chain(
                    dl.texts.iter().map(|t| {
                        let w =
                            self.measurer
                                .measure(&t.text, t.size, t.weight, t.italic, t.monospace);
                        (t.node, t.x, t.y - t.size * 0.75, w, t.size)
                    }),
                );
                let mut it = items.filter(|item| {
                    let node = item.0;
                    if node == d.0 {
                        return true;
                    }
                    if direct_only {
                        return false;
                    }
                    let mut n = NodeId(node);
                    loop {
                        match page.dom.node(n).parent {
                            Some(p) if p != d => n = p,
                            Some(_) => return true,
                            None => return false,
                        }
                    }
                });
                let first = it.next()?;
                let (mut x0, mut y0, mut x1, mut y1) =
                    (first.1, first.2, first.1 + first.3, first.2 + first.4);
                for item in it {
                    x0 = x0.min(item.1);
                    y0 = y0.min(item.2);
                    x1 = x1.max(item.1 + item.3);
                    y1 = y1.max(item.2 + item.4);
                }
                Some((x0, y0, x1 - x0, y1 - y0))
            };
        let old_box = match bounds(old, true).or_else(|| bounds(old, false)) {
            Some(b) => b,
            None => return None,
        };
        let (old_x, old_top, _old_w, old_h) = old_box;
        let old_h_like = old_h;
        // The shift threshold uses the like-for-like extent (see
        // `height_delta`): items whose top is at or below the splice root's
        // old bottom move down by the delta.
        let old_bottom = old_top + old_h_like;
        // Body content width: resolved from the body's actual chrome —
        // the hard-coded viewport − 32 assumed the UA padding the page
        // may legitimately have reset.
        let width = self.body_content_width();
        let sub = kitae_layout::layout_subtree_in_env(
            &page.dom,
            &page.styles,
            d,
            width,
            self.viewport_h,
            self.measurer.as_ref(),
            &self.environment,
        );
        let new_box = match bounds(&sub, true).or_else(|| bounds(&sub, false)) {
            Some(b) => b,
            None => return None,
        };
        // Height delta like-for-like: rect-union heights on both sides, so
        // chrome gaps (a control's +4px flow gap) cancel exactly when only
        // text changed.
        let new_h_like = new_box.3;
        let height_delta = new_h_like - old_h_like;
        // Build the spliced list: keep items of other groups (translated),
        // insert D's new subtree translated to (old_x, old_top).
        let dx = old_x - new_box.0;
        let dy = old_top - new_box.1;
        // Mutate a clone of the old list (`DisplayList` is non_exhaustive,
        // so it cannot be built with a struct expression from outside).
        let mut dl = old.clone();
        dl.rects.clear();
        dl.texts.clear();
        dl.links.clear();
        dl.images.clear();
        dl.media.clear();
        dl.shadows.clear();
        dl.toggles.clear();
        dl.content_height = old.content_height + height_delta;
        // An item belongs to the splice root when its provenance node
        // descends from D (v7 makes this exact — no geometry guessing).
        let belongs = |node: u32| -> bool {
            let mut n = NodeId(node);
            loop {
                if n == d {
                    return true;
                }
                match page.dom.node(n).parent {
                    Some(p) => n = p,
                    None => return false,
                }
            }
        };
        for r in &old.rects {
            if belongs(r.node) {
                continue;
            }
            let mut r = r.clone();
            if r.y >= old_bottom {
                r.y += height_delta;
            }
            dl.rects.push(r);
        }
        for t in &old.texts {
            if belongs(t.node) {
                continue;
            }
            let mut t = t.clone();
            if t.y >= old_bottom {
                t.y += height_delta;
            }
            dl.texts.push(t);
        }
        for l in &old.links {
            if belongs(l.node) {
                continue;
            }
            let mut l = l.clone();
            if l.y >= old_bottom {
                l.y += height_delta;
                if let Some(m) = l.transform {
                    l.transform = Some(m.rebase(0.0, height_delta));
                }
            }
            dl.links.push(l);
        }
        for t in &old.toggles {
            if belongs(t.node) {
                continue;
            }
            let mut t = t.clone();
            if t.y >= old_bottom {
                t.y += height_delta;
            }
            dl.toggles.push(t);
        }
        for im in &old.images {
            if belongs(im.node) {
                continue;
            }
            let mut im = im.clone();
            if im.y >= old_bottom {
                im.y += height_delta;
            }
            dl.images.push(im);
        }
        for m in &old.media {
            if belongs(m.node) {
                continue;
            }
            let mut m = m.clone();
            if m.y >= old_bottom {
                m.y += height_delta;
            }
            dl.media.push(m);
        }
        for s in &old.shadows {
            if belongs(s.node) {
                continue;
            }
            let mut s = s.clone();
            if s.y >= old_bottom {
                s.y += height_delta;
            }
            dl.shadows.push(s);
        }
        // D's fresh subtree, translated into place.
        for r in sub.rects {
            let mut r = r;
            r.x += dx;
            r.y += dy;
            dl.rects.push(r);
        }
        for t in sub.texts {
            let mut t = t;
            t.x += dx;
            t.y += dy;
            dl.texts.push(t);
        }
        for l in sub.links {
            let mut l = l;
            l.x += dx;
            l.y += dy;
            if let Some(m) = l.transform {
                l.transform = Some(m.rebase(dx, dy));
            }
            dl.links.push(l);
        }
        for t in sub.toggles {
            let mut t = t;
            t.x += dx;
            t.y += dy;
            dl.toggles.push(t);
        }
        for im in sub.images {
            let mut im = im;
            im.x += dx;
            im.y += dy;
            dl.images.push(im);
        }
        for m in sub.media {
            let mut m = m;
            m.x += dx;
            m.y += dy;
            dl.media.push(m);
        }
        for s in sub.shadows {
            let mut s = s;
            s.x += dx;
            s.y += dy;
            dl.shadows.push(s);
        }
        // New images need their raster payloads resolved like a full layout.
        let base_url = page.resource_base_url.clone();
        let document_base = page.base_url.clone();
        for im in &mut dl.images {
            if im.payload.is_none() {
                let abs = resolve(&base_url, &im.src).unwrap_or_else(|| im.src.clone());
                // I6: mixed-content policy applies to late-resolving splice
                // images too.
                if !kitae_net::mixed_content_allowed(&document_base, &abs) {
                    continue;
                }
                if let Some(raster) = ensure_qtsq_raster(&abs)
                    .or_else(|| ensure_svg_raster(&abs, im.w, im.h))
                    .or_else(|| ensure_web_raster(&abs))
                {
                    im.payload = Some(raster.id);
                    im.animated = qtsq_animation_for_id(raster.id).is_some();
                }
            }
        }
        Some((dl, height_delta))
    }

    fn style_engine_for_dom(
        &self,
        dom: &Dom,
        base_url: &str,
    ) -> (StyleEngine, StylesheetLoadStats) {
        // B9: the measurer doubles as the style engine's font-metrics
        // source, so `ch`/`ex` fold against the page's real fonts.
        let metrics = Arc::new(MeasurerMetrics(self.measurer.clone()));
        let Some(root) = dom.root() else {
            return (
                StyleEngine::with_environment(
                    &[],
                    self.viewport_w,
                    self.viewport_h,
                    &self.environment,
                )
                .with_metrics(metrics),
                StylesheetLoadStats::default(),
            );
        };
        let sources = collect_styles_in_env(
            dom,
            root,
            base_url,
            self.viewport_w,
            self.viewport_h,
            &self.environment,
        );
        let (sheets, stats) = load_stylesheets_in_env(
            &sources,
            base_url,
            &resource_base_url(dom, Some(root), base_url),
            self.viewport_w,
            self.viewport_h,
            &self.environment,
            MAX_EXTERNAL_STYLESHEETS,
            MAX_EXTERNAL_STYLESHEET_BYTES,
        );
        let refs: Vec<&str> = sheets.iter().map(String::as_str).collect();
        (
            StyleEngine::with_environment(
                &refs,
                self.viewport_w,
                self.viewport_h,
                &self.environment,
            )
            .with_metrics(metrics),
            stats,
        )
    }

    /// Download + install the page's `@font-face` families (Phase N5), then
    /// relayout-independent: a face whose family *and descriptors* are
    /// already registered is skipped, so resizes never refetch while a
    /// second weight of a loaded family still installs. Returns families
    /// newly registered.
    fn ensure_page_fonts(&mut self) -> Vec<String> {
        let (faces, base_url, document_base, doc_text) = match self.page.as_ref() {
            Some(page) => {
                let (engine, _) = self.style_engine_for_dom(&page.dom, &page.base_url);
                let mut text = String::new();
                collect_dom_text(page.dom.root(), &page.dom, &mut text);
                (
                    engine.font_faces().to_vec(),
                    page.resource_base_url.clone(),
                    page.base_url.clone(),
                    text,
                )
            }
            None => return Vec::new(),
        };
        let mut done: Vec<String> = Vec::new();
        // B11: two blocks with the same family + descriptors are the same
        // face — the second one's `src` is a duplicate download.
        let mut seen_faces: Vec<String> = Vec::new();
        for face in &faces {
            let key = face.descriptor_key();
            if seen_faces.contains(&key) {
                continue;
            }
            seen_faces.push(key);
            if !face_needs_install(face, &doc_text) {
                continue;
            }
            if ensure_web_font(face, &base_url, &document_base)
                && !done.iter().any(|f| f.eq_ignore_ascii_case(&face.family))
            {
                done.push(face.family.clone());
            }
        }
        done
    }

    fn publish_current(&mut self) {
        let Some(mut dl) = self.last_dl.clone() else {
            return;
        };
        // J5: re-step video sentinels to the current transport position so a
        // `media_play`/`media_seek` republish swaps pixels without a
        // relayout (stable raster ids — hosts just re-resolve the payload).
        let (base_url, document_base) = self
            .page
            .as_ref()
            .map(|page| (page.resource_base_url.clone(), page.base_url.clone()))
            .unwrap_or_else(|| (self.page_url.clone(), self.page_url.clone()));
        let media_pos = self.media_state().map(|m| (m.url, m.position));
        for im in &mut dl.images {
            // Sentinels re-step on every republish (that is the whole point
            // of the pixel swap); other images only stamp when unresolved.
            let is_sentinel = im.src.starts_with("\u{0}media:");
            if im.src.starts_with("\u{0}canvas:") {
                im.payload = qtsq_raster_for_url(&im.src).map(|r| r.id);
                continue;
            }
            if is_sentinel || im.payload.is_none() {
                if is_sentinel {
                    let id = im.src.trim_start_matches("\u{0}media:");
                    let item = id.parse::<usize>().ok().and_then(|i| dl.media.get(i));
                    if let Some(item) = item {
                        let src = item.src.clone();
                        let url = resolve(&base_url, &src).unwrap_or_else(|| src.clone());
                        let pos = media_pos
                            .as_ref()
                            .filter(|(u, _)| u == &url)
                            .map(|(_, p)| *p)
                            .unwrap_or(0.0);
                        if let Some(raster) =
                            video_frame_raster(&url, pos, &item.poster, &base_url, &document_base)
                        {
                            im.payload = Some(raster.id);
                        }
                    }
                } else {
                    let abs = resolve(&base_url, &im.src).unwrap_or_else(|| im.src.clone());
                    // I6: mixed-content policy on republish too.
                    if !kitae_net::mixed_content_allowed(&document_base, &abs) {
                        continue;
                    }
                    if let Some(raster) = ensure_qtsq_raster(&abs) {
                        im.payload = Some(raster.id);
                        im.animated = qtsq_animation_for_id(raster.id).is_some();
                    }
                }
            }
        }
        self.sync_display_media_state(&mut dl);
        self.host.publish(&self.state(), &self.decorate_caret(&dl));
        self.last_dl = Some(dl);
        // page-js view state (scroll/activeElement/getSelection) is cheap
        // and must never be stale after a caret repaint or a script-driven
        // scroll — styles/geometry refresh stays on the relayout cadence.
        #[cfg(feature = "page-js")]
        self.publish_page_js_view();
    }

    /// Refresh the JS-3 view state a page script can read.
    #[cfg(feature = "page-js")]
    fn publish_page_js_view(&mut self) {
        if let Some(js) = self.page_js.as_ref() {
            js.set_view_state(
                (0.0, self.scroll_y),
                self.focused.map(|n| n.0),
                self.selection.clone().unwrap_or_default(),
            );
        }
    }

    fn sync_display_media_state(&self, dl: &mut DisplayList) {
        let transport = self.media_state();
        let base = self
            .page
            .as_ref()
            .map(|page| page.resource_base_url.clone())
            .unwrap_or_else(|| self.page_url.clone());
        for item in &mut dl.media {
            let url = resolve(&base, &item.src).unwrap_or_else(|| item.src.clone());
            item.state = transport
                .as_ref()
                .filter(|media| media.url == url)
                .map(|media| (media.playing, media.position))
                .unwrap_or((false, 0.0));
        }
    }

    // -- media transport (J4) ------------------------------------------------

    /// The embedder reports it opened the current page's media in its player.
    /// Engine-side this is a no-op beyond a republish (the engine decodes;
    /// the embedder plays) — the reply frame carries the fresh state mirror.
    pub fn media_open(&mut self) {
        self.publish_current();
    }

    /// Transport mirror update from the embedder's player: playing flag and
    /// position in seconds. Republishes so the frame stream carries the new
    /// state (the FFI host learns of transport changes through frames).
    ///
    /// Two content attributes are honored here, because the transport is the
    /// only place that sees the end of a resource: `preload="none"` (play
    /// loads the deferred source) and `loop` (landing on the end rewinds to
    /// the start and keeps playing).
    pub fn media_play(&mut self, playing: bool, position: f32) {
        self.set_transport(playing, position, true);
    }

    /// Shared transport-mirror update. `loop_rewind` is the `loop` behavior
    /// and is off for a seek the in-page chrome made: dragging the scrubber to
    /// the end is a seek, only playback *reaching* the end loops (a play
    /// request may restart, per HTML's loop-on-ended rule).
    fn set_transport(&mut self, playing: bool, position: f32, loop_rewind: bool) {
        if playing {
            self.load_deferred_transport();
        }
        let duration = self
            .media_state()
            .map(|media| media.duration)
            .unwrap_or(0.0);
        let position = if position.is_finite() {
            position.clamp(0.0, duration.max(0.0))
        } else {
            0.0
        };
        // `loop`: the host parks the transport at the end of the resource;
        // rewind to the start and keep playing (the republish then re-steps
        // the decoder to position 0).
        let (playing, position) =
            if loop_rewind && duration > 0.0 && position >= duration && self.transport_loops() {
                (true, 0.0)
            } else {
                (playing, position)
            };
        self.media_session = (playing, position);
        self.publish_current();
    }

    /// Whether the page's transport media (its first media item) carries
    /// `loop`. QTSQ viewer media has no element, so it never loops.
    fn transport_loops(&self) -> bool {
        web_page_transport()
            .lock()
            .unwrap()
            .get(&self.page_url)
            .is_some_and(|transport| transport.attrs.looping)
    }

    /// Load a `preload="none"` source that playback asked for: fetching is
    /// deferred at relayout, and the transport mirror reaching the engine as
    /// "playing" is the request. The element's candidate list runs the
    /// resource selection here, so a `<source>` list behaves the same whether
    /// the load is eager or deferred. Re-running the selection is cheap and
    /// idempotent (loaded candidates are a store hit, failed ones are already
    /// marked tried), so this also repairs an association whose candidate
    /// turned out to be undecodable. No-op when nothing decodes, or when the
    /// page never deferred one.
    fn load_deferred_transport(&mut self) {
        let Some(transport) = web_page_transport()
            .lock()
            .unwrap()
            .get(&self.page_url)
            .cloned()
            .filter(|t| t.attrs.defers_load())
        else {
            return;
        };
        let Some(loaded) = select_web_media(
            &self.page_url,
            transport.node,
            &transport.candidates,
            transport.is_audio,
        ) else {
            return;
        };
        // `select_web_media` re-pointed the page's transport URL at the
        // winner (this item owns that association); the display list
        // re-stamps so the frame's media section names the resource the host
        // should decode — not the candidate that failed to decode.
        if let Some(dl) = self.last_dl.as_mut() {
            for item in &mut dl.media {
                if item.node == transport.node {
                    item.src = loaded.clone();
                }
            }
        }
    }

    /// Natural sizes of every `background-image` this page's cascade
    /// references, for hosts that want to lay out or paint artwork
    /// themselves. Empty when the page has no background images.
    pub fn background_image_sizes_public(&self) -> HashMap<String, (f32, f32)> {
        self.page
            .as_ref()
            .map(|page| background_image_sizes(page, &page.styles))
            .unwrap_or_default()
    }

    /// Current media snapshot for the page (query path; same data frames
    /// carry). `None` when the page carries no media.
    pub fn media_state_public(&self) -> Option<MediaInfo> {
        self.media_state()
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        release_qtsq_page_state(&self.page_url);
        clear_web_media_for(&self.page_url);
    }
}

/// The wire ref a patch op dirties — the anchor for choosing the splice
/// root. Structural ops that remove/move children anchor on the parent
/// (its subtree re-lays-out); insertions anchor on the new node.
fn dirty_ref(op: &PatchOp) -> Option<u64> {
    match op {
        PatchOp::InsertNode { id, .. } => Some(id.0),
        PatchOp::RemoveNode { parent, .. } | PatchOp::MoveNode { parent, .. } => Some(parent.0),
        PatchOp::SetText { node, .. }
        | PatchOp::SetAttr { node, .. }
        | PatchOp::RemoveAttr { node, .. }
        | PatchOp::SetStyle { node, .. }
        | PatchOp::SetFocus { node }
        | PatchOp::ScrollIntoView { node } => Some(node.0),
    }
}

impl Browser {
    fn is_focusable_node(
        &self,
        dom: &Dom,
        styles: &[(NodeId, ComputedStyle)],
        node: NodeId,
    ) -> bool {
        let Some(el) = dom.element(node) else {
            return false;
        };
        if styles
            .iter()
            .find(|(n, _)| *n == node)
            .map(|(_, s)| s.display)
            == Some(kitae_style::Display::None)
        {
            return false;
        }
        if el.attr("disabled").is_some() {
            return false;
        }
        match el.tag.as_str() {
            "a" => el
                .attr("href")
                .map(|h| !h.is_empty() && !h.starts_with('#'))
                .unwrap_or(false),
            "button" | "select" | "textarea" => true,
            "input" => el.attr("type").unwrap_or("text").to_ascii_lowercase() != "hidden",
            // A11b: contenteditable hosts join the tab order (editing state
            // re-checked live, same rule as is_editable_node).
            _ => el
                .attr("contenteditable")
                .map(|v| v != "false")
                .unwrap_or(false),
        }
    }

    /// Focusable elements in document order.
    pub fn focusables(&self) -> Vec<NodeId> {
        let Some(page) = self.page.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(root) = page.dom.root() {
            page.dom.walk(root, &mut |n| {
                if self.is_focusable_node(&page.dom, &page.styles, n) {
                    out.push(n);
                }
            });
        }
        out
    }

    pub fn focused(&self) -> Option<NodeId> {
        self.focused
    }

    pub fn focused_label(&self) -> Option<String> {
        let f = self.focused?;
        let page = self.page.as_ref()?;
        let label = page
            .dom
            .element(f)
            .and_then(|el| el.attr("id"))
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| page.dom.tag(f).to_string());
        Some(label)
    }

    pub fn set_focus(&mut self, node: NodeId) -> bool {
        let ok = self
            .page
            .as_ref()
            .map(|p| p.dom.root().is_some() && self.is_focusable_node(&p.dom, &p.styles, node))
            .unwrap_or(false);
        if !ok {
            return false;
        }
        let canceled = self.reset_editing_for_focus();
        self.focused = Some(node);
        self.sync_caret();
        self.refresh_ax_focus();
        if canceled {
            self.relayout();
        } else {
            self.publish_current();
        }
        true
    }

    pub fn focus_by_id(&mut self, element_id: &str) -> bool {
        let target = self
            .page
            .as_ref()
            .and_then(|p| p.dom.get_element_by_id(element_id));
        target.map(|n| self.set_focus(n)).unwrap_or(false)
    }

    pub fn clear_focus(&mut self) {
        if self.focused.is_some() {
            let canceled = self.reset_editing_for_focus();
            self.focused = None;
            self.caret = None;
            self.refresh_ax_focus();
            if canceled {
                self.relayout();
            } else {
                self.publish_current();
            }
        }
    }

    /// Re-resolve the focused AX id after a focus change that republishes
    /// without a relayout (the tree — and its id map — are unchanged).
    fn refresh_ax_focus(&mut self) {
        self.ax_focused = self.focused.and_then(|f| self.ax_ids.get(&f).cloned());
    }

    pub fn focus_next(&mut self) -> Option<NodeId> {
        let mut items = self.focusables();
        // A6: focus stays inside an open modal dialog.
        if let Some(page) = self.page.as_ref() {
            if let Some(root) = page.dom.root() {
                if let Some(dialog) = open_dialog(&page.dom, root) {
                    items.retain(|n| within(&page.dom, *n, dialog));
                }
            }
        }
        if items.is_empty() {
            return None;
        }
        let next = match self
            .focused
            .and_then(|f| items.iter().position(|&n| n == f))
        {
            Some(i) => items[(i + 1) % items.len()],
            None => items[0],
        };
        let canceled = self.reset_editing_for_focus();
        self.focused = Some(next);
        self.sync_caret();
        self.refresh_ax_focus();
        if canceled {
            self.relayout();
        } else {
            self.publish_current();
        }
        Some(next)
    }

    pub fn focus_prev(&mut self) -> Option<NodeId> {
        let mut items = self.focusables();
        // A6: focus stays inside an open modal dialog.
        if let Some(page) = self.page.as_ref() {
            if let Some(root) = page.dom.root() {
                if let Some(dialog) = open_dialog(&page.dom, root) {
                    items.retain(|n| within(&page.dom, *n, dialog));
                }
            }
        }
        if items.is_empty() {
            return None;
        }
        let prev = match self
            .focused
            .and_then(|f| items.iter().position(|&n| n == f))
        {
            None => *items.last().unwrap(),
            Some(0) => *items.last().unwrap(),
            Some(i) => items[i - 1],
        };
        let canceled = self.reset_editing_for_focus();
        self.focused = Some(prev);
        self.sync_caret();
        self.refresh_ax_focus();
        if canceled {
            self.relayout();
        } else {
            self.publish_current();
        }
        Some(prev)
    }

    /// A6: Esc on an open modal dialog — close the topmost one (remove its
    /// `open` attribute) and relayout. No-op without an open dialog.
    pub fn escape_key(&mut self) {
        let Some(page) = self.page.as_mut() else {
            return;
        };
        let Some(root) = page.dom.root() else {
            return;
        };
        if let Some(dialog) = open_dialog(&page.dom, root) {
            page.dom.remove_attr(dialog, "open");
            self.reset_editing_for_focus();
            self.focused = None;
            self.caret = None;
            self.relayout();
        }
    }

    // -- caret + form-control editing (A11a) ---------------------------------

    /// Whether `node` is an editable target: a text-like form control, or
    /// (A11b) an element carrying `contenteditable`. The button/checkbox/
    /// radio/file/image/range/color/reset/hidden widgets and anything
    /// disabled or readonly never edit; contenteditable="false" opts out.
    fn is_editable_node(&self, dom: &Dom, node: NodeId) -> bool {
        let Some(el) = dom.element(node) else {
            return false;
        };
        if el.attr("disabled").is_some() || el.attr("readonly").is_some() {
            return false;
        }
        match el.tag.as_str() {
            "textarea" => true,
            "input" => {
                let t = el.attr("type").unwrap_or("text").to_ascii_lowercase();
                !matches!(
                    t.as_str(),
                    "checkbox"
                        | "radio"
                        | "submit"
                        | "button"
                        | "reset"
                        | "hidden"
                        | "file"
                        | "image"
                        | "range"
                        | "color"
                )
            }
            // A11b: a contenteditable host edits as plain text (the editing
            // ops flatten it — see write_control_value); the attribute is
            // read live so a runtime flip between true/false takes effect.
            _ => el
                .attr("contenteditable")
                .map(|v| v != "false")
                .unwrap_or(false),
        }
    }

    /// The editable control's current value: textarea text content, the
    /// input `value` attribute, or (A11b) a contenteditable host's flattened
    /// text. Empty string for anything else.
    fn read_control_value(&self, node: NodeId) -> String {
        let Some(page) = self.page.as_ref() else {
            return String::new();
        };
        match page.dom.tag(node) {
            "textarea" => {
                let mut s = String::new();
                for &c in page.dom.children(node) {
                    let n = page.dom.node(c);
                    if matches!(n.kind, NodeKind::Text) {
                        s.push_str(&n.text);
                    }
                }
                s
            }
            "input" => page
                .dom
                .element(node)
                .and_then(|e| e.attr("value"))
                .unwrap_or("")
                .to_string(),
            // A11b: the contenteditable value is its concatenated
            // descendant text (markup inside is preserved until an edit
            // flattens it — same model as the textarea arm).
            _ => {
                let mut s = String::new();
                page.dom.walk(node, &mut |n| {
                    if matches!(page.dom.node(n).kind, NodeKind::Text) {
                        s.push_str(&page.dom.node(n).text);
                    }
                });
                s
            }
        }
    }

    /// Write a new value into an editable control, keyed by node — the
    /// editable arms of [`Self::set_input_value`] without the id lookup, so
    /// the editing ops work on id-less controls too.
    fn write_control_value(&mut self, node: NodeId, value: &str) -> bool {
        let Some(page) = self.page.as_mut() else {
            return false;
        };
        match page.dom.tag(node) {
            "textarea" => {
                let kids: Vec<NodeId> = page.dom.children(node).to_vec();
                let mut first_text: Option<NodeId> = None;
                for k in kids {
                    if matches!(page.dom.node(k).kind, NodeKind::Text) {
                        if first_text.is_none() {
                            first_text = Some(k);
                        } else {
                            page.dom.detach(node, k);
                        }
                    }
                }
                match first_text {
                    Some(t) => {
                        page.dom.set_text(t, value.to_string());
                        true
                    }
                    None => {
                        let t = page.dom.create_text(value.to_string());
                        page.dom.append(node, t);
                        true
                    }
                }
            }
            "input" => page.dom.set_attr(node, "value", value),
            // A11b: editing a contenteditable host collapses it to ONE text
            // child — element children go too (their text is already part of
            // the value, so keeping them would duplicate it). Markup inside
            // is not preserved across edits: the v1 editing contract;
            // structured selection editing is A11c.
            _ => {
                let kids: Vec<NodeId> = page.dom.children(node).to_vec();
                for k in kids {
                    page.dom.detach(node, k);
                }
                let t = page.dom.create_text(value.to_string());
                page.dom.append(node, t);
                true
            }
        }
    }

    /// The focused editable control, if any.
    fn editable_focus(&self) -> Option<NodeId> {
        self.focused.filter(|&f| {
            self.page
                .as_ref()
                .map(|p| self.is_editable_node(&p.dom, f))
                .unwrap_or(false)
        })
    }

    fn clear_text_selection(&mut self) {
        self.selection_range = None;
    }

    fn reset_editing_for_focus(&mut self) -> bool {
        let had_range = self.selection_range.is_some();
        let had_composition = self.composition.is_some();
        self.cancel_composition_state();
        self.clear_text_selection();
        if had_range {
            self.selection = None;
        }
        had_composition
    }

    fn active_text_selection(&self, len: usize) -> Option<(usize, usize)> {
        self.selection_range
            .map(|s| s.bounds(len))
            .filter(|(start, end)| start < end)
    }

    fn cancel_composition_state(&mut self) -> bool {
        let Some(state) = self.composition.take() else {
            return false;
        };
        let restored = self.write_control_value(state.node, &state.base);
        if restored {
            self.caret = Some(state.start);
        } else {
            self.composition = Some(state);
        }
        restored
    }

    fn selected_text(&self, node: NodeId, start: usize, end: usize) -> Option<String> {
        let value = self.read_control_value(node);
        let chars: Vec<char> = value.chars().collect();
        if start >= end || end > chars.len() {
            return None;
        }
        let password = self
            .page
            .as_ref()
            .and_then(|p| p.dom.element(node))
            .filter(|e| e.tag == "input")
            .and_then(|e| e.attr("type"))
            .is_some_and(|t| t.eq_ignore_ascii_case("password"));
        Some(if password {
            "•".repeat(end - start)
        } else {
            chars[start..end].iter().collect()
        })
    }

    fn refresh_text_selection(&mut self, node: NodeId, len: usize) {
        let Some((start, end)) = self.active_text_selection(len) else {
            self.selection = None;
            return;
        };
        self.selection = self.selected_text(node, start, end);
    }

    /// Reconcile the caret with the focused node: park it at the end of an
    /// editable control's value (the click-to-place and append-on-focus
    /// contract), clear it for anything else.
    fn sync_caret(&mut self) {
        let editable = self.editable_focus().is_some();
        self.caret = if editable {
            self.focused
                .map(|f| self.read_control_value(f).chars().count())
        } else {
            None
        };
    }

    /// Public caret position (char offset into the focused control's value),
    /// or `None` when no editable control is focused.
    pub fn caret_position(&self) -> Option<usize> {
        self.caret
    }

    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let node = self.editable_focus()?;
        let len = self.read_control_value(node).chars().count();
        self.active_text_selection(len)
    }

    pub fn begin_selection(&mut self, offset: usize) -> bool {
        let Some(node) = self.editable_focus() else {
            return false;
        };
        let canceled = self.composition.is_some();
        self.cancel_composition_state();
        let len = self.read_control_value(node).chars().count();
        let offset = offset.min(len);
        self.selection_range = Some(TextSelection {
            anchor: offset,
            head: offset,
        });
        self.selection = None;
        self.caret = Some(offset);
        if canceled {
            self.relayout();
        } else {
            self.publish_current();
        }
        true
    }

    pub fn extend_selection(&mut self, offset: usize) -> bool {
        let Some(node) = self.editable_focus() else {
            return false;
        };
        let canceled = self.composition.is_some();
        self.cancel_composition_state();
        let len = self.read_control_value(node).chars().count();
        let anchor = self
            .selection_range
            .map(|s| s.anchor.min(len))
            .unwrap_or_else(|| self.caret.unwrap_or(len).min(len));
        let head = offset.min(len);
        self.selection_range = Some(TextSelection { anchor, head });
        self.caret = Some(head);
        self.refresh_text_selection(node, len);
        if canceled {
            self.relayout();
        } else {
            self.publish_current();
        }
        true
    }

    fn editable_offset_at_point(&self, x: f32, y: f32) -> Option<usize> {
        let (node, _, _, _) = self.clicked_control(x, y)?;
        let page = self.page.as_ref()?;
        if !self.is_editable_node(&page.dom, node) {
            return None;
        }
        Some(if self.is_contenteditable_host(&page.dom, node) {
            self.caret_offset_for_point(node, x, y)
        } else {
            self.caret_offset_for_x(node, x)
        })
    }

    pub fn begin_selection_at_point(&mut self, x: f32, y: f32) -> bool {
        let Some(offset) = self.editable_offset_at_point(x, y) else {
            return false;
        };
        self.begin_selection(offset)
    }

    pub fn extend_selection_at_point(&mut self, x: f32, y: f32) -> bool {
        let Some(offset) = self.editable_offset_at_point(x, y) else {
            return false;
        };
        if self.selection_range.is_none() {
            self.begin_selection(offset)
        } else {
            self.extend_selection(offset)
        }
    }

    pub fn end_selection(&mut self) -> bool {
        self.selection_range.is_some()
    }

    pub fn set_selection_range(&mut self, start: usize, end: usize) -> bool {
        if !self.begin_selection(start) {
            return false;
        }
        self.extend_selection(end)
    }

    pub fn selection_begin(&mut self, offset: usize) -> bool {
        self.begin_selection(offset)
    }

    pub fn selection_extend(&mut self, offset: usize) -> bool {
        self.extend_selection(offset)
    }

    pub fn selection_end(&mut self) -> bool {
        self.end_selection()
    }

    /// Backspace: delete the char before the caret. No-op at position 0.
    pub fn backspace_focused(&mut self) -> bool {
        let Some(node) = self.editable_focus() else {
            return false;
        };
        let canceled = self.composition.is_some();
        self.cancel_composition_state();
        let value = self.read_control_value(node);
        let chars: Vec<char> = value.chars().collect();
        let caret = self.caret.unwrap_or(chars.len()).min(chars.len());
        let (start, end) = self
            .active_text_selection(chars.len())
            .unwrap_or((caret, caret));
        let (remove_start, remove_end, next_caret) = if start < end {
            (start, end, start)
        } else if caret > 0 {
            (caret - 1, caret, caret - 1)
        } else {
            if canceled {
                self.relayout();
            }
            return false;
        };
        let mut next = String::new();
        next.extend(chars[..remove_start].iter());
        next.extend(chars[remove_end..].iter());
        if !self.write_control_value(node, &next) {
            return false;
        }
        self.clear_text_selection();
        self.selection = None;
        self.caret = Some(next_caret);
        self.relayout();
        true
    }

    /// Delete-forward: remove the char after the caret. No-op at the end.
    pub fn delete_forward(&mut self) -> bool {
        let Some(node) = self.editable_focus() else {
            return false;
        };
        let canceled = self.composition.is_some();
        self.cancel_composition_state();
        let value = self.read_control_value(node);
        let chars: Vec<char> = value.chars().collect();
        let caret = self.caret.unwrap_or(chars.len()).min(chars.len());
        let (start, end) = self
            .active_text_selection(chars.len())
            .unwrap_or((caret, caret));
        let (remove_start, remove_end, next_caret) = if start < end {
            (start, end, start)
        } else if caret < chars.len() {
            (caret, caret + 1, caret)
        } else {
            if canceled {
                self.relayout();
            }
            return false;
        };
        let mut next = String::new();
        next.extend(chars[..remove_start].iter());
        next.extend(chars[remove_end..].iter());
        if !self.write_control_value(node, &next) {
            return false;
        }
        self.clear_text_selection();
        self.selection = None;
        self.caret = Some(next_caret);
        self.relayout();
        true
    }

    /// Move the caret by `delta` chars (negative = left), clamped to the
    /// value bounds. Returns the new position, or `None` without an editable
    /// focus. Republishes (caret paint) without a relayout.
    pub fn move_caret(&mut self, delta: isize) -> Option<usize> {
        let node = self.editable_focus()?;
        let canceled = self.composition.is_some();
        self.cancel_composition_state();
        let len = self.read_control_value(node).chars().count();
        let next = if let Some((start, end)) = self.active_text_selection(len) {
            match delta.cmp(&0) {
                std::cmp::Ordering::Less => start,
                std::cmp::Ordering::Greater => end,
                std::cmp::Ordering::Equal => self.caret.unwrap_or(end),
            }
        } else {
            let cur = self.caret.unwrap_or(len) as isize;
            (cur.saturating_add(delta)).clamp(0, len as isize) as usize
        };
        self.clear_text_selection();
        self.selection = None;
        self.caret = Some(next);
        if canceled {
            self.relayout();
        } else {
            self.publish_current();
        }
        Some(next)
    }

    /// Place the caret at an absolute char offset (clamped to the value).
    pub fn set_caret(&mut self, offset: usize) -> Option<usize> {
        let node = self.editable_focus()?;
        let canceled = self.composition.is_some();
        self.cancel_composition_state();
        let len = self.read_control_value(node).chars().count();
        let next = offset.min(len);
        self.clear_text_selection();
        self.selection = None;
        self.caret = Some(next);
        if canceled {
            self.relayout();
        } else {
            self.publish_current();
        }
        Some(next)
    }

    pub fn extend_caret(&mut self, delta: isize) -> Option<usize> {
        let node = self.editable_focus()?;
        let canceled = self.composition.is_some();
        self.cancel_composition_state();
        let len = self.read_control_value(node).chars().count();
        let anchor = self
            .selection_range
            .map(|selection| selection.anchor.min(len))
            .unwrap_or_else(|| self.caret.unwrap_or(len).min(len));
        let head = self
            .selection_range
            .map(|selection| selection.head.min(len))
            .unwrap_or_else(|| self.caret.unwrap_or(len).min(len));
        let next = (head as isize + delta).clamp(0, len as isize) as usize;
        self.selection_range = Some(TextSelection { anchor, head: next });
        self.caret = Some(next);
        self.refresh_text_selection(node, len);
        if canceled {
            self.relayout();
        } else {
            self.publish_current();
        }
        Some(next)
    }

    /// Caret to the start of the value (Home).
    pub fn caret_home(&mut self) -> Option<usize> {
        self.set_caret(0)
    }

    /// Caret to the end of the value (End).
    pub fn caret_end(&mut self) -> Option<usize> {
        let node = self.editable_focus()?;
        let len = self.read_control_value(node).chars().count();
        self.set_caret(len)
    }

    pub fn update_marked_text(&mut self, text: &str) -> bool {
        let Some(node) = self.editable_focus() else {
            return false;
        };
        let (start, base) = match self.composition.clone() {
            Some(state) => (state.start, state.base),
            None => {
                let value = self.read_control_value(node);
                let len = value.chars().count();
                let (start, end) = self.active_text_selection(len).unwrap_or_else(|| {
                    (
                        self.caret.unwrap_or(len).min(len),
                        self.caret.unwrap_or(len).min(len),
                    )
                });
                let base = splice_chars(&value, start, end, "");
                self.clear_text_selection();
                self.selection = None;
                (start, base)
            }
        };
        let marked = text.to_string();
        let marked_len = marked.chars().count();
        let preview = splice_chars(&base, start, start, &marked);
        if !self.write_control_value(node, &preview) {
            return false;
        }
        self.composition = Some(CompositionState {
            node,
            start,
            base,
            marked,
        });
        self.selection = None;
        self.caret = Some(start + marked_len);
        self.relayout();
        true
    }

    pub fn set_marked_text(&mut self, text: &str) -> bool {
        self.update_marked_text(text)
    }

    pub fn composition_active(&self) -> bool {
        self.composition.is_some()
    }

    pub fn commit_composition(&mut self) -> bool {
        self.commit_composition_inner(None)
    }

    pub fn commit_composition_text(&mut self, text: &str) -> bool {
        self.commit_composition_inner(Some(text))
    }

    fn commit_composition_inner(&mut self, text: Option<&str>) -> bool {
        let Some(state) = self.composition.take() else {
            return false;
        };
        let replacement = text.unwrap_or(&state.marked);
        let next = splice_chars(&state.base, state.start, state.start, replacement);
        if !self.write_control_value(state.node, &next) {
            self.composition = Some(state);
            return false;
        }
        self.clear_text_selection();
        self.selection = None;
        self.caret = Some(state.start + replacement.chars().count());
        self.relayout();
        true
    }

    pub fn cancel_composition(&mut self) -> bool {
        if self.composition.is_none() {
            return false;
        }
        self.cancel_composition_state();
        self.clear_text_selection();
        self.selection = None;
        self.relayout();
        true
    }

    pub fn composition_update(&mut self, text: &str) -> bool {
        self.update_marked_text(text)
    }

    pub fn composition_commit(&mut self) -> bool {
        self.commit_composition()
    }

    pub fn composition_cancel(&mut self) -> bool {
        self.cancel_composition()
    }

    fn selection_run_width(&self, run: &kitae_layout::TextRun, text: &str) -> f32 {
        if run.letter_spacing == 0.0 {
            self.measurer.measure_family(
                text,
                run.size,
                run.weight,
                run.italic,
                run.monospace,
                &run.family,
            )
        } else {
            self.measurer.measure_run_sp_family(
                text,
                run.size,
                run.weight,
                run.italic,
                run.monospace,
                run.letter_spacing,
                &run.family,
            )
        }
    }

    fn selection_rects(
        &self,
        dl: &DisplayList,
        node: NodeId,
        start: usize,
        end: usize,
    ) -> Vec<kitae_layout::Rect> {
        if start >= end {
            return Vec::new();
        }
        let runs = if self
            .page
            .as_ref()
            .map(|p| self.is_contenteditable_host(&p.dom, node))
            .unwrap_or(false)
        {
            self.editable_runs(dl, node)
        } else {
            dl.texts
                .iter()
                .filter(|run| run.node == node.0)
                .collect::<Vec<_>>()
        };
        let mut logical = 0usize;
        let mut out = Vec::new();
        for (index, run) in runs.iter().enumerate() {
            let run_len = run.text.chars().count();
            let run_start = logical;
            let run_end = logical + run_len;
            let selected_start = start.max(run_start);
            let selected_end = end.min(run_end);
            if selected_start < selected_end {
                let before: String = run.text.chars().take(selected_start - run_start).collect();
                let selected: String = run
                    .text
                    .chars()
                    .take(selected_end - run_start)
                    .skip(selected_start - run_start)
                    .collect();
                let x = run.x + self.selection_run_width(run, &before);
                let w = self.selection_run_width(run, &selected);
                if w > 0.0 {
                    let (ascent, descent) =
                        self.measurer
                            .metrics_family(run.size, run.monospace, &run.family);
                    let mut rect = kitae_layout::Rect::solid(
                        x,
                        run.y - ascent,
                        w,
                        ascent + descent,
                        SELECTION_HIGHLIGHT_COLOR,
                    );
                    rect.node = node.0;
                    rect.transform = run.transform;
                    out.push(rect);
                }
            }
            logical = run_end;
            if index + 1 < runs.len() {
                logical += 1;
            }
        }
        out
    }

    /// Return a copy of `dl` with selection highlights and the caret painted.
    fn decorate_caret(&self, dl: &DisplayList) -> DisplayList {
        let mut out = dl.clone();
        let selection_node = self.editable_focus().map(|node| node.0);
        out.rects.retain(|rect| {
            !(rect.color == SELECTION_HIGHLIGHT_COLOR
                && selection_node.is_some_and(|node| rect.node == node))
        });
        // F4/F5: the builtin media chrome + captions paint before the caret
        // (the caret wins any overlap). The pass strips its own previous
        // sentinel-stamped output first, so republishes never accumulate.
        self.paint_media_chrome(&mut out);
        if let Some(node) = self.editable_focus() {
            let len = self.read_control_value(node).chars().count();
            if let Some((start, end)) = self.active_text_selection(len) {
                let rects = self.selection_rects(&out, node, start, end);
                out.rects.extend(rects);
            }
        }
        let (Some(node), Some(caret)) = (self.editable_focus(), self.caret) else {
            return out;
        };
        let node_u = node.0;
        // A11b: a contenteditable host paints its value as normal text-flow
        // runs stamped with the text nodes' ids (not the host's), wrapped
        // across as many lines as the layout produced — the caret maps the
        // offset across those runs instead of anchoring to one label run.
        let ce = self
            .page
            .as_ref()
            .map(|p| self.is_contenteditable_host(&p.dom, node))
            .unwrap_or(false);
        if ce {
            if let Some(r) = self.contenteditable_caret_rect(&out, node, caret) {
                out.rects.push(r);
            }
            return out;
        }
        // Anchor the caret to the control's painted label run (same face,
        // size, color, baseline as the value text). An empty field with no
        // placeholder paints no run, so fall back to the control box's text
        // inset with default metrics.
        let (base_x, size, weight, italic, monospace, family, color, top, height) =
            match out.texts.iter().find(|t| t.node == node_u) {
                Some(r) => {
                    let (a, d) = self.measurer.metrics_family(r.size, r.monospace, &r.family);
                    (
                        r.x,
                        r.size,
                        r.weight,
                        r.italic,
                        r.monospace,
                        r.family.clone(),
                        r.color,
                        r.y - a,
                        a + d,
                    )
                }
                None => {
                    let Some(b) = out.rects.iter().find(|r| r.node == node_u) else {
                        return out;
                    };
                    let (a, d) = self.measurer.metrics(13.0, false);
                    (
                        b.x + 6.0,
                        13.0,
                        400,
                        false,
                        false,
                        String::new(),
                        0x111111FF,
                        b.y + (b.h - (a + d)) / 2.0,
                        a + d,
                    )
                }
            };
        // Width of the text before the caret, measured with the run's face.
        let value = self.read_control_value(node);
        let prefix: String = value.chars().take(caret).collect();
        let w = self
            .measurer
            .measure_family(&prefix, size, weight, italic, monospace, &family);
        let mut caret_rect = kitae_layout::Rect::solid(base_x + w, top, 1.0, height, color);
        caret_rect.node = node_u;
        out.rects.push(caret_rect);
        out
    }

    // -- contenteditable editing (A11b) --------------------------------------

    /// Whether `node` carries `contenteditable` in an editing state (bare,
    /// "true", or anything but "false"). Editing inheritance from an editable
    /// ancestor is not modeled — the attribute must sit on the element.
    fn is_contenteditable_host(&self, dom: &Dom, node: NodeId) -> bool {
        dom.element(node)
            .and_then(|e| e.attr("contenteditable"))
            .map(|v| v != "false")
            .unwrap_or(false)
    }

    /// The host's painted runs (its own node stamp or a descendant's), in
    /// paint order — the value's words as the layout produced them.
    fn editable_runs<'a>(
        &self,
        dl: &'a DisplayList,
        host: NodeId,
    ) -> Vec<&'a kitae_layout::TextRun> {
        let Some(page) = self.page.as_ref() else {
            return Vec::new();
        };
        dl.texts
            .iter()
            .filter(|t| {
                let tn = NodeId(t.node);
                tn == host || within(&page.dom, tn, host)
            })
            .collect()
    }

    /// Caret rect for a char offset into a contenteditable host's value.
    /// The value's words paint as consecutive runs separated by a single
    /// space, so DOM offsets map by walking word+space counts; an offset
    /// past the last painted word parks at that word's end. Whitespace
    /// outside the single-space model (newlines, runs of spaces) shifts the
    /// mapping — a documented v1 limit until caret positions ride the layout
    /// wire. An empty host anchors the caret at its painted box's text
    /// inset, like an empty form control.
    fn contenteditable_caret_rect(
        &self,
        dl: &DisplayList,
        host: NodeId,
        caret: usize,
    ) -> Option<kitae_layout::Rect> {
        let runs = self.editable_runs(dl, host);
        let mut acc = 0usize;
        let mut hit: Option<(&kitae_layout::TextRun, f32)> = None;
        for run in &runs {
            let n = run.text.chars().count();
            if caret <= acc + n {
                let prefix: String = run.text.chars().take(caret - acc).collect();
                let w = self.measurer.measure_family(
                    &prefix,
                    run.size,
                    run.weight,
                    run.italic,
                    run.monospace,
                    &run.family,
                );
                hit = Some((run, w));
                break;
            }
            acc += n + 1;
        }
        let (run, w) = match hit {
            Some(hw) => hw,
            None => {
                let r = match runs.last() {
                    Some(r) => r,
                    None => {
                        let b = dl.rects.iter().find(|r| r.node == host.0)?;
                        let (a, d) = self.measurer.metrics(13.0, false);
                        let mut rect = kitae_layout::Rect::solid(
                            b.x + 4.0,
                            b.y + (b.h - (a + d)) / 2.0,
                            1.0,
                            a + d,
                            0x111111FF,
                        );
                        rect.node = host.0;
                        return Some(rect);
                    }
                };
                let w = self.measurer.measure_family(
                    &r.text,
                    r.size,
                    r.weight,
                    r.italic,
                    r.monospace,
                    &r.family,
                );
                (*r, w)
            }
        };
        let (a, d) = self
            .measurer
            .metrics_family(run.size, run.monospace, &run.family);
        let mut rect = kitae_layout::Rect::solid(run.x + w, run.y - a, 1.0, a + d, run.color);
        rect.node = host.0;
        Some(rect)
    }

    /// Click-to-place for contenteditable hosts: pick the painted line whose
    /// baselines sit nearest `y`, the word containing `x` on it (clamping to
    /// the line's start when the click is left of everything), and the glyph
    /// edge within that word — the same half-glyph rounding form controls
    /// use. Clamped to the value's length.
    fn caret_offset_for_point(&self, host: NodeId, x: f32, y: f32) -> usize {
        let Some(dl) = self.last_dl.as_ref() else {
            return 0;
        };
        let end = self.read_control_value(host).chars().count();
        let runs = self.editable_runs(dl, host);
        if runs.is_empty() {
            return 0;
        }
        let mut line_y = runs[0].y;
        for r in &runs {
            if (r.y - y).abs() < (line_y - y).abs() {
                line_y = r.y;
            }
        }
        let mut acc = 0usize;
        let mut off = end;
        for run in &runs {
            let n = run.text.chars().count();
            if (run.y - line_y).abs() < 1.0 {
                let w = self.measurer.measure_family(
                    &run.text,
                    run.size,
                    run.weight,
                    run.italic,
                    run.monospace,
                    &run.family,
                );
                if x <= run.x + w {
                    let mut gacc = 0.0;
                    let mut goff = 0usize;
                    for c in run.text.chars() {
                        let mut buf = [0u8; 4];
                        let gw = self.measurer.measure_family(
                            c.encode_utf8(&mut buf),
                            run.size,
                            run.weight,
                            run.italic,
                            run.monospace,
                            &run.family,
                        );
                        if gacc + gw * 0.5 > x - run.x {
                            break;
                        }
                        gacc += gw;
                        goff += 1;
                    }
                    off = acc + goff;
                    break;
                }
                off = acc + n;
            }
            acc += n + 1;
        }
        off.min(end)
    }

    /// Public read for hosts/tests: a contenteditable host's editable value
    /// by element id (form controls read through [`Self::input_value`]).
    pub fn editable_value(&self, element_id: &str) -> Option<String> {
        let page = self.page.as_ref()?;
        let node = page.dom.get_element_by_id(element_id)?;
        if self.is_contenteditable_host(&page.dom, node) {
            return Some(self.read_control_value(node));
        }
        None
    }

    pub fn input_value(&self, element_id: &str) -> Option<String> {
        let page = self.page.as_ref()?;
        let node = page.dom.get_element_by_id(element_id)?;
        match page.dom.tag(node) {
            "input" => {
                let el = page.dom.element(node)?;
                let t = el.attr("type").unwrap_or("text").to_ascii_lowercase();
                match t.as_str() {
                    "checkbox" | "radio" => Some(if el.attr("checked").is_some() {
                        "checked".into()
                    } else {
                        String::new()
                    }),
                    _ => Some(el.attr("value").unwrap_or("").to_string()),
                }
            }
            "textarea" => {
                let mut parts = Vec::new();
                for &c in page.dom.children(node) {
                    let n = page.dom.node(c);
                    if matches!(n.kind, NodeKind::Text) {
                        parts.push(n.text.clone());
                    }
                }
                Some(parts.concat())
            }
            "select" => {
                let mut first: Option<String> = None;
                for &c in page.dom.children(node) {
                    if page.dom.tag(c) != "option" {
                        continue;
                    }
                    let mut txt = String::new();
                    for &g in page.dom.children(c) {
                        let n = page.dom.node(g);
                        if matches!(n.kind, NodeKind::Text) {
                            txt.push_str(&n.text);
                        }
                    }
                    let val = page
                        .dom
                        .element(c)
                        .and_then(|e| e.attr("value"))
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| txt.trim().to_string());
                    if first.is_none() {
                        first = Some(val.clone());
                    }
                    if page
                        .dom
                        .element(c)
                        .map(|e| e.attr("selected").is_some())
                        .unwrap_or(false)
                    {
                        return Some(val);
                    }
                }
                Some(first.unwrap_or_default())
            }
            _ => None,
        }
    }

    pub fn set_input_value(&mut self, element_id: &str, value: &str) -> bool {
        // Phase 2 (`live`): on a live page an id'd input is a two-way
        // binding — the value echoes to the server as an `input` event and
        // the server's patch (draft → rendered value) comes back.
        #[cfg(feature = "live")]
        if self.live.is_some() && self.live_input(element_id, value) {
            return true;
        }
        let target = self
            .page
            .as_mut()
            .and_then(|p| p.dom.get_element_by_id(element_id));
        let Some(node) = target else {
            return false;
        };
        if self.focused == Some(node) {
            self.reset_editing_for_focus();
        }
        let tag = self
            .page
            .as_ref()
            .map(|p| p.dom.tag(node).to_string())
            .unwrap_or_default();
        let ok = match tag.as_str() {
            "input" => {
                let page = self.page.as_mut().unwrap();
                let t = page
                    .dom
                    .element(node)
                    .and_then(|e| e.attr("type"))
                    .unwrap_or("text")
                    .to_ascii_lowercase();
                match t.as_str() {
                    "checkbox" | "radio" => {
                        let on = matches!(
                            value.to_ascii_lowercase().as_str(),
                            "checked" | "true" | "1" | "on"
                        ) || (!value.is_empty()
                            && !matches!(
                                value.to_ascii_lowercase().as_str(),
                                "false" | "0" | "off" | ""
                            ));
                        if on {
                            page.dom.set_attr(node, "checked", "")
                        } else {
                            page.dom.remove_attr(node, "checked")
                        }
                    }
                    _ => page.dom.set_attr(node, "value", value),
                }
            }
            "textarea" => {
                let page = self.page.as_mut().unwrap();
                let kids: Vec<NodeId> = page.dom.children(node).to_vec();
                let mut first_text: Option<NodeId> = None;
                for k in kids {
                    if matches!(page.dom.node(k).kind, NodeKind::Text) {
                        if first_text.is_none() {
                            first_text = Some(k);
                        } else {
                            page.dom.detach(node, k);
                        }
                    }
                }
                match first_text {
                    Some(t) => page.dom.set_text(t, value.to_string()),
                    None => {
                        let t = page.dom.create_text(value.to_string());
                        page.dom.append(node, t);
                        true
                    }
                }
            }
            "select" => {
                let page = self.page.as_mut().unwrap();
                let opts: Vec<NodeId> = page
                    .dom
                    .children(node)
                    .to_vec()
                    .into_iter()
                    .filter(|&c| page.dom.tag(c) == "option")
                    .collect();
                if opts.is_empty() {
                    false
                } else {
                    let mut matched = false;
                    for o in &opts {
                        let val = page
                            .dom
                            .element(*o)
                            .and_then(|e| e.attr("value"))
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| {
                                let mut txt = String::new();
                                for &g in page.dom.children(*o) {
                                    let n = page.dom.node(g);
                                    if matches!(n.kind, NodeKind::Text) {
                                        txt.push_str(&n.text);
                                    }
                                }
                                txt.trim().to_string()
                            });
                        if val == value {
                            page.dom.set_attr(*o, "selected", "");
                            matched = true;
                        } else {
                            page.dom.remove_attr(*o, "selected");
                        }
                    }
                    if !matched {
                        for o in opts.iter().skip(1) {
                            page.dom.remove_attr(*o, "selected");
                        }
                        if let Some(f) = opts.first() {
                            page.dom.set_attr(*f, "selected", "");
                        }
                    }
                    true
                }
            }
            _ => false,
        };
        if ok {
            if self.focused == Some(node) {
                self.caret = Some(value.chars().count());
            }
            // A select's value only moves by user choice (the host's
            // dropdown reports through here): that commit fires input +
            // change. Text fields set from the host are programmatic.
            #[cfg(feature = "page-js")]
            if tag == "select" {
                let _ = self.dispatch_js_event_at("input", node, None);
                let _ = self.dispatch_js_event_at("change", node, None);
            }
            self.relayout();
        }
        ok
    }

    /// The live half of [`Self::set_input_value`]: find the element by id,
    /// resolve its wire ref, and (when the server marked it
    /// handler-bearing) set the value locally + send the `input` event.
    /// False lets the ordinary form path handle it.
    #[cfg(feature = "live")]
    fn live_input(&mut self, element_id: &str, value: &str) -> bool {
        let node = self
            .page
            .as_ref()
            .and_then(|p| p.dom.get_element_by_id(element_id));
        let Some(node) = node else {
            return false;
        };
        let Some(r) = self.live.as_ref().and_then(|l| l.ref_for(node)) else {
            return false;
        };
        if !self.live.as_ref().unwrap().is_handler_ref(r) {
            return false;
        }
        // Local echo for instant feedback; the server's patch confirms.
        if let Some(page) = self.page.as_mut() {
            page.dom.set_attr(node, "value", value);
        }
        self.live
            .as_mut()
            .unwrap()
            .send_event(r, "input", Some(value.to_string()));
        true
    }

    /// Replace the value of a control addressed by **node**, id or not: the
    /// node-keyed form of [`Self::set_input_value`], used by the agent
    /// plane's `fill` (a selector resolves to a node, and the element it
    /// points at need not carry an `id`). Writes through the same
    /// `write_control_value` path as typing, so the DOM, the caret and the
    /// `input`/`change` events all agree with what a person would see.
    pub fn set_control_value_for_node(&mut self, node: NodeId, value: &str) -> bool {
        if self.page.is_none() {
            return false;
        }
        let tag = self.page.as_ref().map(|p| p.dom.tag(node).to_string());
        let Some(tag) = tag else { return false };
        // Checkbox/radio controls want `checked`, which the value writer
        // handles; everything else is a text-ish write.
        if !matches!(tag.as_str(), "input" | "textarea" | "select") {
            return false;
        }
        let editable = {
            let page = self.page.as_ref().unwrap();
            self.is_editable_node(&page.dom, node)
        };
        if !matches!(tag.as_str(), "select") && !editable {
            return false;
        }
        if self.write_control_value(node, value) {
            self.caret = Some(value.chars().count());
            self.relayout();
            #[cfg(feature = "page-js")]
            self.dispatch_js_edit_events(node, value);
            true
        } else {
            false
        }
    }

    /// Insert `text` at the caret of the focused editable control (A11a).
    /// The caret defaults to the end of the value when unset — so focusing
    /// a control and typing appends, matching the pre-caret contract — and
    /// advances past the inserted text. Works on any editable control, id'd
    /// or not (the old id-keyed append required an `id`).
    pub fn type_into_focused(&mut self, text: &str) -> bool {
        let Some(node) = self.editable_focus() else {
            return false;
        };
        if self.composition.is_some() {
            return self.update_marked_text(text);
        }
        let value = self.read_control_value(node);
        let len = value.chars().count();
        let caret = self.caret.unwrap_or(len).min(len);
        let (start, end) = self.active_text_selection(len).unwrap_or((caret, caret));
        let next = splice_chars(&value, start, end, text);
        if !self.write_control_value(node, &next) {
            return false;
        }
        self.clear_text_selection();
        self.selection = None;
        self.caret = Some(start + text.chars().count());
        self.relayout();
        #[cfg(feature = "page-js")]
        self.dispatch_js_edit_events(node, text);
        true
    }

    // -- H2: file uploads -----------------------------------------------------

    /// The key under which a file input's picks are stored: its `id`
    /// attribute, falling back to `name` (form_data's identity rule for
    /// unnamed widgets would strand the pick, so both are accepted here).
    fn file_pick_key(&self, node: NodeId) -> Option<String> {
        let page = self.page.as_ref()?;
        let el = page.dom.element(node)?;
        let key = el
            .attr("id")
            .or_else(|| el.attr("name"))
            .filter(|s| !s.is_empty())?;
        Some(key.to_string())
    }

    /// H2: a clicked `input[type=file]` asks the host to run its open panel.
    /// Wire: `filepick\t{id-or-name}\t{0|1}` (1 = the control has
    /// `multiple`). The host answers with the SET_FILE_FILES command; a
    /// control without either attribute is not addressable and is skipped.
    fn request_file_pick(&mut self, node: NodeId) {
        let Some(key) = self.file_pick_key(node) else {
            return;
        };
        let multiple = self
            .page
            .as_ref()
            .and_then(|p| p.dom.element(node))
            .map(|e| e.attr("multiple").is_some())
            .unwrap_or(false);
        self.host
            .notify(&format!("filepick\t{key}\t{}", u8::from(multiple)));
    }

    /// H2: the host's answer to a `filepick` notification. Records the
    /// picked paths for the submit path and mirrors a display string into
    /// the control's `value` attribute, which the layout paints (basenames
    /// joined with ", "; the count for `multiple` controls with several
    /// picks). Empty `paths` clears both.
    pub fn set_file_files(&mut self, element_id: &str, paths: &[String]) -> bool {
        let Some(node) = self.find_file_input(element_id) else {
            return false;
        };
        if paths.is_empty() {
            self.file_picks.remove(element_id);
        } else {
            self.file_picks
                .insert(element_id.to_string(), paths.to_vec());
        }
        let display = match paths.len() {
            0 => String::new(),
            1 => paths[0].rsplit('/').next().unwrap_or(&paths[0]).to_string(),
            _ => format!("{} files", paths.len()),
        };
        if let Some(page) = self.page.as_mut() {
            page.dom.set_attr(node, "value", &display);
        }
        self.relayout();
        true
    }

    /// The `input[type=file]` matching `element_id` (id first, then name —
    /// the same resolution [`Self::set_input_value`]'s callers expect).
    fn find_file_input(&self, element_id: &str) -> Option<NodeId> {
        let page = self.page.as_ref()?;
        let node = page
            .dom
            .get_element_by_id(element_id)
            .or_else(|| self.first_element_by_name(element_id))?;
        let el = page.dom.element(node)?;
        if el.tag.as_str() != "input"
            || el.attr("type").unwrap_or("").to_ascii_lowercase() != "file"
        {
            return None;
        }
        Some(node)
    }

    /// First element with `name="…"`, document order — the file-pick
    /// fallback identity for controls that only carry a name.
    fn first_element_by_name(&self, name: &str) -> Option<NodeId> {
        let page = self.page.as_ref()?;
        let root = page.dom.root()?;
        let mut found = None;
        page.dom.walk(root, &mut |n| {
            if found.is_some() {
                return;
            }
            if page
                .dom
                .element(n)
                .map(|e| e.attr("name") == Some(name))
                .unwrap_or(false)
            {
                found = Some(n);
            }
        });
        found
    }

    pub fn form_data(&self, form_id: &str) -> Option<Vec<(String, String)>> {
        let form = self.page.as_ref()?.dom.get_element_by_id(form_id)?;
        self.form_data_for_node(form)
    }

    /// [`Self::form_data`] keyed by node: the agent plane resolves a
    /// selector to a node, and a form it fills need not carry an `id`.
    pub fn form_data_for_node(&self, form: NodeId) -> Option<Vec<(String, String)>> {
        let page = self.page.as_ref()?;
        if page.dom.tag(form) != "form" {
            return None;
        }
        let mut out = Vec::new();
        page.dom.walk(form, &mut |n| {
            if n == form {
                return;
            }
            let Some(el) = page.dom.element(n) else {
                return;
            };
            if el.attr("disabled").is_some() {
                return;
            }
            let name = match el.attr("name") {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return,
            };
            match el.tag.as_str() {
                "input" => {
                    let t = el.attr("type").unwrap_or("text").to_ascii_lowercase();
                    match t.as_str() {
                        "submit" | "button" | "image" | "file" => return,
                        "checkbox" | "radio" => {
                            if el.attr("checked").is_none() {
                                return;
                            }
                            out.push((name, el.attr("value").unwrap_or("on").to_string()));
                        }
                        _ => out.push((name, el.attr("value").unwrap_or("").to_string())),
                    }
                }
                "textarea" => {
                    let mut txt = String::new();
                    for &c in page.dom.children(n) {
                        let node = page.dom.node(c);
                        if matches!(node.kind, NodeKind::Text) {
                            txt.push_str(&node.text);
                        }
                    }
                    out.push((name, txt));
                }
                "select" => {
                    let mut picked: Option<String> = None;
                    let mut first: Option<String> = None;
                    for &c in page.dom.children(n) {
                        if page.dom.tag(c) != "option" {
                            continue;
                        }
                        let mut txt = String::new();
                        for &g in page.dom.children(c) {
                            let node = page.dom.node(g);
                            if matches!(node.kind, NodeKind::Text) {
                                txt.push_str(&node.text);
                            }
                        }
                        let val = page
                            .dom
                            .element(c)
                            .and_then(|e| e.attr("value"))
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| txt.trim().to_string());
                        if first.is_none() {
                            first = Some(val.clone());
                        }
                        if page
                            .dom
                            .element(c)
                            .map(|e| e.attr("selected").is_some())
                            .unwrap_or(false)
                        {
                            picked = Some(val);
                            break;
                        }
                    }
                    out.push((name, picked.or(first).unwrap_or_default()));
                }
                _ => {}
            }
        });
        Some(out)
    }

    /// A10: every invalid control inside the form, in document order.
    /// Same predicate the `:invalid` pseudo-class uses, so the UI and the
    /// submit gate can never disagree.
    pub fn form_failures(&self, form_id: &str) -> Vec<NodeId> {
        let Some(form) = self
            .page
            .as_ref()
            .and_then(|p| p.dom.get_element_by_id(form_id))
        else {
            return Vec::new();
        };
        self.form_failures_for_node(form)
    }

    /// [`Self::form_failures`] keyed by node.
    pub fn form_failures_for_node(&self, form: NodeId) -> Vec<NodeId> {
        let Some(page) = self.page.as_ref() else {
            return Vec::new();
        };
        if page.dom.tag(form) != "form" {
            return Vec::new();
        }
        let mut out = Vec::new();
        page.dom.walk(form, &mut |n| {
            if n == form {
                return;
            }
            if kitae_style::control_invalid(&page.dom, n) {
                out.push(n);
            }
        });
        out
    }

    /// H2: build a `multipart/form-data` body for `form_id`. Field rules
    /// mirror [`Self::form_data`] (disabled skipped, unnamed skipped,
    /// checkbox/radio only when checked, textarea text, select option);
    /// file inputs additionally contribute one part per picked file, with
    /// the file's bytes, basenames and a sniffed content type. Returns the
    /// full `multipart/form-data; boundary=…` header value plus the body.
    pub fn multipart_body(&self, form_id: &str) -> Option<(String, Vec<u8>)> {
        let form = self.page.as_ref()?.dom.get_element_by_id(form_id)?;
        self.multipart_body_for_node(form)
    }

    /// [`Self::multipart_body`] keyed by node.
    pub fn multipart_body_for_node(&self, form: NodeId) -> Option<(String, Vec<u8>)> {
        let page = self.page.as_ref()?;
        if page.dom.tag(form) != "form" {
            return None;
        }
        // Boundary: high-resolution time plus a counter — no external RNG,
        // and a collision is practically impossible within one submission.
        static BOUNDARY_COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let n = BOUNDARY_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let boundary = format!("kitae-{nanos:016x}-{n:04x}");
        let dash = format!("--{boundary}");

        let mut body: Vec<u8> = Vec::new();
        let mut push_part = |name: &str, filename: Option<&str>, mime: &str, data: &[u8]| {
            body.extend_from_slice(dash.as_bytes());
            body.extend_from_slice(b"\r\n");
            let mut cd = format!(
                "Content-Disposition: form-data; name=\"{}\"",
                quote_header_value(name)
            );
            if let Some(filename) = filename {
                cd.push_str(&format!("; filename=\"{}\"", quote_header_value(filename)));
            }
            body.extend_from_slice(cd.as_bytes());
            body.extend_from_slice(b"\r\n");
            if !mime.is_empty() {
                body.extend_from_slice(format!("Content-Type: {mime}\r\n").as_bytes());
            }
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(data);
            body.extend_from_slice(b"\r\n");
        };

        page.dom.walk(form, &mut |node| {
            if node == form {
                return;
            }
            let Some(el) = page.dom.element(node) else {
                return;
            };
            if el.attr("disabled").is_some() {
                return;
            }
            let Some(field_name) = el.attr("name").filter(|s| !s.is_empty()) else {
                return;
            };
            match el.tag.as_str() {
                "input" => {
                    let t = el.attr("type").unwrap_or("text").to_ascii_lowercase();
                    match t.as_str() {
                        "submit" | "button" | "image" => {}
                        "checkbox" | "radio" => {
                            if el.attr("checked").is_some() {
                                push_part(
                                    field_name,
                                    None,
                                    "",
                                    el.attr("value").unwrap_or("on").as_bytes(),
                                );
                            }
                        }
                        "file" => {
                            // Picks are keyed by id (fallback name); with no
                            // pick at all the field still gets an empty part
                            // so the server sees the control.
                            let key = el
                                .attr("id")
                                .filter(|s| !s.is_empty())
                                .unwrap_or(field_name);
                            match self.file_picks.get(key) {
                                Some(paths) if !paths.is_empty() => {
                                    for path in paths {
                                        let bytes = std::fs::read(path).unwrap_or_default();
                                        let filename =
                                            path.rsplit('/').next().unwrap_or(path).to_string();
                                        push_part(
                                            field_name,
                                            Some(&filename),
                                            &guess_upload_mime(path),
                                            &bytes,
                                        );
                                    }
                                }
                                _ => push_part(field_name, Some(""), "", b""),
                            }
                        }
                        _ => push_part(
                            field_name,
                            None,
                            "",
                            el.attr("value").unwrap_or("").as_bytes(),
                        ),
                    }
                }
                "textarea" => {
                    let mut txt = String::new();
                    for &c in page.dom.children(node) {
                        let node = page.dom.node(c);
                        if matches!(node.kind, NodeKind::Text) {
                            txt.push_str(&node.text);
                        }
                    }
                    push_part(field_name, None, "", txt.as_bytes());
                }
                "select" => {
                    let mut picked: Option<String> = None;
                    let mut first: Option<String> = None;
                    for &c in page.dom.children(node) {
                        if page.dom.tag(c) != "option" {
                            continue;
                        }
                        let mut txt = String::new();
                        for &g in page.dom.children(c) {
                            let node = page.dom.node(g);
                            if matches!(node.kind, NodeKind::Text) {
                                txt.push_str(&node.text);
                            }
                        }
                        let val = page
                            .dom
                            .element(c)
                            .and_then(|e| e.attr("value"))
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| txt.trim().to_string());
                        if first.is_none() {
                            first = Some(val.clone());
                        }
                        if page
                            .dom
                            .element(c)
                            .map(|e| e.attr("selected").is_some())
                            .unwrap_or(false)
                        {
                            picked = Some(val);
                            break;
                        }
                    }
                    let val = picked.or(first).unwrap_or_default();
                    push_part(field_name, None, "", val.as_bytes());
                }
                _ => {}
            }
        });
        body.extend_from_slice(format!("{dash}--\r\n").as_bytes());
        Some((format!("multipart/form-data; boundary={boundary}"), body))
    }

    pub fn submit_form(&mut self, form_id: &str) -> Option<String> {
        let form = self
            .page
            .as_ref()
            .and_then(|p| p.dom.get_element_by_id(form_id))?;
        self.submit_form_for_node(form)
    }

    /// [`Self::submit_form`] keyed by node — the agent plane's `submit`
    /// resolves a selector to a node, and a form need not carry an `id`.
    /// One behavioural note: the H6 password-save prompt is keyed by
    /// element id (it feeds the host's keychain flow), so a form without an
    /// id submits without offering that prompt. Nothing about the
    /// submission itself depends on the id.
    pub fn submit_form_for_node(&mut self, form: NodeId) -> Option<String> {
        // page-js: the page's own `submit` listeners run first; a
        // preventDefault() hands the submission to the script — the engine
        // performs no navigation.
        #[cfg(feature = "page-js")]
        if self.page_js_enabled
            && self.page_js.is_some()
            && self.dispatch_js_event_at("submit", form, None)
        {
            return None;
        }
        let (action, method, enctype) = {
            let page = self.page.as_ref()?;
            if page.dom.tag(form) != "form" {
                return None;
            }
            let el = page.dom.element(form);
            let action = el.and_then(|e| e.attr("action")).unwrap_or("").to_string();
            let method = el
                .and_then(|e| e.attr("method"))
                .unwrap_or("get")
                .to_ascii_lowercase();
            // H2: `multipart/form-data` POSTs (the only enctype with engine
            // meaning — GETs always urlencode into the query) now build a
            // real multipart body; every other POST stays urlencoded.
            let enctype = el
                .and_then(|e| e.attr("enctype"))
                .unwrap_or("application/x-www-form-urlencoded")
                .to_ascii_lowercase();
            (action, method, enctype)
        };
        // A10: constraint validation blocks the submit. Invalid controls
        // get focus (first failure) so the host can show where; the relayout
        // repaints them with the `input:invalid` UA border.
        let failures = self.form_failures_for_node(form);
        if !failures.is_empty() {
            self.reset_editing_for_focus();
            self.focused = Some(failures[0]);
            self.sync_caret();
            self.relayout();
            return None;
        }
        // H6: a filled password input in a submitting form offers the host
        // a save — before the navigation wipes the page's values. Id-keyed
        // (see the note above); skipped when the form has no id.
        let id = self
            .page
            .as_ref()
            .and_then(|p| p.dom.element(form))
            .and_then(|e| e.attr("id"))
            .unwrap_or("")
            .to_string();
        if !id.is_empty() {
            self.notify_password_save(&id);
        }
        let data = self.form_data_for_node(form).unwrap_or_default();
        let encoded = data
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    percent_encoding::utf8_percent_encode(k, percent_encoding::NON_ALPHANUMERIC),
                    percent_encoding::utf8_percent_encode(v, percent_encoding::NON_ALPHANUMERIC)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let base = if action.is_empty() {
            self.page_url.clone()
        } else {
            let resource_base = self
                .page
                .as_ref()
                .map(|page| page.resource_base_url.clone())
                .unwrap_or_else(|| self.page_url.clone());
            resolve(&resource_base, &action).unwrap_or_else(|| action.clone())
        };
        if method == "post" {
            if enctype.starts_with("multipart/") {
                if let Some((content_type, body)) = self.multipart_body_for_node(form) {
                    return self.submit_post_bytes(&base, &body, &content_type);
                }
            }
            return self.submit_post(&base, &encoded);
        }
        let url = if encoded.is_empty() {
            base
        } else if base.contains('?') {
            format!("{base}&{encoded}")
        } else {
            format!("{base}?{encoded}")
        };
        self.navigate(&url);
        Some(url)
    }

    /// POST a urlencoded form body (Phase O1) and render the response as the
    /// new page. The action URL enters history like a navigation, but the
    /// response is never cached (POST semantics) — it renders straight from
    /// the returned bytes through the same response mapping as GET.
    fn submit_post(&mut self, action_url: &str, body: &str) -> Option<String> {
        self.submit_post_bytes(
            action_url,
            body.as_bytes(),
            "application/x-www-form-urlencoded",
        )
    }

    /// H2: POST `body` with an explicit content type — the urlencoded form
    /// path and the multipart upload path share this pipeline (response
    /// mapping, history, media teardown all identical).
    fn submit_post_bytes(
        &mut self,
        action_url: &str,
        body: &[u8],
        content_type: &str,
    ) -> Option<String> {
        if kitae_net::warn_insecure_http() && kitae_net::is_insecure_navigation(action_url) {
            let message = action_url
                .replace('\t', "")
                .replace('\r', "")
                .replace('\n', "");
            self.pending_navigation = Some(PendingNavigation::Post {
                url: action_url.to_string(),
                body: body.to_vec(),
                content_type: content_type.to_string(),
            });
            self.host.notify(&format!("insecure\t{message}"));
            return None;
        }
        self.submit_post_bytes_unchecked(action_url, body, content_type)
    }

    fn submit_post_bytes_unchecked(
        &mut self,
        action_url: &str,
        body: &[u8],
        content_type: &str,
    ) -> Option<String> {
        self.host.set_loading(true);
        self.focused = None;
        self.hover = None;
        self.active = None;
        self.selection = None;
        self.caret = None;
        self.selection_range = None;
        self.composition = None;
        self.media_session = (false, 0.0);
        let old_page_for_media = self.page_url.clone();
        self.find_active = None;
        self.find_matches.clear();
        release_qtsq_page_state(&old_page_for_media);
        clear_web_media_for(&old_page_for_media);
        let outcome = fetch_with(
            "POST",
            action_url,
            &[("Content-Type".to_string(), content_type.to_string())],
            Some(body),
        );
        if let Err(FetchError::InsecureRedirect(target)) = &outcome {
            let target = target.clone();
            self.request_navigation(target, PendingHistory::Push);
            return None;
        }
        let ((html, base_url), doc_base, result, locked, sig) = match outcome {
            Err(e) => (
                (
                    error_page_html(action_url, &e.to_string()),
                    action_url.to_string(),
                ),
                None,
                Err(e),
                false,
                QtsqSigState::Unsigned,
            ),
            Ok(resp) => render_response_state(
                action_url,
                resp,
                self.qtsq_password.as_ref().map(|z| z.as_str()),
            ),
        };
        // H1: a POST response can be a download too — claim it for this tab.
        for id in take_begun_downloads() {
            self.download_owned.insert(id);
        }
        self.push_history(action_url.to_string());
        self.scroll_y = 0.0;
        self.load_rendered(
            html,
            &base_url,
            doc_base.as_deref(),
            action_url,
            result,
            locked,
            sig,
        );
        self.host.set_loading(false);
        Some(action_url.to_string())
    }

    pub fn activate_focused(&mut self) -> bool {
        let Some(f) = self.focused else {
            return false;
        };
        let (tag, href, input_type, ancestor_form) = {
            let Some(page) = self.page.as_ref() else {
                return false;
            };
            let el = page.dom.element(f);
            let tag = page.dom.tag(f).to_string();
            let href = el.and_then(|e| e.attr("href")).map(|s| s.to_string());
            let input_type = if tag == "input" {
                el.and_then(|e| e.attr("type"))
                    .unwrap_or("text")
                    .to_ascii_lowercase()
            } else {
                String::new()
            };
            let mut form = None;
            let mut cur = page.dom.node(f).parent;
            while let Some(p) = cur {
                if page.dom.tag(p) == "form" {
                    form = Some(p);
                    break;
                }
                cur = page.dom.node(p).parent;
            }
            (tag, href, input_type, form)
        };
        match tag.as_str() {
            "a" => {
                let Some(h) = href else {
                    return false;
                };
                let resource_base = self
                    .page
                    .as_ref()
                    .map(|page| page.resource_base_url.clone())
                    .unwrap_or_else(|| self.page_url.clone());
                let Some(abs) = resolve(&resource_base, &h) else {
                    return false;
                };
                if let Some(target) = abs.strip_prefix("kitae:open-external:") {
                    if open_external_permitted(&self.gate_origin(), &abs) {
                        self.host.open_external(target);
                    } else {
                        self.host.open_external("about:blank");
                    }
                    return true;
                }
                if abs.starts_with("mailto:") {
                    self.host.open_external(&abs);
                    return true;
                }
                if abs.starts_with("file://")
                    && !kitae_net::local_origin_allowed(&self.gate_origin())
                {
                    self.navigate_blocked_local(&abs);
                    return true;
                }
                self.navigate(&ddg_result_target(&abs).unwrap_or(abs));
                true
            }
            "input" if matches!(input_type.as_str(), "checkbox" | "radio") => {
                let is_set = self
                    .page
                    .as_ref()
                    .and_then(|p| p.dom.element(f))
                    .map(|e| e.attr("checked").is_some())
                    .unwrap_or(false);
                if let Some(page) = self.page.as_mut() {
                    if is_set {
                        page.dom.remove_attr(f, "checked");
                    } else {
                        if input_type == "radio" {
                            let group = page
                                .dom
                                .element(f)
                                .and_then(|e| e.attr("name"))
                                .map(|s| s.to_string());
                            if let Some(form) = ancestor_form {
                                let mut peers = Vec::new();
                                page.dom.walk(form, &mut |n| {
                                    if n != f && page.dom.tag(n) == "input" {
                                        let same = page
                                            .dom
                                            .element(n)
                                            .and_then(|e| e.attr("name"))
                                            .map(|s| s.to_string())
                                            == group
                                            && page
                                                .dom
                                                .element(n)
                                                .map(|e| {
                                                    e.attr("type")
                                                        .unwrap_or("")
                                                        .to_ascii_lowercase()
                                                        == "radio"
                                                })
                                                .unwrap_or(false);
                                        if same {
                                            peers.push(n);
                                        }
                                    }
                                });
                                for p in peers {
                                    page.dom.remove_attr(p, "checked");
                                }
                            }
                        }
                        page.dom.set_attr(f, "checked", "");
                    }
                }
                #[cfg(feature = "page-js")]
                {
                    // A toggle commits the control's state: input + change.
                    let _ = self.dispatch_js_event_at("input", f, None);
                    let _ = self.dispatch_js_event_at("change", f, None);
                }
                self.relayout();
                true
            }
            "input" if matches!(input_type.as_str(), "submit" | "image" | "button") => {
                match ancestor_form {
                    Some(form) => {
                        let id = self
                            .page
                            .as_ref()
                            .and_then(|p| p.dom.element(form))
                            .and_then(|e| e.attr("id"))
                            .map(|s| s.to_string());
                        match id {
                            Some(i) if !i.is_empty() => self.submit_form(&i).is_some(),
                            _ => false,
                        }
                    }
                    None => false,
                }
            }
            "button" => match ancestor_form {
                Some(form) => {
                    let id = self
                        .page
                        .as_ref()
                        .and_then(|p| p.dom.element(form))
                        .and_then(|e| e.attr("id"))
                        .map(|s| s.to_string());
                    match id {
                        Some(i) if !i.is_empty() => self.submit_form(&i).is_some(),
                        _ => false,
                    }
                }
                None => false,
            },
            "input" | "select" => {
                if tag == "input"
                    && !matches!(
                        input_type.as_str(),
                        "text" | "search" | "url" | "email" | "password" | "number" | "tel"
                    )
                {
                    return false;
                }
                match ancestor_form {
                    Some(form) => {
                        let id = self
                            .page
                            .as_ref()
                            .and_then(|p| p.dom.element(form))
                            .and_then(|e| e.attr("id"))
                            .map(|s| s.to_string());
                        match id {
                            Some(i) if !i.is_empty() => self.submit_form(&i).is_some(),
                            _ => false,
                        }
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    pub fn page_text(&self) -> String {
        let Some(page) = self.page.as_ref() else {
            return String::new();
        };
        let Some(root) = page.dom.root() else {
            return String::new();
        };
        let Some(body) = find_element(&page.dom, root, "body") else {
            return String::new();
        };
        let mut parts = Vec::new();
        page.dom.walk(body, &mut |n| {
            let node = page.dom.node(n);
            if matches!(node.kind, NodeKind::Text) {
                let t = node.text.split_whitespace().collect::<Vec<_>>().join(" ");
                if !t.is_empty() {
                    parts.push(t);
                }
            }
        });
        parts.join("\n")
    }

    pub fn select_all(&mut self) -> String {
        if let Some(node) = self.editable_focus() {
            let canceled = self.composition.is_some();
            self.cancel_composition_state();
            let value = self.read_control_value(node);
            let len = value.chars().count();
            self.selection_range = Some(TextSelection {
                anchor: 0,
                head: len,
            });
            let selected = self.selected_text(node, 0, len);
            self.selection = selected.clone();
            self.caret = Some(len);
            if canceled {
                self.relayout();
            } else {
                self.publish_current();
            }
            return selected.unwrap_or_default();
        }
        self.clear_text_selection();
        let text = self.page_text();
        self.selection = if text.is_empty() {
            None
        } else {
            Some(text.clone())
        };
        self.publish_current();
        text
    }

    pub fn clear_selection(&mut self) {
        let had_range = self.selection_range.take().is_some();
        if self.selection.is_some() || had_range {
            self.selection = None;
            self.publish_current();
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.selection.clone()
    }

    // -- find-in-page (Phase P1) ------------------------------------------

    /// Case-folded substring search over the last display list, in page
    /// order: adjacent runs on one line merge, so a match spanning word (or
    /// style) boundaries reports as one [`FindMatch`] with one rect per run
    /// it touches. Empty/blank queries match nothing; at most
    /// [`FIND_MAX_MATCHES`] matches report. Pure: no state changed.
    pub fn find_in_page(&self, query: &str) -> Vec<FindMatch> {
        let Some(dl) = self.last_dl.as_ref() else {
            return Vec::new();
        };
        self.find_in_dl(dl, query)
    }

    fn find_in_dl(&self, dl: &DisplayList, query: &str) -> Vec<FindMatch> {
        let q: Vec<char> = query.to_lowercase().chars().collect();
        if q.is_empty() || q.iter().all(|c| c.is_whitespace()) {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut i = 0;
        while i < dl.texts.len() && out.len() < FIND_MAX_MATCHES {
            // One line group: consecutive runs sharing a baseline.
            let mut j = i + 1;
            while j < dl.texts.len() && (dl.texts[j].y - dl.texts[i].y).abs() < 0.5 {
                j += 1;
            }
            self.find_in_group(&dl.texts[i..j], &q, &mut out);
            i = j;
        }
        out
    }

    /// Search one same-baseline run group for every non-overlapping
    /// occurrence of `q` (both already case-folded by the caller).
    fn find_in_group(&self, runs: &[TextRun], q: &[char], out: &mut Vec<FindMatch>) {
        // Folded concat with single join spaces; each char maps to
        // (run, char index). A join space maps to the end of the run before
        // it, so it contributes no width of its own.
        let folded: Vec<Vec<char>> = runs
            .iter()
            .map(|r| r.text.to_lowercase().chars().collect())
            .collect();
        let mut concat: Vec<char> = Vec::new();
        let mut map: Vec<(usize, usize)> = Vec::new();
        for (ri, fr) in folded.iter().enumerate() {
            if ri > 0 {
                concat.push(' ');
                map.push((ri - 1, folded[ri - 1].len()));
            }
            for (ci, &ch) in fr.iter().enumerate() {
                concat.push(ch);
                map.push((ri, ci));
            }
        }
        let mut from = 0;
        while from + q.len() <= concat.len() && out.len() < FIND_MAX_MATCHES {
            let Some(rel) = concat[from..].windows(q.len()).position(|w| w == q) else {
                break;
            };
            let start = from + rel;
            let end = start + q.len(); // exclusive
            let (rs, _) = map[start];
            let (re, _) = map[end - 1];
            let mut rects = Vec::new();
            for ri in rs..=re {
                let run = &runs[ri];
                let fr = &folded[ri];
                let a = if ri == rs { map[start].1 } else { 0 };
                let b = if ri == re {
                    map[end - 1].1 + 1
                } else {
                    fr.len()
                };
                let (a, b) = (a.min(fr.len()), b.min(fr.len()));
                if a >= b {
                    continue;
                }
                let prefix: String = fr[..a].iter().collect();
                let part: String = fr[a..b].iter().collect();
                let x0 = run.x
                    + self.measurer.measure(
                        &prefix,
                        run.size,
                        run.weight,
                        run.italic,
                        run.monospace,
                    );
                let w =
                    self.measurer
                        .measure(&part, run.size, run.weight, run.italic, run.monospace);
                if w <= 0.0 {
                    continue;
                }
                let (ascent, descent) = self.measurer.metrics(run.size, run.monospace);
                rects.push(FindRect {
                    x: x0,
                    y: run.y - ascent,
                    w,
                    h: ascent + descent,
                });
            }
            if !rects.is_empty() {
                let index = out.len();
                out.push(FindMatch { rects, index });
            }
            from = end;
        }
    }

    /// Re-resolve matches against a fresh display list (relayout): the query
    /// survives, the active index sticks when still valid, otherwise it
    /// falls back to the first match (or inactive). Never activates from
    /// inactive — stepping is the user's gesture.
    fn refresh_find_for(&mut self, dl: &DisplayList) {
        let q = self.find_query.clone();
        self.find_matches = if q.trim().is_empty() {
            Vec::new()
        } else {
            self.find_in_dl(dl, &q)
        };
        match self.find_active {
            Some(a) if a < self.find_matches.len() => {}
            Some(_) => {
                self.find_active = if self.find_matches.is_empty() {
                    None
                } else {
                    Some(0)
                };
            }
            None => {}
        }
    }

    /// Current find query (survives navigation; re-resolved per layout).
    pub fn find_query(&self) -> &str {
        &self.find_query
    }

    /// Current matches in page order (recomputed on every layout).
    pub fn find_matches(&self) -> &[FindMatch] {
        &self.find_matches
    }

    /// Active match index (`None` when the query is empty or matched
    /// nothing, or after [`Browser::clear_find`]).
    pub fn find_active(&self) -> Option<usize> {
        self.find_active
    }

    /// Set the find query, resolve it against the current page, select the
    /// first match and republish (no relayout — geometry is unchanged).
    pub fn set_find_query(&mut self, query: &str) {
        self.find_query = query.to_string();
        self.find_active = None;
        if let Some(dl) = self.last_dl.clone() {
            self.refresh_find_for(&dl);
        } else {
            self.find_matches.clear();
        }
        if !self.find_matches.is_empty() {
            self.find_active = Some(0);
        }
        self.publish_current();
    }

    /// Step the active match forward (wrapping). No-op when nothing matched.
    pub fn find_next(&mut self) {
        if self.find_matches.is_empty() {
            return;
        }
        self.find_active = Some(match self.find_active {
            Some(a) => (a + 1) % self.find_matches.len(),
            None => 0,
        });
        self.publish_current();
    }

    /// Step the active match backward (wrapping). No-op when nothing matched.
    pub fn find_prev(&mut self) {
        if self.find_matches.is_empty() {
            return;
        }
        self.find_active = Some(match self.find_active {
            Some(0) | None => self.find_matches.len() - 1,
            Some(a) => a - 1,
        });
        self.publish_current();
    }

    /// Clear the query, matches and highlight; republishes.
    pub fn clear_find(&mut self) {
        if self.find_query.is_empty() && self.find_matches.is_empty() {
            return;
        }
        self.find_query.clear();
        self.find_matches.clear();
        self.find_active = None;
        self.publish_current();
    }

    // -- accessibility tree (Phase P2) ------------------------------------

    /// The last-built accessibility tree (Document-rooted; rebuilt on every
    /// relayout). Roles follow the tag map in [`build_ax_tree`]; labels come
    /// from text content, `alt`, `placeholder`, `value` or `aria-label`;
    /// bounds cite the display list (a zero rect means no geometry resolved
    /// — the v1 limitation for label-less form controls, whose boxes carry
    /// no node identity on the display-list contract).
    pub fn accessibility_tree(&self) -> &[AxNode] {
        &self.ax_tree
    }

    /// AX id of the focused node, if it made the tree.
    pub fn accessibility_focused(&self) -> Option<&str> {
        self.ax_focused.as_deref()
    }

    /// Sample the page's declarative animations at `t_secs` (page-load clock,
    /// Phase N8): `(node, property, value)` overrides for the embedder to
    /// apply when republishing (interpolation lives here; timelines and
    /// reduced-motion gating live embedder-side). Empty when nothing is due.
    pub fn sample_animations(&self, t_secs: f32) -> Vec<(NodeId, String, String)> {
        let Some(page) = self.page.as_ref() else {
            return Vec::new();
        };
        // Keyframe rules ride the author sheets; parse once per call.
        let mut keyframes = Vec::new();
        if let Some(root) = page.dom.root() {
            let sources = collect_styles_in_env(
                &page.dom,
                root,
                &page.base_url,
                self.viewport_w,
                self.viewport_h,
                &self.environment,
            );
            let (sheets, _) = load_stylesheets_in_env(
                &sources,
                &page.base_url,
                &page.resource_base_url,
                self.viewport_w,
                self.viewport_h,
                &self.environment,
                MAX_EXTERNAL_STYLESHEETS,
                MAX_EXTERNAL_STYLESHEET_BYTES,
            );
            for sheet in sheets {
                keyframes.extend(kitae_css::parse_stylesheet(&sheet).keyframes);
            }
        }
        let mut out = Vec::new();
        for (node, style) in &page.styles {
            for anim in &style.animations {
                if let Some(frames) = keyframes.iter().find(|k| k.name == anim.name) {
                    for (prop, value) in kitae_style::sample_animation(anim, frames, t_secs) {
                        out.push((*node, prop, value));
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Engine pipeline (shared by every host, tests included)
// ---------------------------------------------------------------------------

/// One author stylesheet found in a document: a `<style>` block's text or a
/// `<link rel=stylesheet>` target (already resolved against the resource base).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StyleSource {
    Inline(String),
    External(String),
}

const MAX_EXTERNAL_STYLESHEETS: usize = 64;
const MAX_EXTERNAL_STYLESHEET_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMPORT_DEPTH: usize = 8;

/// Collect author CSS sources in document order. Pure: links are resolved,
/// not fetched.
/// A2: true when `node` sits inside a `<template>` — inert content (the
/// no-JS approximation: children stay in the DOM but never activate).
fn inside_template(dom: &Dom, mut node: NodeId) -> bool {
    loop {
        let parent = match dom.node(node).parent {
            Some(p) => p,
            None => return false,
        };
        if dom.tag(parent) == "template" {
            return true;
        }
        node = parent;
    }
}

/// A6: true when `node` is `ancestor` or a descendant of it — a parent-chain
/// walk (same idiom as [`inside_template`]).
fn within(dom: &Dom, mut node: NodeId, ancestor: NodeId) -> bool {
    loop {
        if node == ancestor {
            return true;
        }
        match dom.node(node).parent {
            Some(p) => node = p,
            None => return false,
        }
    }
}

/// A6: the topmost open `<dialog>` in tree order — the one modal
/// interactivity gates on. `None` when no dialog is open.
fn open_dialog(dom: &Dom, root: NodeId) -> Option<NodeId> {
    let mut found = None;
    dom.walk(root, &mut |n| {
        if dom.tag(n) == "dialog" && dom.element(n).is_some_and(|e| e.attr("open").is_some()) {
            found = Some(n);
        }
    });
    found
}

/// A10 finish: paint the focused control's validation bubble onto the
/// finished display list (a post-pass — appended rects/texts sit on top, so
/// no DisplayList version bump and no layout signature change). Gated on the
/// control actually being invalid, the same predicate the submit gate uses.
fn paint_error_bubble(
    dl: &mut kitae_layout::DisplayList,
    dom: &Dom,
    node: NodeId,
    viewport_w: f32,
    tm: Option<&dyn kitae_layout::TextMeasurer>,
) {
    if !kitae_style::control_invalid(dom, node) {
        return;
    }
    let msg = kitae_style::control_invalid_message(dom, node);
    if msg.is_empty() {
        return;
    }
    // The control's box = its widest node-stamped rect (borders paint as
    // separate 1px segments; the box face is the big one).
    let Some(boxr) = dl
        .rects
        .iter()
        .filter(|r| r.node == node.0 && r.w > 2.0 && r.h > 2.0)
        .max_by(|a, b| {
            (a.w * a.h)
                .partial_cmp(&(b.w * b.h))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
    else {
        return;
    };
    let (tw, ascent) = match tm {
        Some(tm) => {
            let tw = tm.measure_family(&msg, 12.0, 400, false, false, "");
            let (asc, _) = tm.metrics_family(12.0, false, "");
            (tw, asc)
        }
        None => (msg.chars().count() as f32 * 6.0, 9.6),
    };
    let bw = (tw + 16.0).min(viewport_w.max(24.0));
    let bh = 22.0;
    let bx = boxr.x.clamp(0.0, (viewport_w - bw).max(0.0));
    let by = boxr.y + boxr.h + 2.0;
    dl.rects
        .push(kitae_layout::Rect::solid(bx, by, bw, bh, 0x202124FF));
    dl.rects.last_mut().unwrap().node = node.0;
    // nub pointing at the control
    dl.rects.push(kitae_layout::Rect::solid(
        boxr.x + 10.0,
        by - 4.0,
        8.0,
        4.0,
        0x202124FF,
    ));
    dl.rects.last_mut().unwrap().node = node.0;
    dl.texts.push(kitae_layout::TextRun {
        x: bx + 8.0,
        y: by + bh * 0.5 + ascent * 0.35,
        text: msg,
        size: 12.0,
        color: 0xFFFFFFFF,
        node: node.0,
        ..Default::default()
    });
}

pub fn collect_styles(dom: &Dom, root: NodeId, base_url: &str) -> Vec<StyleSource> {
    collect_styles_in_env(
        dom,
        root,
        base_url,
        1280.0,
        0.0,
        &EnvironmentState::default(),
    )
}

fn collect_styles_in_env(
    dom: &Dom,
    root: NodeId,
    base_url: &str,
    viewport_width: f32,
    viewport_height: f32,
    env: &EnvironmentState,
) -> Vec<StyleSource> {
    let resource_base = resource_base_url(dom, Some(root), base_url);
    let mut out = Vec::new();
    dom.walk(root, &mut |id| {
        // A2: templates are inert — sheets inside never apply.
        if inside_template(dom, id) {
            return;
        }
        let el = dom.element(id);
        let media = el.and_then(|el| el.attr("media"));
        if !media_allows(media, viewport_width, viewport_height, env) {
            return;
        }
        match dom.tag(id) {
            "style" => {
                let css: String = dom
                    .children(id)
                    .iter()
                    .filter_map(|&c| {
                        let node = dom.node(c);
                        matches!(node.kind, NodeKind::Text).then(|| node.text.clone())
                    })
                    .collect();
                if !css.trim().is_empty() {
                    out.push(StyleSource::Inline(css));
                }
            }
            "link" => {
                let rel = el
                    .and_then(|el| el.attr("rel"))
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !rel.split_ascii_whitespace().any(|w| w == "stylesheet") {
                    return;
                }
                let Some(href) = el.and_then(|el| el.attr("href")) else {
                    return;
                };
                if let Some(abs) = resolve(&resource_base, href) {
                    out.push(StyleSource::External(abs));
                }
            }
            _ => {}
        }
    });
    out
}

fn media_allows(
    media: Option<&str>,
    viewport_width: f32,
    viewport_height: f32,
    env: &EnvironmentState,
) -> bool {
    match media.map(str::trim).filter(|media| !media.is_empty()) {
        Some(media) => {
            kitae_css::eval_media_condition_size(media, viewport_width, viewport_height, env)
        }
        None => true,
    }
}

/// Turn sources into CSS text: inline blocks pass through, external sheets
/// are fetched (cached like any other resource). Fetch failures are ignored —
/// a missing sheet renders as if it were absent, never as a page error.
pub fn load_stylesheets(sources: &[StyleSource], page_base: &str) -> Vec<String> {
    load_stylesheets_with_stats(sources, page_base).0
}

pub fn load_stylesheets_with_stats(
    sources: &[StyleSource],
    page_base: &str,
) -> (Vec<String>, StylesheetLoadStats) {
    load_stylesheets_in_env(
        sources,
        page_base,
        page_base,
        1280.0,
        0.0,
        &EnvironmentState::default(),
        MAX_EXTERNAL_STYLESHEETS,
        MAX_EXTERNAL_STYLESHEET_BYTES,
    )
}

struct StylesheetLoadState<'a> {
    sheets: Vec<String>,
    seen: Vec<String>,
    stats: StylesheetLoadStats,
    document_base: &'a str,
    viewport_width: f32,
    viewport_height: f32,
    env: EnvironmentState,
    max_count: usize,
    max_bytes: usize,
}

impl StylesheetLoadState<'_> {
    fn load_external(&mut self, url: &str) -> Option<String> {
        if self.seen.iter().any(|seen| seen == url) {
            self.stats.duplicate_count = self.stats.duplicate_count.saturating_add(1);
            return None;
        }
        self.seen.push(url.to_string());
        if !kitae_net::mixed_content_allowed(self.document_base, url) {
            self.stats.mixed_content_blocked = self.stats.mixed_content_blocked.saturating_add(1);
            return None;
        }
        if self.stats.external_candidates as usize >= self.max_count {
            self.stats.skipped_count = self.stats.skipped_count.saturating_add(1);
            return None;
        }
        self.stats.external_candidates = self.stats.external_candidates.saturating_add(1);
        let response = match fetch_cached(url) {
            Ok(response) if response.status < 400 => response,
            _ => {
                self.stats.failed_count = self.stats.failed_count.saturating_add(1);
                return None;
            }
        };
        let response_bytes = response.body.len() as u64;
        if self.stats.external_bytes.saturating_add(response_bytes) > self.max_bytes as u64 {
            self.stats.skipped_count = self.stats.skipped_count.saturating_add(1);
            self.stats.skipped_bytes = self.stats.skipped_bytes.saturating_add(response_bytes);
            return None;
        }
        self.stats.external_loaded = self.stats.external_loaded.saturating_add(1);
        self.stats.external_bytes = self.stats.external_bytes.saturating_add(response_bytes);
        Some(response.text())
    }
}

fn load_stylesheets_in_env(
    sources: &[StyleSource],
    document_base: &str,
    resource_base: &str,
    viewport_width: f32,
    viewport_height: f32,
    env: &EnvironmentState,
    max_count: usize,
    max_bytes: usize,
) -> (Vec<String>, StylesheetLoadStats) {
    let mut state = StylesheetLoadState {
        sheets: Vec::new(),
        seen: Vec::new(),
        stats: StylesheetLoadStats::default(),
        document_base,
        viewport_width,
        viewport_height,
        env: *env,
        max_count,
        max_bytes,
    };
    for source in sources {
        match source {
            StyleSource::Inline(css) => {
                expand_imports(css, resource_base, &mut state, 0);
            }
            StyleSource::External(url) => {
                let url = resolve(resource_base, url).unwrap_or_else(|| url.clone());
                if let Some(css) = state.load_external(&url) {
                    expand_imports(&css, &url, &mut state, 0);
                }
            }
        }
    }
    (state.sheets, state.stats)
}

fn expand_imports(css: &str, base: &str, state: &mut StylesheetLoadState<'_>, depth: usize) {
    let mut rest = css;
    loop {
        let trimmed = rest.trim_start();
        // Skip leading comments so `/* x */ @import` still expands.
        let mut text = trimmed;
        while let Some(after) = text.strip_prefix("/*") {
            text = match after.find("*/") {
                Some(at) => after[at + 2..].trim_start(),
                None => after,
            };
        }
        let Some(without_keyword) = text.strip_prefix("@import") else {
            break;
        };
        // The statement ends at the first ';' (import values carry no
        // strings-with-semicolon edge worth handling at this level).
        let Some(end) = without_keyword.find(';') else {
            break;
        };
        let statement = without_keyword[..end].trim();
        rest = &without_keyword[end + 1..];

        // Target: a quoted string or url(...) — the wrapper is optional.
        // `url(...)` closes at the *first* `)` after the quote-aware body,
        // so a media condition after the wrapper is not swallowed into the
        // target (`@import url(x.css) (min-width: 700px)` must fetch
        // `x.css`, not `x.css) (min-width: 700px`).
        let (target, media) = if let Some(inner) = statement.strip_prefix("url(") {
            let inner = inner.trim_start();
            let quote = inner.chars().next().filter(|c| *c == '"' || *c == '\'');
            let (target, after) = match quote {
                Some(q) => {
                    let body = &inner[1..];
                    let Some(close) = body.find(q) else {
                        break;
                    };
                    let Some(after) = body[close + 1..].trim_start().strip_prefix(')') else {
                        break;
                    };
                    (&body[..close], after)
                }
                None => {
                    let Some(close) = inner.find(')') else { break };
                    (inner[..close].trim(), &inner[close + 1..])
                }
            };
            (target.trim(), after.trim())
        } else if statement.starts_with('"') || statement.starts_with('\'') {
            let quote = statement.as_bytes()[0] as char;
            let Some(close) = statement[1..].find(quote) else {
                break;
            };
            (&statement[1..1 + close], statement[1 + close + 1..].trim())
        } else {
            match statement.find(char::is_whitespace) {
                Some(at) => (statement[..at].trim(), statement[at..].trim()),
                None => (statement, ""),
            }
        };
        if !media_allows(
            (!media.is_empty()).then_some(media),
            state.viewport_width,
            state.viewport_height,
            &state.env,
        ) || depth >= MAX_IMPORT_DEPTH
        {
            continue;
        }
        let abs = resolve(base, target).unwrap_or_else(|| target.to_string());
        if let Some(css) = state.load_external(&abs) {
            expand_imports(&css, &abs, state, depth + 1);
        }
    }
    let rest = rest.trim_start();
    if !rest.is_empty() {
        state.sheets.push(rest.to_string());
    }
}

/// Parse, apply the JS-content policy, run the style cascade with the page's
/// own CSS (no base URL, default viewport — see [`prepare_in`]).
pub fn prepare(html: &str) -> (Dom, Option<NodeId>, Vec<(NodeId, ComputedStyle)>) {
    prepare_in(html, "", 1280.0)
}

/// [`prepare`] with the document's base URL (for `<link>` resolution) and the
/// layout viewport width (for `@media` evaluation).
pub fn prepare_in(
    html: &str,
    base_url: &str,
    viewport_width: f32,
) -> (Dom, Option<NodeId>, Vec<(NodeId, ComputedStyle)>) {
    prepare_in_env(html, base_url, viewport_width, &EnvironmentState::default())
}

/// [`prepare_in`] with a concrete embedder environment (Q6): the `prefers-*`
/// media features evaluate against it.
pub fn prepare_in_env(
    html: &str,
    base_url: &str,
    viewport_width: f32,
    env: &EnvironmentState,
) -> (Dom, Option<NodeId>, Vec<(NodeId, ComputedStyle)>) {
    let (mut dom, root) = parse(html);
    if let Some(root) = root {
        prefer_noscript(&mut dom, root);
        let sources = collect_styles_in_env(&dom, root, base_url, viewport_width, 0.0, env);
        let (sheets, _) = load_stylesheets_in_env(
            &sources,
            base_url,
            &resource_base_url(&dom, Some(root), base_url),
            viewport_width,
            0.0,
            env,
            MAX_EXTERNAL_STYLESHEETS,
            MAX_EXTERNAL_STYLESHEET_BYTES,
        );
        let refs: Vec<&str> = sheets.iter().map(String::as_str).collect();
        // Headless default height 600 for vh folding (no real viewport).
        let styles = StyleEngine::with_environment(&refs, viewport_width, 600.0, env).compute(&dom);
        return (dom, Some(root), styles);
    }
    let styles = StyleEngine::with_environment(&[], viewport_width, 600.0, env).compute(&dom);
    (dom, root, styles)
}

/// Fetch a URL and turn the response into renderable HTML. Also returns the
/// base URL for resolving links (redirect-followed). Render-level failures
/// become error pages; the typed [`FetchError`] is reported to the host when
/// the load path reports one (see [`Browser::load_with_outcome`]).
pub fn fetch_step(url: &str) -> (String, String) {
    fetch_result(url).0
}

/// Like [`fetch_step`] but also reports the typed outcome: `Ok(_)` for a
/// rendered document (including synthesized error pages at the render
/// layer), `Err(e)` when the fetch itself failed and an error page was
/// substituted. `(html, base_url, outcome)`.
pub fn fetch_result(url: &str) -> ((String, String), Result<(), FetchError>) {
    let ((html, base), _, outcome, _, _) = fetch_result_state(url, None);
    ((html, base), outcome)
}

/// [`fetch_result`] with a QTSQ password: encrypted .qtsq payloads are
/// decrypted with it when present; `None` keeps the passwordless behavior
/// (device-key files still open). The existing public fn wraps this with
/// `None` so dumps/tests stay on the plain path. Also reports whether the
/// produced page is a locked-QTSQ info page (drives PublicState.qtsq_locked
/// and frame v9).
pub fn fetch_result_full(
    url: &str,
    password: Option<&str>,
) -> ((String, String), Result<(), FetchError>) {
    let ((html, base), _, outcome, _, _) = fetch_result_state(url, password);
    ((html, base), outcome)
}

fn fetch_result_state(
    url: &str,
    password: Option<&str>,
) -> (
    (String, String),
    Option<String>,
    Result<(), FetchError>,
    bool,
    QtsqSigState,
) {
    // Top-level fetches are host-trusted (S-1, SECURITY_AUDIT.md): link
    // clicks were already gated in `click_link`/`app_click`, so whatever
    // reaches here — a user navigation, back/forward, or a direct
    // `fetch_step` call — records *itself* as the fetch context. Without
    // this, a `file://` document fetch would be judged against whatever
    // origin the process-wide static happens to hold (another browser
    // instance's page in tests, or a stale one after an error page).
    kitae_net::set_page_origin(url);
    if url == START_URL {
        return (
            (start_page_html(), url.to_string()),
            None,
            Ok(()),
            false,
            QtsqSigState::Unsigned,
        );
    }
    if url == "about:blank" {
        return (
            (String::new(), url.to_string()),
            None,
            Ok(()),
            false,
            QtsqSigState::Unsigned,
        );
    }
    match fetch_cached(url) {
        Err(e) => (
            (error_page_html(url, &e.to_string()), url.to_string()),
            None,
            Err(e),
            false,
            QtsqSigState::Unsigned,
        ),
        Ok(resp) => render_response_state(url, resp, password),
    }
}

/// Map one fetched response to renderable HTML (shared by GET navigation
/// and POST submits — Phase O1). Returns ((html, base_url), outcome).
fn render_response(url: &str, resp: Response) -> ((String, String), Result<(), FetchError>) {
    let ((html, base), _, outcome, _, _) = render_response_state(url, resp, None);
    ((html, base), outcome)
}

/// [`render_response`] with a QTSQ password threaded to the viewer; the
/// trailing bool reports a locked-QTSQ info page and the [`QtsqSigState`]
/// the payload's signature verdict (`Unsigned` off the QTSQ path). The
/// `Option<String>` between them is the saved-page archive's base override
/// (slice ②): the base to lay the document out against when it differs from
/// the fetched URL.
fn render_response_state(
    url: &str,
    resp: Response,
    password: Option<&str>,
) -> (
    (String, String),
    Option<String>,
    Result<(), FetchError>,
    bool,
    QtsqSigState,
) {
    let base = resp.final_url.clone();
    // H6: a 401 with a challenge the engine speaks is a sign-in page, not
    // an error page — the host is asked through the `auth\t` notification
    // and answers with [`Browser::set_auth`], which stores the credentials
    // and re-fetches this URL. Unspoken schemes (NTLM/Negotiate) keep the
    // generic error page; the engine cannot retry those.
    if resp.status == 401 && !resp.www_authenticate.is_empty() {
        if let Some((scheme, realm)) = kitae_net::describe_challenge(&resp.www_authenticate) {
            let host = host_of_url(url);
            let scheme_note = match scheme {
                kitae_net::AuthScheme::Basic => {
                    note_auth_challenge(AuthPrompt {
                        url: url.to_string(),
                        host: host.clone(),
                        realm: realm.clone(),
                        scheme: "basic".to_string(),
                    });
                    "Basic"
                }
                kitae_net::AuthScheme::Digest => {
                    note_auth_challenge(AuthPrompt {
                        url: url.to_string(),
                        host: host.clone(),
                        realm: realm.clone(),
                        scheme: "digest".to_string(),
                    });
                    "Digest"
                }
            };
            let shown_host = escape_html(&host);
            let realm_note = if realm.is_empty() {
                String::new()
            } else {
                format!(" (realm “{}”)", escape_html(&realm))
            };
            let html = format!(
                "<!doctype html><html><head><title>Sign in — {shown_host}</title></head>\
<body><h1>Sign in</h1>\
<p>{shown_host}{realm_note} requires {scheme_note} authentication.</p>\
<p>Kitae will ask for your user name and password.</p></body></html>"
            );
            return ((html, base), None, Ok(()), false, QtsqSigState::Unsigned);
        }
    }
    if resp.status >= 400 {
        let err = FetchError::Http {
            status: resp.status,
        };
        (
            (error_page_html(url, &err.to_string()), url.to_string()),
            None,
            Err(err),
            false,
            QtsqSigState::Unsigned,
        )
    } else if resp.is_html() {
        (
            (resp.text(), base),
            None,
            Ok(()),
            false,
            QtsqSigState::Unsigned,
        )
    } else if resp.is_qtsq() {
        // QTSQ native-format documents: dispatch on the probed data
        // type. Images/text render immediately (text via decode);
        // audio/video get media viewer pages whose playback path
        // lands in J4/J5; everything else (tensor, brain, unknown,
        // encrypted) gets a metadata info page.
        let (body, base_override, locked, sig) = qtsq_viewer_page(url, &base, &resp.body, password);
        // A saved-page archive keeps the .qtsq file URL as the session URL
        // (address bar, reload) but resolves links and subresources against
        // the archived page's original base — hence the separate override.
        ((body, base), base_override, Ok(()), locked, sig)
    } else if resp.is_image() {
        // Direct image navigations render as a document containing
        // the image (real display-list item via layout).
        let src = escape_html(&base);
        let title = escape_html(url);
        // Intrinsic size from header decode when available, so the
        // page has stable geometry before the renderer fetches.
        let size_attr = kitae_layout::image_dimensions(&resp.body)
            .map(|(w, h)| format!(" width=\"{w}\" height=\"{h}\""))
            .unwrap_or_default();
        (
            (
                format!(
                    "<!doctype html><html><head><title>{title}</title></head>\
<body><p><img src=\"{src}\" alt=\"{title}\"{size_attr}></p></body></html>"
                ),
                base,
            ),
            None,
            Ok(()),
            false,
            QtsqSigState::Unsigned,
        )
    } else {
        // H1: something the engine cannot render (or that explicitly asked
        // to be saved) becomes a managed download — the already-fetched
        // body spills to the download directory in the background, progress
        // reaches the host through [`Browser::poll_downloads`], and the tab
        // shows an info page. The download URL is the history entry, so
        // Reload re-fetches it like any other page.
        if is_download_response(&resp) {
            let kitae_net::Response {
                body,
                content_type,
                content_disposition,
                ..
            } = resp;
            let name = kitae_net::suggested_filename(&base, &content_disposition);
            let id = kitae_net::begin_download(&base, body, &content_type, &name);
            note_begun_download(id);
            let title = escape_html(url);
            let shown_name = escape_html(&name);
            let mime_note = if content_type.is_empty() {
                "unknown type".to_string()
            } else {
                escape_html(&content_type)
            };
            return (
                (download_page_html(&title, &shown_name, &mime_note), base),
                None,
                Ok(()),
                false,
                QtsqSigState::Unsigned,
            );
        }
        // Plain text and unknown types render as preformatted text.
        let body = escape_html(&resp.text());
        let title = escape_html(url);
        (
            (
                format!(
                    "<!doctype html><html><head><title>{title}</title></head>\
<body><pre>{body}</pre></body></html>"
                ),
                base,
            ),
            None,
            Ok(()),
            false,
            QtsqSigState::Unsigned,
        )
    }
}

/// H1: does this response belong in the download manager instead of the
/// renderer? An explicit `attachment` disposition wins (even for types the
/// engine could render); otherwise anything outside the renderable set
/// downloads. An empty content type falls back to a UTF-8 sniff — decodable
/// bytes render as text, the rest is binary. (QTSQ never reaches here: the
/// magic-sniffing `is_qtsq` branch runs first.)
fn is_download_response(resp: &Response) -> bool {
    let cd = resp.content_disposition.to_ascii_lowercase();
    if cd.split(';').next().unwrap_or("").trim() == "attachment" {
        return true;
    }
    let ct = resp.content_type.to_ascii_lowercase();
    let base = ct.split(';').next().unwrap_or("").trim();
    if base.is_empty() {
        return std::str::from_utf8(&resp.body).is_err();
    }
    let renderable = base.starts_with("text/")
        || base == "application/xhtml+xml"
        || base == "application/json"
        || base == "application/xml"
        || base.ends_with("+xml")
        || base.starts_with("image/")
        || base == "application/x-qtsq";
    !renderable
}

/// H1: the download manager's registry is process-global, but each download
/// must be narrated by exactly one tab — the one whose load began it.
/// `render_response_state` (a free function, no Browser in reach) records
/// every id it begins here; the Browser method that drove the load drains
/// the ids into `download_owned`, and `poll_downloads` announces only owned
/// downloads. One engine thread per Browser, so the handoff is thread-clean.
fn note_begun_download(id: u32) {
    BEGUN_DOWNLOADS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(id);
}

static BEGUN_DOWNLOADS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

fn take_begun_downloads() -> Vec<u32> {
    std::mem::take(
        BEGUN_DOWNLOADS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut(),
    )
}

/// H6: one 401 challenge awaiting the host's answer. `scheme` is the
/// engine-spoken token (`basic` / `digest`) — `describe_challenge` parses
/// only those, so anything else never reaches the sign-in page.
#[derive(Debug, Clone)]
struct AuthPrompt {
    url: String,
    host: String,
    realm: String,
    scheme: String,
}

/// H6: challenges noted by `render_response_state` (a free function, no
/// Browser in reach) — the Browser method that drove the load drains them
/// into `pending_auth` and notifies the host. Same handoff as H1
/// downloads: one engine thread per Browser, so the queue is thread-clean.
static AUTH_CHALLENGES: std::sync::Mutex<Vec<AuthPrompt>> = std::sync::Mutex::new(Vec::new());

fn note_auth_challenge(prompt: AuthPrompt) {
    AUTH_CHALLENGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(prompt);
}

fn take_auth_challenges() -> Vec<AuthPrompt> {
    std::mem::take(
        AUTH_CHALLENGES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut(),
    )
}

/// H6: the host[:port] of an absolute URL (userinfo stripped; "" when the
/// URL has no authority) — the key session credentials are stored under.
fn host_of_url(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    authority.to_string()
}

/// H6: `scheme://authority` — the origin the host keys its password store
/// on. file URLs have no authority and key on the scheme alone.
fn origin_of(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
            format!("{scheme}://{authority}")
        }
        None => url.to_string(),
    }
}

/// H1: the tab's info page while/after a download runs. Deliberately plain —
/// no link back to the source (clicking it would re-fetch and spawn a
/// second copy); Reload and the app's Downloads panel are the affordances.
fn download_page_html(title: &str, name: &str, mime: &str) -> String {
    format!(
        "<!doctype html><html><head><title>{title}</title></head><body>\
<h1>Downloading {name}</h1>\
<p>Kitae saved this file ({mime}) to your downloads folder. \
Open the Downloads panel from the toolbar to see its progress.</p>\
</body></html>"
    )
}

/// H2: percent-escape the two characters that would break a quoted
/// `Content-Disposition` parameter (plus line breaks) — everything else,
/// including spaces, stays legible for server-side parsing.
fn quote_header_value(value: &str) -> String {
    value
        .replace('\\', "%5C")
        .replace('"', "%22")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// H2: a content type for a picked file, by extension. The set mirrors the
/// file:// transport's table (kitae_net); unknown extensions upload as
/// `application/octet-stream`.
fn guess_upload_mime(path: &str) -> String {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "txt" | "md" => "text/plain",
        "json" => "application/json",
        "xml" => "application/xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "mp3" => "audio/mpeg",
        "mp4" | "m4v" => "video/mp4",
        "qtsq" => "application/x-qtsq",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// §4.5 of the brief: pages that gate content behind JavaScript ship the real
/// text in <noscript>. When the script-visible body is nearly empty, swap the
/// noscript content in — that is the version a script-free reader wants.
pub fn prefer_noscript(dom: &mut Dom, root: NodeId) -> bool {
    let Some(body) = find_element(dom, root, "body") else {
        return false;
    };
    let Some(noscript) = find_element(dom, root, "noscript") else {
        return false;
    };
    let body_text = text_len(dom, body);
    let noscript_text = text_len(dom, noscript);
    if body_text.saturating_sub(noscript_text) >= 200 {
        return false;
    }
    let kids: Vec<NodeId> = dom.children(noscript).to_vec();
    if kids.is_empty() {
        return false;
    }
    for kid in kids {
        dom.detach(noscript, kid);
        dom.append(body, kid);
    }
    if let Some(parent) = dom.node(noscript).parent {
        dom.detach(parent, noscript);
    }
    true
}

/// §4.5: after the noscript preference the body is still essentially empty
/// (< 100 chars) and the page ships ≥ 2 scripts — a JS-only shell.
pub fn needs_interstitial(dom: &Dom, root: NodeId) -> bool {
    let Some(body) = find_element(dom, root, "body") else {
        return false;
    };
    text_len(dom, body) < 100 && count_elements(dom, root, "script") >= 2
}

pub fn page_title(dom: &Dom, root: NodeId) -> Option<String> {
    let title = find_element(dom, root, "title")?;
    let text: String = dom
        .children(title)
        .iter()
        .filter_map(|&c| {
            let node = dom.node(c);
            matches!(node.kind, NodeKind::Text).then(|| node.text.clone())
        })
        .collect();
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!t.is_empty()).then_some(t)
}

// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

pub fn find_element(dom: &Dom, root: NodeId, tag: &str) -> Option<NodeId> {
    let mut found = None;
    dom.walk(root, &mut |id| {
        if found.is_none() && dom.tag(id) == tag {
            found = Some(id);
        }
    });
    found
}

/// Does this document carry the kitae_live marker (`<meta name="kitae-live">`
/// with content "1")? The kitae_live server stamps every static page; the
/// engine opens the Live socket only for pages that asked for it.
fn is_live_page(dom: &Dom) -> bool {
    let mut found = false;
    dom.walk(dom.root().unwrap_or(NodeId(0)), &mut |id| {
        if dom.tag(id) == "meta" {
            if let Some(el) = dom.element(id) {
                if el.attr("name") == Some("kitae-live") && el.attr("content") == Some("1") {
                    found = true;
                }
            }
        }
    });
    found
}

/// Build the accessibility tree (Phase P2): one Document root (labelled
/// with the page title) over the `<body>` subtree. Returns the forest, the
/// AX id of `focused` (when it made the tree), and every built element's
/// AX id for later focus re-resolution without a relayout.
///
/// * Roles map from tags (headings, links, buttons, checkboxes, textboxes,
///   combo boxes, images with `alt`, tables; everything else is a generic
///   container). `script`/`style`/`head`-family subtrees and
///   `display: none` elements are pruned.
/// * Labels prefer `aria-label`, then `alt`/`placeholder`/`value`, then the
///   whitespace-collapsed text content (truncated to 200 chars).
/// * Bounds cite the display list: images and links match by `src`/`href`,
///   everything else greedily matches its words against still-unclaimed
///   text runs in document order (runs are per-word, so multi-word labels
///   union several runs); containers union their children. Whatever matches
///   nothing keeps a zero rect.
fn build_ax_tree(
    dom: &Dom,
    title: &str,
    focused: Option<NodeId>,
    dl: &DisplayList,
    styles: &[(NodeId, ComputedStyle)],
    measurer: &dyn TextMeasurer,
) -> (
    Vec<AxNode>,
    Option<String>,
    std::collections::HashMap<NodeId, String>,
) {
    let hidden: std::collections::HashSet<NodeId> = styles
        .iter()
        .filter(|(_, s)| s.display == kitae_style::Display::None)
        .map(|(n, _)| *n)
        .collect();
    let mut ctx = AxCtx {
        dl,
        measurer,
        used_runs: vec![false; dl.texts.len()],
        counter: 0,
        node_ids: std::collections::HashMap::new(),
    };
    let start = dom
        .root()
        .and_then(|r| find_element(dom, r, "body"))
        .or_else(|| dom.root());
    let mut children = Vec::new();
    if let Some(root) = start {
        for &kid in dom.children(root) {
            if let Some(node) = ax_element(&mut ctx, dom, &hidden, kid) {
                children.push(node);
            }
        }
    }
    let doc = AxNode {
        id: "document".to_string(),
        role: AxRole::Document,
        label: title.to_string(),
        x: 0.0,
        y: 0.0,
        w: 0.0,
        h: dl.content_height,
        value: None,
        value_min: None,
        value_max: None,
        disabled: false,
        checked: AxChecked::Unspecified,
        expanded: None,
        selected: None,
        heading_level: 0,
        live_politeness: AxLivePoliteness::Off,
        children,
    };
    let focused_ax = focused.and_then(|f| ctx.node_ids.get(&f).cloned());
    (vec![doc], focused_ax, ctx.node_ids)
}

/// Scratch state for one [`build_ax_tree`] pass.
struct AxCtx<'a> {
    dl: &'a DisplayList,
    measurer: &'a dyn TextMeasurer,
    /// Text runs already claimed by an earlier (document-order) node.
    used_runs: Vec<bool>,
    counter: usize,
    /// Every built element's AX id, for the focused mapping.
    node_ids: std::collections::HashMap<NodeId, String>,
}

/// Tags whose subtrees never enter the tree (chrome, metadata, script).
fn ax_pruned(tag: &str) -> bool {
    matches!(
        tag,
        "head" | "script" | "style" | "meta" | "link" | "title" | "noscript" | "template"
    )
}

fn ax_role_for(tag: &str, el: &kitae_dom::ElementData) -> AxRole {
    if let Some(role) = el.attr("role") {
        let role = role
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match role.as_str() {
            "document" => return AxRole::Document,
            "heading" => return AxRole::Heading,
            "link" => return AxRole::Link,
            "button" => return AxRole::Button,
            "checkbox" => return AxRole::Checkbox,
            "radio" => return AxRole::RadioButton,
            "textbox" => return AxRole::Textbox,
            "combobox" => return AxRole::ComboBox,
            "slider" => return AxRole::Slider,
            "spinbutton" => return AxRole::SpinButton,
            "progressbar" => return AxRole::ProgressIndicator,
            "img" | "image" => return AxRole::Image,
            "table" => return AxRole::Table,
            "text" | "paragraph" => return AxRole::Text,
            "group" | "region" => return AxRole::Group,
            "list" => return AxRole::List,
            "listitem" => return AxRole::ListItem,
            "navigation" => return AxRole::Navigation,
            "main" => return AxRole::Main,
            "banner" => return AxRole::Banner,
            "contentinfo" => return AxRole::ContentInfo,
            "complementary" => return AxRole::Complementary,
            "search" => return AxRole::Search,
            "status" => return AxRole::Status,
            "alert" => return AxRole::Alert,
            "article" => return AxRole::Article,
            "form" => return AxRole::Form,
            "separator" => return AxRole::Separator,
            "switch" => return AxRole::Switch,
            "radiogroup" => return AxRole::RadioGroup,
            "toolbar" => return AxRole::Toolbar,
            "row" => return AxRole::Row,
            "cell" => return AxRole::Cell,
            "columnheader" => return AxRole::ColumnHeader,
            "rowheader" => return AxRole::RowHeader,
            "dialog" => return AxRole::Dialog,
            "disclosuretriangle" => return AxRole::DisclosureTriangle,
            "term" => return AxRole::Term,
            "definition" => return AxRole::Definition,
            "timer" => return AxRole::Timer,
            "scrollbar" => return AxRole::ScrollBar,
            _ => {}
        }
    }
    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => AxRole::Heading,
        "a" => {
            if el.attr("href").is_some() {
                AxRole::Link
            } else {
                AxRole::Group
            }
        }
        "button" => AxRole::Button,
        "input" => match el
            .attr("type")
            .unwrap_or("text")
            .to_ascii_lowercase()
            .as_str()
        {
            "checkbox" => AxRole::Checkbox,
            "radio" => AxRole::RadioButton,
            "range" => AxRole::Slider,
            "number" => AxRole::SpinButton,
            "color" => AxRole::ColorWell,
            "submit" | "button" | "image" | "reset" | "file" => AxRole::Button,
            "hidden" => AxRole::Group,
            _ => AxRole::Textbox,
        },
        "select" => AxRole::ComboBox,
        "textarea" => AxRole::TextArea,
        "option" => AxRole::ListItem,
        "ul" | "ol" => AxRole::List,
        "li" => AxRole::ListItem,
        "img" => AxRole::Image,
        "table" => AxRole::Table,
        "tr" => AxRole::Row,
        "td" => AxRole::Cell,
        "th" => {
            if el
                .attr("scope")
                .is_some_and(|scope| scope.eq_ignore_ascii_case("row"))
            {
                AxRole::RowHeader
            } else {
                AxRole::ColumnHeader
            }
        }
        "progress" | "meter" => AxRole::ProgressIndicator,
        "nav" => AxRole::Navigation,
        "main" => AxRole::Main,
        "header" => AxRole::Banner,
        "footer" => AxRole::ContentInfo,
        "aside" => AxRole::Complementary,
        "form" => AxRole::Form,
        "article" => AxRole::Article,
        "hr" => AxRole::Separator,
        "dialog" => AxRole::Dialog,
        "summary" => AxRole::DisclosureTriangle,
        "output" => AxRole::Status,
        "label" | "legend" | "time" | "strong" | "b" | "em" | "i" | "code" | "mark" | "small"
        | "sub" | "sup" => AxRole::Text,

        _ => AxRole::Group,
    }
}

/// Whitespace-collapsed text content of a subtree, truncated to 200 chars.
fn ax_text_content(dom: &Dom, node: NodeId) -> String {
    let mut words = Vec::new();
    dom.walk(node, &mut |n| {
        if matches!(dom.node(n).kind, NodeKind::Text) {
            words.extend(dom.node(n).text.split_whitespace().map(str::to_string));
        }
    });
    let mut label = words.join(" ");
    if label.chars().count() > 200 {
        label = label.chars().take(200).collect();
    }
    label
}

/// Label for one element: `aria-label` first, then the per-role attribute,
/// then text content.
fn ax_label(dom: &Dom, node: NodeId, tag: &str, el: &kitae_dom::ElementData) -> String {
    if let Some(l) = el.attr("aria-label") {
        if !l.trim().is_empty() {
            return l.split_whitespace().collect::<Vec<_>>().join(" ");
        }
    }
    if let Some(ids) = el.attr("aria-labelledby") {
        let label = ids
            .split_whitespace()
            .filter_map(|id| dom.get_element_by_id(id))
            .map(|id| ax_text_content(dom, id))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !label.is_empty() {
            return label;
        }
    }
    if let Some(label_id) = el.attr("for") {
        if let Some(label_node) = dom.get_element_by_id(label_id) {
            let label = ax_text_content(dom, label_node);
            if !label.is_empty() {
                return label;
            }
        }
    }
    if let Some(parent) = dom.node(node).parent {
        if dom.tag(parent).eq_ignore_ascii_case("label") {
            let label = ax_text_content(dom, parent);
            if !label.is_empty() {
                return label;
            }
        }
    }
    if let Some(id) = el.attr("id") {
        let mut associated = None;
        if let Some(root) = dom.root() {
            dom.walk(root, &mut |candidate| {
                if dom.tag(candidate).eq_ignore_ascii_case("label")
                    && dom.element(candidate).and_then(|e| e.attr("for")) == Some(id)
                {
                    associated = Some(candidate);
                }
            });
        }
        if let Some(label_node) = associated {
            let label = ax_text_content(dom, label_node);
            if !label.is_empty() {
                return label;
            }
        }
    }
    match tag {
        "img" => el.attr("alt").unwrap_or("").to_string(),
        "input" => {
            let t = el.attr("type").unwrap_or("text").to_ascii_lowercase();
            match t.as_str() {
                "checkbox" | "radio" | "submit" | "button" | "reset" => el
                    .attr("value")
                    .or(el.attr("aria-label"))
                    .unwrap_or("")
                    .to_string(),
                "hidden" => String::new(),
                "password" => el
                    .attr("placeholder")
                    .filter(|v| !v.is_empty())
                    .unwrap_or("Password")
                    .to_string(),
                _ => el
                    .attr("value")
                    .filter(|v| !v.is_empty())
                    .or(el.attr("placeholder"))
                    .unwrap_or("")
                    .to_string(),
            }
        }
        _ => ax_text_content(dom, node),
    }
}

fn ax_aria_bool(value: Option<&str>) -> Option<bool> {
    match value.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("true") || value == "1" => Some(true),
        Some(value) if value.eq_ignore_ascii_case("false") || value == "0" => Some(false),
        _ => None,
    }
}

fn ax_checked(tag: &str, el: &kitae_dom::ElementData) -> AxChecked {
    if let Some(value) = el.attr("aria-checked") {
        return match value.trim() {
            value if value.eq_ignore_ascii_case("true") || value == "1" => AxChecked::Checked,
            value if value.eq_ignore_ascii_case("false") || value == "0" => AxChecked::Unchecked,
            value if value.eq_ignore_ascii_case("mixed") => AxChecked::Mixed,
            _ => AxChecked::Unspecified,
        };
    }
    if tag == "input" {
        let input_type = el.attr("type").unwrap_or("text").to_ascii_lowercase();
        if input_type == "checkbox" || input_type == "radio" {
            return if el.attr("checked").is_some() {
                AxChecked::Checked
            } else {
                AxChecked::Unchecked
            };
        }
    }
    AxChecked::Unspecified
}

fn ax_live_politeness(role: AxRole, el: &kitae_dom::ElementData) -> AxLivePoliteness {
    match el.attr("aria-live").map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("assertive") => AxLivePoliteness::Assertive,
        Some(value) if value.eq_ignore_ascii_case("polite") => AxLivePoliteness::Polite,
        Some(value) if value.eq_ignore_ascii_case("off") || value.is_empty() => {
            AxLivePoliteness::Off
        }
        Some(_) => AxLivePoliteness::Polite,
        None => match role {
            AxRole::Alert => AxLivePoliteness::Assertive,
            AxRole::Status => AxLivePoliteness::Polite,
            _ => AxLivePoliteness::Off,
        },
    }
}

fn ax_heading_level(tag: &str, role: AxRole, el: &kitae_dom::ElementData) -> u8 {
    let native = match tag {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    };
    native
        .or_else(|| {
            (role == AxRole::Heading)
                .then(|| {
                    el.attr("aria-level")
                        .and_then(|v| v.trim().parse::<u8>().ok())
                })
                .flatten()
                .filter(|level| (1..=6).contains(level))
        })
        .unwrap_or(0)
}

fn ax_disabled(dom: &Dom, node: NodeId, el: &kitae_dom::ElementData) -> bool {
    if el.attr("disabled").is_some() || ax_aria_bool(el.attr("aria-disabled")) == Some(true) {
        return true;
    }
    let mut parent = dom.node(node).parent;
    while let Some(current) = parent {
        if dom.tag(current) == "fieldset"
            && dom
                .element(current)
                .and_then(|fieldset| fieldset.attr("disabled"))
                .is_some()
        {
            return true;
        }
        parent = dom.node(current).parent;
    }
    false
}

fn ax_expanded(dom: &Dom, node: NodeId, tag: &str, el: &kitae_dom::ElementData) -> Option<bool> {
    if let Some(value) = ax_aria_bool(el.attr("aria-expanded")) {
        return Some(value);
    }
    if tag == "summary" {
        if let Some(parent) = dom.node(node).parent {
            if dom.tag(parent) == "details" {
                return Some(
                    dom.element(parent)
                        .and_then(|details| details.attr("open"))
                        .is_some(),
                );
            }
        }
    }
    None
}

fn ax_selected(dom: &Dom, node: NodeId, tag: &str, el: &kitae_dom::ElementData) -> Option<bool> {
    if let Some(value) = ax_aria_bool(el.attr("aria-selected")) {
        return Some(value);
    }
    if tag != "option" {
        return None;
    }
    let parent = dom.node(node).parent?;
    if dom.tag(parent) != "select" {
        return None;
    }
    let selected = el.attr("selected").is_some();
    if selected {
        return Some(true);
    }
    let multiple = dom
        .element(parent)
        .and_then(|select| select.attr("multiple"))
        .is_some();
    if multiple {
        return Some(false);
    }
    let first = dom
        .children(parent)
        .iter()
        .copied()
        .find(|child| dom.tag(*child) == "option");
    Some(first == Some(node))
}

fn ax_number(value: Option<&str>) -> Option<f32> {
    value
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|value| value.is_finite())
}

fn ax_option_value(dom: &Dom, node: NodeId) -> Option<String> {
    let mut first = None;
    for child in dom.children(node) {
        if dom.tag(*child) != "option" {
            continue;
        }
        let value = dom
            .element(*child)
            .and_then(|option| option.attr("value"))
            .map(str::to_string)
            .unwrap_or_else(|| ax_text_content(dom, *child).trim().to_string());
        if first.is_none() {
            first = Some(value.clone());
        }
        if dom
            .element(*child)
            .and_then(|option| option.attr("selected"))
            .is_some()
        {
            return Some(value);
        }
    }
    first
}

fn ax_value(
    dom: &Dom,
    node: NodeId,
    tag: &str,
    el: &kitae_dom::ElementData,
) -> (Option<String>, Option<f32>, Option<f32>) {
    let mut value = None;
    let mut min = ax_number(el.attr("aria-valuemin"));
    let mut max = ax_number(el.attr("aria-valuemax"));

    if tag == "input" {
        let input_type = el.attr("type").unwrap_or("text").to_ascii_lowercase();
        if input_type == "password" {
            return (None, None, None);
        }
        if input_type != "checkbox" && input_type != "radio" {
            value = Some(el.attr("value").unwrap_or("").to_string());
        }
        if input_type == "range" {
            min = min.or_else(|| ax_number(el.attr("min")).or(Some(0.0)));
            max = max.or_else(|| ax_number(el.attr("max")).or(Some(100.0)));
        } else {
            min = min.or_else(|| ax_number(el.attr("min")));
            max = max.or_else(|| ax_number(el.attr("max")));
        }
    } else if tag == "textarea" {
        value = Some(ax_text_content(dom, node));
    } else if tag == "select" {
        value = Some(ax_option_value(dom, node).unwrap_or_default());
    } else if tag == "progress" || tag == "meter" {
        value = Some(el.attr("value").unwrap_or("0").to_string());
        max = max.or_else(|| ax_number(el.attr("max")).or(Some(1.0)));
    } else if el
        .attr("contenteditable")
        .is_some_and(|value| value != "false")
    {
        value = Some(ax_text_content(dom, node));
    }

    if let Some(text) = el.attr("aria-valuetext") {
        value = Some(text.to_string());
    } else if let Some(now) = el.attr("aria-valuenow") {
        value = Some(now.to_string());
    }
    min = min.or_else(|| ax_number(el.attr("min")));
    max = max.or_else(|| ax_number(el.attr("max")));
    (
        value.map(|value| value.chars().take(4096).collect()),
        min,
        max,
    )
}

/// Union `box` into `acc` (a zero rect absorbs and is absorbed).
fn ax_union(acc: &mut (f32, f32, f32, f32), b: (f32, f32, f32, f32)) {
    if b.2 <= 0.0 || b.3 <= 0.0 {
        return;
    }
    if acc.2 <= 0.0 || acc.3 <= 0.0 {
        *acc = b;
        return;
    }
    let x0 = acc.0.min(b.0);
    let y0 = acc.1.min(b.1);
    let x1 = (acc.0 + acc.2).max(b.0 + b.2);
    let y1 = (acc.1 + acc.3).max(b.1 + b.3);
    *acc = (x0, y0, x1 - x0, y1 - y0);
}

/// Page-space box of one text run (baseline `y` → top + height).
fn ax_run_box(ctx: &AxCtx, idx: usize) -> (f32, f32, f32, f32) {
    let run = &ctx.dl.texts[idx];
    let w = ctx
        .measurer
        .measure(&run.text, run.size, run.weight, run.italic, run.monospace);
    let (ascent, descent) = ctx.measurer.metrics(run.size, run.monospace);
    (run.x, run.y - ascent, w, ascent + descent)
}

/// Claim the next still-unused run whose text equals `word`, in document
/// order. Runs are per-word, so control/element labels made of rendered
/// words resolve run by run.
fn ax_claim_word(ctx: &mut AxCtx, word: &str) -> Option<(f32, f32, f32, f32)> {
    let idx =
        (0..ctx.dl.texts.len()).find(|&i| !ctx.used_runs[i] && ctx.dl.texts[i].text == word)?;
    ctx.used_runs[idx] = true;
    Some(ax_run_box(ctx, idx))
}

/// Build one element (and its subtree) into an [`AxNode`); `None` when the
/// subtree is pruned.
fn ax_element(
    ctx: &mut AxCtx,
    dom: &Dom,
    hidden: &std::collections::HashSet<NodeId>,
    node: NodeId,
) -> Option<AxNode> {
    if !matches!(dom.node(node).kind, NodeKind::Element) {
        return None;
    }
    let tag = dom.tag(node).to_string();
    if ax_pruned(&tag) || hidden.contains(&node) {
        return None;
    }
    if dom
        .element(node)
        .and_then(|element| element.attr("aria-hidden"))
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    {
        return None;
    }
    let el = dom.element(node);
    let empty_data;
    let el_data = match el {
        Some(e) => e,
        None => {
            empty_data = kitae_dom::ElementData {
                tag: tag.clone(),
                attributes: Vec::new(),
            };
            &empty_data
        }
    };
    if tag == "input"
        && el_data
            .attr("type")
            .unwrap_or("")
            .eq_ignore_ascii_case("hidden")
    {
        return None;
    }
    ctx.counter += 1;
    let id = el_data
        .attr("id")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("ax{}", ctx.counter));
    ctx.node_ids.insert(node, id.clone());

    let base_role = ax_role_for(&tag, el_data);
    let live_politeness = ax_live_politeness(base_role, el_data);
    let role = match el_data.attr("aria-live").map(str::trim) {
        Some(value) if !value.eq_ignore_ascii_case("off") && !value.is_empty() => {
            AxRole::LiveRegion
        }
        _ => base_role,
    };
    let heading_level = ax_heading_level(&tag, base_role, el_data);
    let disabled = ax_disabled(dom, node, el_data);
    let checked = ax_checked(&tag, el_data);
    let expanded = ax_expanded(dom, node, &tag, el_data);
    let selected = ax_selected(dom, node, &tag, el_data);
    let (value, value_min, value_max) = ax_value(dom, node, &tag, el_data);
    let label = ax_label(dom, node, &tag, el_data);

    // Children first (they claim their runs before containers union them).
    let mut children = Vec::new();
    let mut bounds = (0.0, 0.0, 0.0, 0.0);
    for &kid in dom.children(node) {
        if let Some(child) = ax_element(ctx, dom, hidden, kid) {
            ax_union(&mut bounds, (child.x, child.y, child.w, child.h));
            children.push(child);
        }
    }

    // Direct geometry: images and links match by identity; everything else
    // matches its own direct words (attribute labels for controls, text for
    // the rest — containers pick up inter-child words like "and").
    match role {
        AxRole::Image => {
            if let Some(src) = el_data.attr("src") {
                for im in &ctx.dl.images {
                    if im.src == src {
                        ax_union(&mut bounds, (im.x, im.y, im.w, im.h));
                    }
                }
            }
        }
        AxRole::Link => {
            if let Some(href) = el_data.attr("href") {
                for l in &ctx.dl.links {
                    if l.href == href {
                        ax_union(&mut bounds, (l.x, l.y, l.w, l.h));
                    }
                }
            }
        }
        _ => {}
    }
    if bounds.2 <= 0.0 {
        // No identity geometry (or no match): claim direct words. For
        // controls the label words (value/placeholder) are what layout
        // painted; for text elements the direct text-node words.
        let words: Vec<String> = if matches!(
            role,
            AxRole::Textbox
                | AxRole::TextArea
                | AxRole::Checkbox
                | AxRole::RadioButton
                | AxRole::Button
                | AxRole::ComboBox
                | AxRole::Slider
                | AxRole::SpinButton
                | AxRole::ColorWell
        ) {
            label.split_whitespace().map(str::to_string).collect()
        } else {
            dom.children(node)
                .iter()
                .filter(|&&c| matches!(dom.node(c).kind, NodeKind::Text))
                .flat_map(|&c| dom.node(c).text.split_whitespace().map(str::to_string))
                .collect()
        };
        for w in &words {
            if let Some(b) = ax_claim_word(ctx, w) {
                ax_union(&mut bounds, b);
            }
        }
    }

    Some(AxNode {
        id,
        role,
        label,
        x: bounds.0,
        y: bounds.1,
        w: bounds.2,
        h: bounds.3,
        value,
        value_min,
        value_max,
        disabled,
        checked,
        expanded,
        selected,
        heading_level,
        live_politeness,
        children,
    })
}

fn count_elements(dom: &Dom, root: NodeId, tag: &str) -> usize {
    let mut n = 0;
    dom.walk(root, &mut |id| {
        if dom.tag(id) == tag {
            n += 1;
        }
    });
    n
}

/// Total characters of text in a subtree (trimmed per text node).
pub fn text_len(dom: &Dom, root: NodeId) -> usize {
    let mut n = 0;
    dom.walk(root, &mut |id| {
        let node = dom.node(id);
        if matches!(node.kind, NodeKind::Text) {
            n += node.text.trim().chars().count();
        }
    });
    n
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// H5: the CSS spelling of a [`kitae_style::Display`] for the inspector
/// report (the enum's Debug form uses Rust casing).
fn inspect_display_name(d: kitae_style::Display) -> &'static str {
    match d {
        kitae_style::Display::None => "none",
        kitae_style::Display::Block => "block",
        kitae_style::Display::Inline => "inline",
        kitae_style::Display::InlineBlock => "inline-block",
        kitae_style::Display::ListItem => "list-item",
        kitae_style::Display::Flex => "flex",
        kitae_style::Display::Grid => "grid",
        kitae_style::Display::Table => "table",
        kitae_style::Display::TableRow => "table-row",
        kitae_style::Display::TableCell => "table-cell",
        kitae_style::Display::TableRowGroup => "table-row-group",
        kitae_style::Display::TableCaption => "table-caption",
        kitae_style::Display::TableColumn => "table-column",
        kitae_style::Display::TableColumnGroup => "table-column-group",
    }
}

/// H5: `rgb()` / `rgba()` spelling of a 0xRRGGBBAA packed color.
fn inspect_css_color(c: u32) -> String {
    let (r, g, b, a) = (
        (c >> 24) & 0xFF,
        (c >> 16) & 0xFF,
        (c >> 8) & 0xFF,
        c & 0xFF,
    );
    if a == 0xFF {
        format!("rgb({r}, {g}, {b})")
    } else {
        format!("rgba({r}, {g}, {b}, {:.2})", a as f32 / 255.0)
    }
}

/// Extract the real target from a DuckDuckGo HTML-endpoint result link
/// (`…/html/?uddg=https%3A%2F%2F…`). Non-http decodes stay untouched.
pub fn ddg_result_target(url: &str) -> Option<String> {
    let idx = url.find("uddg=")?;
    let rest = &url[idx + "uddg=".len()..];
    let end = rest.find('&').unwrap_or(rest.len());
    let decoded = percent_encoding::percent_decode_str(&rest[..end])
        .decode_utf8_lossy()
        .into_owned();
    (decoded.starts_with("http://") || decoded.starts_with("https://")).then_some(decoded)
}

// ---------------------------------------------------------------------------
// Synthetic pages
// ---------------------------------------------------------------------------

/// Canonical plain-HTML mirror of the kitae_ui start page
/// (`start_page::StartPage`), used for save/archive. The archive path has
/// no signals, so the demo button renders at "Count: 0"; keep the copy in
/// sync with the component tree.
pub fn start_page_html() -> String {
    "<!doctype html><html><head><title>Start</title>\
<style>\
body{margin:0;padding:0;background:#0e0e12;color:#ededf2;font-family:-apple-system,'Helvetica Neue',Arial,sans-serif}\
.hero{position:relative;overflow:hidden;text-align:center;height:100vh;display:flex;align-items:center;justify-content:center;padding:0 24px;background:linear-gradient(to bottom,#181826 0%,#101018 100%)}\
.hero-bg{position:absolute;left:0;top:0;width:100%;height:100vh}\
.hero-bg-img{width:100%;height:100vh;object-fit:fill}\
.hero-copy{position:relative;max-width:640px}\
.hero-title{font-family:Georgia,'Times New Roman',serif;font-size:64px;font-weight:600;margin:0 0 10px;text-shadow:0 2px 24px rgba(0,0,0,0.55)}\
.hero-italic{font-family:Georgia,'Times New Roman',serif;font-style:italic;font-size:30px;color:#d8d8e4;margin:0 0 26px}\
.hero-sub{font-size:16px;line-height:1.65;color:#a6a6b4;margin:0 auto;max-width:540px}\
.hero-curve{position:absolute;left:50%;margin-left:-1200px;bottom:-20px}\
.story{max-width:1280px;margin:0 auto;padding:0 48px}\
.screen{position:relative;min-height:100vh;display:flex;align-items:center;justify-content:center}\
.screen-pad{width:0;height:100vh}\
.chapter{width:100%;margin:0;text-align:center}\
.badge{display:inline-block;border:1px solid #3d3d4c;border-radius:999px;padding:6px 18px;font-size:12px;color:#b8b8c8;font-style:italic;font-family:Georgia,'Times New Roman',serif;letter-spacing:1px}\
.ch-title{font-family:Georgia,'Times New Roman',serif;font-size:40px;font-weight:600;margin:20px 0 44px}\
.topic-row{display:flex;flex-wrap:wrap;gap:32px;text-align:left}\
.topic{flex-grow:1;flex-shrink:1;flex-basis:480px;border:1px solid #26262f;border-radius:22px;padding:30px 32px 34px;background:#12121a;transition:transform 0.2s ease}\
.topic:hover{transform:translateY(-3px)}\
.topic-title{font-family:Georgia,'Times New Roman',serif;font-size:24px;font-weight:600;margin:0 0 18px}\
.card-note{color:#9a9aa8;font-size:14px;line-height:1.7;margin:0 0 16px}\
.pill-row{display:flex;flex-wrap:wrap;gap:10px;margin:2px 0 20px}\
.pill{display:inline-block;border:1px solid #34343f;border-radius:999px;padding:8px 16px;color:#cfcfda;text-decoration:none;font-size:14px;background:#1b1b24;white-space:nowrap;transition:transform 0.16s ease,background-color 0.16s ease,color 0.16s ease}\
.pill:hover{border-color:#e5484d;color:#ffffff;background:#232331;transform:translateY(-2px)}\
.key-row{display:flex;align-items:center;gap:12px;margin:0 0 14px}\
.key{ text-align:center;font-family:Menlo,monospace;font-size:13px;background:#20202b;border:1px solid #34343f;border-bottom-width:2px;border-radius:8px;padding:5px 14px;color:#e8e8f0;transition:background-color 0.15s ease,color 0.15s ease}\
.key:hover{background:#2b2b3a;color:#ffffff}\
.key-desc{color:#a8a8b6;font-size:14px}\
.kitae-card{border:1px solid #26262f;border-radius:14px;background:#16161f;padding:18px 20px 20px;margin:0 0 18px}\
.kitae-card h3{font-size:17px;font-weight:600;margin:0 0 6px}\
.kitae-card p{color:#9a9aa8;font-size:13px;line-height:1.6;margin:0 0 12px}\
.demo-btn{background:#e5484d;color:#ffffff;border:none;border-radius:999px;padding:9px 20px;font-size:14px;font-weight:600;transition:background-color 0.15s ease,transform 0.15s ease}\
.demo-btn:hover{background:#f0565b;transform:translateY(-1px)}\
.demo-btn:active{transform:translateY(1px)}\
.faq-wrap{max-width:920px;margin:0 auto;text-align:left}\
.faq{border:1px solid #26262f;border-radius:14px;background:#12121a;padding:16px 22px;margin:0 0 14px;text-align:left;transition:transform 0.18s ease}\
.faq:hover{transform:translateY(-2px)}\
.faq summary{cursor:pointer;font-weight:600;font-size:15px;color:#dedeea;transition:color 0.15s ease}\
.faq summary:hover{color:#ffffff}\
.faq .card-note{margin:10px 0 0}\
.site-footer{position:absolute;left:0;right:0;bottom:0;text-align:center;padding:26px 0 30px;border-top:1px solid #2a2a35}\
.colophon-tag{font-family:Georgia,'Times New Roman',serif;font-style:italic;font-size:22px;color:#c9c9d6;margin:0 0 10px}\
.colophon-fine{font-size:13px;color:#77778a;max-width:460px;margin:0 auto;line-height:1.6}\
</style></head><body>\
<header class=\"hero\"><div class=\"hero-bg\"><img class=\"hero-bg-img\" src=\"__KITAE_HERO_BG_QTSQ__\" alt=\"\"></div><div class=\"screen-pad\"></div>\
<div class=\"hero-copy\">\
<h1 class=\"hero-title\">Kitae, built the hard way.</h1>\
<p class=\"hero-italic\">No scripts. No shortcuts.</p>\
<p class=\"hero-sub\">A browser engine written in Rust with its own parser, its own layout and its own renderer — and no JavaScript anywhere in the loop. The chapters below are a short tour; this very page is the test bench it runs on.</p>\
</div><svg class=\"hero-curve\" width=\"2400\" height=\"240\" viewBox=\"0 0 2400 240\"><path d=\"M0 240 L0 150 Q 1200 -30 2400 150 L2400 240 Z\" fill=\"#0e0e12\"></path></svg></header>\
<main class=\"story\">\
<div class=\"screen\"><div class=\"screen-pad\"></div><section class=\"chapter\">\
<div class=\"badge\">How to use &middot; How it works</div>\
<h2 class=\"ch-title\">Get going, then look under the hood</h2>\
<div class=\"topic-row\">\
<div class=\"topic\">\
<h3 class=\"topic-title\">How to use</h3>\
<div class=\"pill-row\">\
<a class=\"pill\" href=\"https://example.com\">example.com</a>\
<a class=\"pill\" href=\"https://text.npr.org\">text.npr.org</a>\
<a class=\"pill\" href=\"https://lite.duckduckgo.com/lite\">DuckDuckGo Lite</a>\
<a class=\"pill\" href=\"https://en.wikipedia.org/wiki/Main_Page\">Wikipedia</a>\
<a class=\"pill\" href=\"https://wiby.me\">wiby.me</a>\
</div>\
<p class=\"card-note\">Visit any of these — no script runs anywhere in Kitae, so pages arrive as text, links and images. Plain words in the address bar search DuckDuckGo Lite.</p>\
<div class=\"key-row\"><div class=\"key\">&#8984;K</div><div class=\"key-desc\">Command palette — every tab and action, type to filter</div></div>\
<div class=\"key-row\"><div class=\"key\">&#8984;L</div><div class=\"key-desc\">Focus the address bar</div></div>\
<div class=\"key-row\"><div class=\"key\">&#8984;T</div><div class=\"key-desc\">New tab &middot; &#8984;&#8997;W closes the current one</div></div>\
<div class=\"key-row\"><div class=\"key\">&#8984;,</div><div class=\"key-desc\">Settings</div></div>\
</div>\
<div class=\"topic\">\
<h3 class=\"topic-title\">How it works</h3>\
<div class=\"kitae-card\"><h3>Incremental demo</h3><p>One signal change &rarr; one patch op &rarr; one spliced frame. No full relayout.</p><button class=\"demo-btn\">Count: 0</button></div>\
<p class=\"card-note\">That counter is a real form control laid out by kitae_layout; each click is one SetText patch spliced into the display list — no full relayout.</p>\
<p class=\"card-note\"><b>file:///some/page.html</b> opens local files, and <b>kitae --dump &lt;url&gt;</b> prints a headless text dump in a terminal.</p>\
</div>\
</div>\
</section></div>\
<div class=\"screen\"><div class=\"screen-pad\"></div><section class=\"chapter\">\
<div class=\"badge\">FAQ</div>\
<h2 class=\"ch-title\">Frequently asked questions</h2>\
<div class=\"faq-wrap\">\
<details class=\"faq\" open=\"\">\
<summary>Does any JavaScript run, ever?</summary>\
<p class=\"card-note\">No. Kitae parses, lays out and renders every page itself; there is no script engine to run, so no page can execute code.</p>\
</details>\
<details class=\"faq\">\
<summary>What image formats can it show?</summary>\
<p class=\"card-note\">QTSQ natively, plus PNG, JPEG, GIF, WebP and SVG decoded by the engine&rsquo;s own media stack.</p>\
</details>\
<details class=\"faq\">\
<summary>Can it play video or audio?</summary>\
<p class=\"card-note\">Yes — video and audio elements render and play through the engine&rsquo;s own media layer, with real controls.</p>\
</details>\
<details class=\"faq\">\
<summary>What happens on script-heavy sites?</summary>\
<p class=\"card-note\">You get the server-rendered HTML: text, links and images. Anything that only exists through script never arrives — that is the point.</p>\
</details>\
<details class=\"faq\">\
<summary>Is this page itself running script?</summary>\
<p class=\"card-note\">No — it is kitae_ui components styled with plain CSS and rendered by the Kitae engine, this very binary.</p>\
</details>\
</div>\
<footer class=\"site-footer\">\
<p class=\"colophon-tag\"><i>The web, without the noise.</i></p>\
<p class=\"colophon-fine\">This page is kitae_ui components styled with plain CSS and rendered by the Kitae engine. Every gradient, curve and toggle is a live test — if something renders wrong, that&rsquo;s the next fix.</p>\
</footer>\
</section></div>\
</main>\
</body></html>"
        // The hero background's inline QTSQ data URL is a runtime value
        // (base64 of the embedded file), so the static mirror carries a
        // token resolved here — the same bytes the component tree rides.
        .replace("__KITAE_HERO_BG_QTSQ__", start_page::hero_bg_data_url())
}

pub fn error_page_html(url: &str, message: &str) -> String {
    let url = escape_html(url);
    let msg = escape_html(message);
    format!(
        "<!doctype html><html><head><title>Can't load this page — Kitae</title></head><body>\
<h1>Can't load this page</h1>\
<p><b>{url}</b></p>\
<p>{msg}</p>\
<p>Kitae fetched this address directly over HTTPS. If the site needs JavaScript \
or blocks non-browser agents, there is no JS-free version to show.</p>\
</body></html>"
    )
}

/// S-2 gate (SECURITY_AUDIT.md): may the page at `page_origin` open `abs`
/// (a resolved href) in the OS? `kitae:open-external:` is authored only by
/// [`interstitial_html`], whose link always embeds the page's own URL — so
/// the request is honored when the origin is engine-internal/local or the
/// target self-references the page, and only http(s) targets pass.
/// `mailto:`/`tel:` are ordinary web vocabulary and always allowed.
fn open_external_permitted(page_origin: &str, abs: &str) -> bool {
    if abs.starts_with("mailto:") || abs.starts_with("tel:") {
        return true;
    }
    let Some(target) = abs.strip_prefix("kitae:open-external:") else {
        return false;
    };
    if !(target.starts_with("http://") || target.starts_with("https://")) {
        return false;
    }
    let origin = page_origin.trim();
    origin.is_empty()
        || origin.starts_with("file://")
        || origin.starts_with("about:")
        || origin.starts_with("kitae:")
        || origin.starts_with("data:")
        // The interstitial link embeds the bare page URL (no scheme); the
        // `//` guard keeps "example.com" from matching "notexample.com".
        || target.contains(&format!("//{origin}"))
}

pub fn interstitial_html(url: &str) -> String {
    let url = escape_html(url);
    format!(
        "<!doctype html><html><head><title>JavaScript required — Kitae</title></head><body>\
<h1>This page runs on JavaScript</h1>\
<p>Kitae never executes JavaScript — that is the deal. The page at <b>{url}</b> \
shipped an empty shell instead of content.</p>\
<p><a href=\"kitae:open-external:{url}\">Open this address in your default browser</a></p>\
<p>Many sites keep a plain version — try their text-only or lite mirror.</p>\
</body></html>"
    )
}

/// H8: the page a tab shows after the S-5 guard recovers from a panic (see
/// [`Browser::show_crash_page`]). Plain HTML on purpose — the recovery path
/// must not lean on anything the panic could have left half-mutated. The one
/// link is engine vocabulary (`kitae:reload`), routed in `click_link`.
pub fn crash_page_html(what: &str) -> String {
    let what = escape_html(what);
    format!(
        "<!doctype html><html><head><title>This tab crashed — Kitae</title></head><body>\
<h1>This tab crashed</h1>\
<p>Kitae recovered from an internal error in the {what}. The tab was reset \
and everything else kept running — per-tab isolation means a crash never \
leaves this tab.</p>\
<p><a href=\"kitae:reload\">Reload this tab</a></p>\
</body></html>"
    )
}

// ---------------------------------------------------------------------------
// QTSQ viewer pages (J2)
// ---------------------------------------------------------------------------

/// File name (last path segment, decoded) of a URL — the title for `.qtsq`
/// navigations per the integration plan.
fn url_filename(url: &str) -> String {
    let raw = url.rsplit('/').next().unwrap_or(url);
    let raw = raw.split(['?', '#']).next().unwrap_or(raw);
    percent_encoding::percent_decode_str(raw)
        .decode_utf8_lossy()
        .into_owned()
}

/// Body HTML for a decoded QTSQ text payload: Markdown renders rich, JSON
/// and plain prose keep the (now wrapping) monospace view — the payload
/// carries no extension, so the shape of the text decides.
fn qtsq_text_body(text: &str) -> String {
    if markdown::looks_like_json(text) {
        return format!("<pre>{}</pre>", escape_html(text));
    }
    if markdown::looks_like_markdown(text) {
        return markdown::to_html(text);
    }
    format!("<pre>{}</pre>", escape_html(text))
}

// ── Save-Page-as-QTSQ (slice ②) ──
//
// A saved page is a device-encrypted, device-signed text payload whose text
// is a line-oriented archive envelope followed by the page's own HTML. The
// envelope must survive the encoder's strict sanitizer (it collapses space
// and tab runs but preserves newlines), hence the one-item-per-line shape:
//
//   <!--kitae-archive v1
//   base=<absolute URL of the page>
//   title=<page title, newlines flattened>
//   -->
//   <the document HTML>

/// First line of every Kitae archive payload.
const QTSQ_ARCHIVE_MARKER: &str = "<!--kitae-archive v1";

/// Build the archive payload text for a page. `base` and `title` carry no
/// newlines (titles are flattened by the caller of record,
/// [`Browser::save_page_qtsq`]).
fn qtsq_archive_payload(base: &str, title: &str, html: &str) -> String {
    let title = title.replace(['\r', '\n'], " ");
    format!("{QTSQ_ARCHIVE_MARKER}\nbase={base}\ntitle={title}\n-->\n{html}")
}

/// Split a decoded archive payload into (base, title, document HTML).
/// `None` when the text is not a Kitae archive — plain text and Markdown
/// files keep their usual escaped-text views. `base`/`title` take the first
/// occurrence of each header line; a missing terminator or base leaves no
/// archive to render.
fn qtsq_archive_parts(text: &str) -> Option<(String, String, String)> {
    let mut lines = text.split('\n');
    let first = lines.next()?.trim_end_matches('\r');
    if first != QTSQ_ARCHIVE_MARKER {
        return None;
    }
    let mut base = String::new();
    let mut title = String::new();
    let mut terminator = None;
    for (i, line) in lines.enumerate() {
        let line = line.trim_end_matches('\r');
        if line == "-->" {
            terminator = Some(i);
            break;
        }
        if let Some(rest) = line.strip_prefix("base=") {
            if base.is_empty() {
                base = rest.to_string();
            }
        } else if let Some(rest) = line.strip_prefix("title=") {
            if title.is_empty() {
                title = rest.to_string();
            }
        }
    }
    let terminator = terminator?;
    if base.is_empty() {
        return None;
    }
    // The document is everything after the terminator line (first line of
    // the payload was consumed before the loop, hence the +2).
    let html = text
        .split('\n')
        .skip(terminator + 2)
        .collect::<Vec<_>>()
        .join("\n");
    Some((base, title, html))
}

/// Decode a QTSQ image payload for the viewer page. Fails closed on any
/// decode error — the caller falls back to the info page. Device-encrypted
/// files decode here automatically (the kitae_qtsq layer holds the key
/// logic); truly unopenable ones error out. (The dispatch in
/// [`qtsq_viewer_page`] inlines this; kept for tests.)
#[cfg(test)]
fn qtsq_decode_to_image(bytes: &[u8]) -> Option<kitae_qtsq::RgbaImage> {
    let info = kitae_qtsq::probe(bytes)?;
    if !info.data_type.is_image() {
        return None;
    }
    kitae_qtsq::decode_image(bytes, None).ok()
}

/// Metadata lines shared by the info page and media viewer pages.
fn qtsq_meta_lines(info: &kitae_qtsq::QtsqInfo) -> Vec<String> {
    let mut lines = vec![
        format!("Strategy: {}", info.strategy.name()),
        format!("Data type: {}", info.data_type.name()),
        format!("Original size: {} bytes", info.original_size),
        format!("Encryption: {}", if info.encrypted { "yes" } else { "no" }),
    ];
    if let (Some(w), Some(h)) = (info.width, info.height) {
        lines.push(format!("Dimensions: {w} × {h}"));
    }
    if let Some(sr) = info.sample_rate {
        lines.push(format!("Sample rate: {sr} Hz"));
    }
    if let Some(fr) = info.frame_rate {
        lines.push(format!("Frame rate: {fr} fps"));
    }
    if info.lossy {
        lines.push("Compression: lossy".to_string());
    }
    lines
}

/// Dispatch a fetched QTSQ payload to its viewer page. Images decode to
/// pixels here (J2 ships real pixels via the `<img>` page + raster cache —
/// see [`qtsq_raster_for_url`]); text decodes to a rich document (Markdown
/// rendered to headings/lists/tables — see `markdown`); audio/video
/// get media viewer placeholders (J4/J5 wire playback); tensors decode to a
/// heat-map strip + values table; everything else — container, brain,
/// unknown, or undecodable/encrypted — gets a metadata info page. Plain HTML
/// either way, so it styles like every page.
///
/// `password` is the session QTSQ password, applied to every decode attempt.
/// The bool result is the page's `qtsq_locked` flag: true exactly when the
/// file is encrypted and we could not render its payload (no password, wrong
/// password, or keys we don't hold) — frames publish it so hosts can prompt.
/// The [`QtsqSigState`] result is the payload's signature verdict (slice ④);
/// signed pages also lead their body with the verdict banner. The
/// `Option<String>` result is the archived page's base URL (slice ②): the
/// caller keeps the .qtsq file URL as the session URL (address bar, reload)
/// while the archived base drives link and subresource resolution.
fn qtsq_viewer_page(
    url: &str,
    base: &str,
    body: &[u8],
    password: Option<&str>,
) -> (String, Option<String>, bool, QtsqSigState) {
    let sig = qtsq_sig_verdict(body);
    let title = url_filename(url);
    let esc_title = escape_html(&title);
    // Shared failure page: the note distinguishes "no password given" from
    // "password not accepted", and the flag marks encrypted files locked so
    // the host offers the password prompt again.
    let info_page = |info: &kitae_qtsq::QtsqInfo| -> (String, String, bool, Option<String>) {
        let (inner, doc) = qtsq_info_page_body(&esc_title, info, password.is_some());
        (inner, doc, info.encrypted, None)
    };
    let (inner, doc_title, locked, base_override) = match kitae_qtsq::probe(body) {
        Some(info) if info.data_type.is_image() => match kitae_qtsq::decode_image(body, password) {
            Ok(img) => {
                let raster = qtsq_register_raster(base, img.width, img.height, img.pixels.clone());
                let src = escape_html(base);
                let dim = qtsq_fit_dims(raster.width, raster.height, 1024);
                (
                    format!(
                        "<p><img src=\"{src}\" alt=\"{esc_title}\" \
width=\"{}\" height=\"{}\"></p>",
                        dim.0, dim.1
                    ),
                    esc_title,
                    false,
                    None,
                )
            }
            Err(_) => info_page(&info),
        },
        Some(info) if info.data_type.is_text() => {
            match kitae_qtsq::decode_text(body, password) {
                Ok(text) => match qtsq_archive_parts(&text) {
                    // A saved page reopens as the original document: the
                    // stored HTML is appended inside the viewer body (the
                    // parser ignores its nested html/head/body tags, so its
                    // <title> and <style> come back), a provenance note leads
                    // it, and relative links resolve against the archived
                    // base via the returned override.
                    Some((arch_base, arch_title, doc)) => {
                        let title = if arch_title.is_empty() {
                            esc_title
                        } else {
                            escape_html(&arch_title)
                        };
                        (
                            format!(
                                "<p class=\"qtsq-archive-base\">Saved from {}</p>{doc}",
                                escape_html(&arch_base)
                            ),
                            title,
                            false,
                            Some(arch_base),
                        )
                    }
                    None => (
                        format!("<div class=\"qtsq-text\">{}</div>", qtsq_text_body(&text)),
                        esc_title,
                        false,
                        None,
                    ),
                },
                Err(_) => info_page(&info),
            }
        }
        Some(info) if info.data_type.is_audio() => {
            let rate = info.sample_rate.unwrap_or(0);
            let channels = match info.data_type {
                kitae_qtsq::QtsqDataType::AudioMono => 1,
                kitae_qtsq::QtsqDataType::AudioStereo => 2,
                _ => info.channels.max(1),
            };
            // J4: decode on the engine thread, write the temp CAF the
            // embedder's player will open, register MediaInfo for the
            // Browser to pick up after fetch. A file whose keys we don't
            // hold fails the decode here and gets the info page.
            match qtsq_prepare_audio(base, body, &info, password) {
                Some(media) => {
                    qtsq_register_media(media);
                    let meta = qtsq_meta_lines(&info);
                    (
                        format!(
                            "<h1>{esc_title}</h1>\
<p>QTSQ audio — playback is handled by the system player.</p>\
<p class=\"qtsq-media\" data-kind=\"audio\" data-rate=\"{rate}\" \
data-channels=\"{channels}\">{}</p>",
                            meta.iter()
                                .map(|l| escape_html(l))
                                .collect::<Vec<_>>()
                                .join("<br>")
                        ),
                        esc_title,
                        false,
                        None,
                    )
                }
                None => info_page(&info),
            }
        }
        Some(info) if info.data_type.is_video() => {
            // Files whose keys we don't hold fail the decoder open here and
            // get the info page; decodable ones get the media viewer.
            match kitae_qtsq::VideoDecoder::open(body, password) {
                Ok(vd) => {
                    let mut meta = qtsq_meta_lines(&info);
                    let dims = vd.dimensions();
                    let duration = vd.duration().unwrap_or(0.0);
                    if info.frame_rate.is_none() {
                        if let Some(fps) = vd.fps() {
                            meta.push(format!("Frame rate: {fps} fps"));
                        }
                    }
                    qtsq_register_video(base, body, vd);
                    qtsq_register_media(MediaInfo {
                        url: base.to_string(),
                        kind: MediaKind::Video,
                        duration,
                        rate: 0,
                        channels: 0,
                        playing: false,
                        position: 0.0,
                        caf_path: String::new(),
                    });
                    // The <video> element lays out through the J5 replaced-element
                    // path; the engine paints the stepped frame as its child image.
                    let src = escape_html(base);
                    let dim_attr = if dims.0 > 0 {
                        format!(" width=\"{}\" height=\"{}\"", dims.0, dims.1)
                    } else {
                        String::new()
                    };
                    (
                        format!(
                            "<h1>{esc_title}</h1>\
<p><video src=\"{src}\"{dim_attr}>QTSQ video — frames decode in the engine \
and paint as images; the shell hosts the transport.</video></p>\
<p class=\"qtsq-media\" data-kind=\"video\">{}</p>",
                            meta.iter()
                                .map(|l| escape_html(l))
                                .collect::<Vec<_>>()
                                .join("<br>")
                        ),
                        esc_title,
                        false,
                        None,
                    )
                }
                Err(_) => info_page(&info),
            }
        }
        Some(info) if info.data_type.is_tensor() => {
            // Tensors (weights/embeddings) decode to shape + f32 values; the
            // viewer draws a heat-map strip (one pixel per value, scaled) plus
            // a values table. Files whose keys we don't hold fail the decode
            // here and get the info page.
            match kitae_qtsq::decode_tensor(body, password) {
                Ok(tensor) => {
                    let (hw, hh) = tensor_heatmap_dims(&tensor);
                    let heat_url = format!("{base}#heat");
                    let raster =
                        qtsq_register_raster(&heat_url, hw, hh, tensor_heatmap_pixels(&tensor));
                    let src = escape_html(&heat_url);
                    let dim = qtsq_fit_dims(raster.width, raster.height, 1024);
                    let body_html = qtsq_tensor_page_body(&esc_title, &info, &tensor, &src, dim);
                    (body_html, esc_title, false, None)
                }
                Err(_) => info_page(&info),
            }
        }
        Some(info) => info_page(&info),
        None => (
            format!(
                "<h1>{esc_title}</h1><p>QTSQ file — header unreadable; \
the payload may be corrupt or use a newer format version.</p>"
            ),
            esc_title,
            false,
            None,
        ),
    };
    (
        format!(
            "<!doctype html><html><head><title>{doc_title}</title></head><body>{}{inner}</body></html>",
            qtsq_sig_banner(sig)
        ),
        base_override,
        locked,
        sig,
    )
}

/// Signature verdict for a fetched QTSQ payload (slice ④): the header's
/// signed flag gates everything; a signed payload is checked against this
/// machine's device sign key. No key → `Unverified`; key + bad signature →
/// `Invalid`; everything else is `Unsigned`. Runs once per .qtsq
/// navigation, before the payload dispatch, on the engine thread.
fn qtsq_sig_verdict(body: &[u8]) -> QtsqSigState {
    let Some(info) = kitae_qtsq::probe(body) else {
        return QtsqSigState::Unsigned;
    };
    if !info.signed {
        return QtsqSigState::Unsigned;
    }
    match kitae_qtsq::device_sign_pk() {
        None => QtsqSigState::Unverified,
        Some(pk) => match kitae_qtsq::verify_signature(body, &pk) {
            Ok(kitae_qtsq::SignatureCheck::Valid) => QtsqSigState::Valid,
            Ok(kitae_qtsq::SignatureCheck::Invalid) => QtsqSigState::Invalid,
            Ok(kitae_qtsq::SignatureCheck::Unsigned) => QtsqSigState::Unsigned,
            Err(_) => QtsqSigState::Unverified,
        },
    }
}

/// The verdict strip at the top of a signed viewer page. Unsigned pages get
/// nothing; the three verdicts get tinted, classed paragraphs styled by the
/// UA sheet (`kitae_style::UA_STYLESHEET`).
fn qtsq_sig_banner(sig: QtsqSigState) -> String {
    let (class, text) = match sig {
        QtsqSigState::Unsigned => return String::new(),
        QtsqSigState::Valid => (
            "qtsq-sig-valid",
            "Signature verified — signed by this device.",
        ),
        QtsqSigState::Invalid => (
            "qtsq-sig-invalid",
            "Signature check failed — this file was modified after signing \
or was signed by another device.",
        ),
        QtsqSigState::Unverified => (
            "qtsq-sig-unverified",
            "Signed file — this device has no key to verify the signature.",
        ),
    };
    format!("<p class=\"{class}\">{text}</p>")
}

/// Cap an image's laid-out dimensions to `max_edge` on the long side
/// (viewer pages show photos up to 5760 px wide; the 8×8 test fixtures stay
/// untouched). Aspect ratio preserved, minimum 1.
fn qtsq_fit_dims(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= max_edge || long == 0 {
        return (w, h);
    }
    let scale = max_edge as f64 / long as f64;
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    )
}

/// The tensor/unknown/encrypted fallback: strategy, data type, dimensions,
/// original size, encryption flag — plain HTML like every other page.
/// `password_tried` selects the note: a rejected password, a file we hold no
/// password/keys for, or an unrenderable plaintext payload.
fn qtsq_info_page_body(
    title: &str,
    info: &kitae_qtsq::QtsqInfo,
    password_tried: bool,
) -> (String, String) {
    let meta = qtsq_meta_lines(info)
        .iter()
        .map(|l| escape_html(l))
        .collect::<Vec<_>>()
        .join("<br>");
    let note = if info.encrypted && password_tried {
        "<p>That password did not open this file. Check it and try again, or \
the file may be locked to another device.</p>"
    } else if info.encrypted {
        "<p>This file is encrypted and Kitae could not open it — it may be \
locked to another device or need a password.</p>"
    } else {
        "<p>Kitae cannot display this payload kind yet — the metadata above \
is read straight from the file header.</p>"
    };
    (
        format!("<h1>{title}</h1><p>QTSQ native-format file.</p>{note}<p>{meta}</p>"),
        title.to_string(),
    )
}

/// Heat-map strip geometry for a decoded tensor: one cell per value laid out
/// in rows of [`TENSOR_HEAT_COLS`] cells (downsampled by stride past the cap),
/// each cell [`TENSOR_HEAT_CELL`] px.
const TENSOR_HEAT_COLS: usize = 16;
const TENSOR_HEAT_CELL: u32 = 8;
const TENSOR_HEAT_MAX_VALUES: usize = 4096;

fn tensor_heatmap_dims(tensor: &kitae_qtsq::QtsqTensor) -> (u32, u32) {
    let shown = tensor.values.len().min(TENSOR_HEAT_MAX_VALUES);
    let cols = shown.min(TENSOR_HEAT_COLS).max(1);
    let rows = shown.div_ceil(cols).max(1);
    (
        cols as u32 * TENSOR_HEAT_CELL,
        rows as u32 * TENSOR_HEAT_CELL,
    )
}

/// Color a normalized value on a viridis-like three-stop ramp
/// (indigo → teal → yellow). Constant across min==max tensors.
fn tensor_heat_color(t: f32) -> [u8; 3] {
    let stops: [(f32, f32, f32); 3] = [
        (68.0, 1.0, 84.0),
        (33.0, 145.0, 140.0),
        (253.0, 231.0, 37.0),
    ];
    let t = t.clamp(0.0, 1.0);
    let seg = t * 2.0;
    let (a, b, k) = if seg < 1.0 {
        (stops[0], stops[1], seg)
    } else {
        (stops[1], stops[2], seg - 1.0)
    };
    let ch = |x: f32, y: f32| (x + (y - x) * k).round().clamp(0.0, 255.0) as u8;
    [ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2)]
}

/// Heat-map pixels for a decoded tensor: values min/max-normalized and
/// mapped through [`tensor_heat_color`], one [`TENSOR_HEAT_CELL`]px cell per
/// value (row-major, downsampled by stride past the cap). Non-finite values
/// paint neutral gray; unused padding cells (rows round up) stay dark.
fn tensor_heatmap_pixels(tensor: &kitae_qtsq::QtsqTensor) -> Vec<u8> {
    let (w, h) = tensor_heatmap_dims(tensor);
    let cols = w as usize / TENSOR_HEAT_CELL as usize;
    let cells = cols * (h as usize / TENSOR_HEAT_CELL as usize);
    let n = tensor.values.len();
    let shown = n.min(TENSOR_HEAT_MAX_VALUES);
    let mut finite_min = f32::INFINITY;
    let mut finite_max = f32::NEG_INFINITY;
    for v in &tensor.values {
        if v.is_finite() {
            finite_min = finite_min.min(*v);
            finite_max = finite_max.max(*v);
        }
    }
    let usable = finite_min.is_finite() && finite_max.is_finite() && finite_max > finite_min;
    let cell_px = TENSOR_HEAT_CELL as usize;
    let mut pixels = vec![16u8; cells * cell_px * cell_px * 4];
    for cell in 0..cells {
        let rgba = if cell < shown {
            let v = tensor.values[cell * n / shown];
            if v.is_finite() && usable {
                let t = (v - finite_min) / (finite_max - finite_min);
                let [r, g, b] = tensor_heat_color(t);
                [r, g, b, 255]
            } else {
                [120, 120, 126, 255]
            }
        } else {
            [16, 16, 20, 255]
        };
        let cx = cell % cols;
        let cy = cell / cols;
        for py in 0..cell_px {
            for px in 0..cell_px {
                let off = ((cy * cell_px + py) * w as usize + cx * cell_px + px) * 4;
                pixels[off..off + 4].copy_from_slice(&rgba);
            }
        }
    }
    pixels
}

/// The tensor viewer page: shape/count summary, the heat-map strip image,
/// header metadata, and a values table (first [`TENSOR_TABLE_MAX`] values).
/// `src`/`dim` are the registered strip raster's URL and fitted dimensions.
const TENSOR_TABLE_MAX: usize = 24;

fn qtsq_tensor_page_body(
    title: &str,
    info: &kitae_qtsq::QtsqInfo,
    tensor: &kitae_qtsq::QtsqTensor,
    src: &str,
    dim: (u32, u32),
) -> String {
    let count = tensor.values.len();
    let shape = if tensor.dims.is_empty() {
        format!("{count} (flat)")
    } else {
        tensor
            .dims
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(" × ")
    };
    let finite: Vec<f32> = tensor
        .values
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .collect();
    let min = finite.iter().copied().fold(f32::INFINITY, f32::min);
    let max = finite.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mean = if finite.is_empty() {
        0.0
    } else {
        finite.iter().sum::<f32>() / finite.len() as f32
    };
    let meta = qtsq_meta_lines(info)
        .iter()
        .map(|l| escape_html(l))
        .collect::<Vec<_>>()
        .join("<br>");
    let shown = count.min(TENSOR_TABLE_MAX);
    let mut table = String::from("<table><tr><th>#</th><th>Value</th></tr>");
    for (i, v) in tensor.values.iter().take(shown).enumerate() {
        table.push_str(&format!(
            "<tr><td>{i}</td><td>{}</td></tr>",
            if v.is_finite() {
                format!("{v:.6}")
            } else {
                "—".to_string()
            }
        ));
    }
    table.push_str("</table>");
    let more = if count > shown {
        format!("<p>… and {} more values.</p>", count - shown)
    } else {
        String::new()
    };
    format!(
        "<h1>{title}</h1>\
<p>QTSQ neural tensor — {count} values, shape {shape}. \
Range [{min:.6}, {max:.6}], mean {mean:.6}.</p>\
<p><img src=\"{src}\" alt=\"{title} heat map\" width=\"{}\" height=\"{}\"></p>\
<p>{meta}</p>\
<h2>Values</h2>\
{table}{more}",
        dim.0, dim.1
    )
}

// ---------------------------------------------------------------------------
// QTSQ raster cache (J2/J3): decoded pixels keyed by the document URL that
// produced them. `qtsq_viewer_page` fills it during fetch;
// `Browser::relayout` resolves in-page `<img src>` entries against it and
// stamps `DisplayImage.payload` ids; `qtsq_raster_for_id` is the
// shell/renderer side-table lookup that turns a payload id into pixels.
// ---------------------------------------------------------------------------

use std::sync::{Arc, Mutex, OnceLock};

/// Decoded raster: stable payload id, dimensions, tightly packed RGBA8.
/// Shared by QTSQ (J3) and web stills (K3) — one id space, one pull API
/// (`qtsq_raster_for_id`); the frame carries ids, this store carries bytes.
#[derive(Debug, Clone)]
pub struct QtsqRaster {
    /// Id stamped into `DisplayImage.payload` for this raster. Stable across
    /// re-registration (re-decode keeps the id, replaces the pixels).
    pub id: u64,
    pub width: u32,
    pub height: u32,
    /// width * height * 4 bytes, row-major, straight alpha.
    pub pixels: Arc<Vec<u8>>,
}

/// Bound for the shared decoded-raster cache (Phase K3: LRU, ~64 entries).
pub const RASTER_CACHE_MAX: usize = 64;

/// Byte budget for decoded pixels (I3 purgeable-cache rule). Decoded RGBA
/// bytes dwarf encoded files — one 12-megapixel photo is ~46 MB — so the
/// raster store evicts by *bytes* as well as by entry count. Embedders can
/// retune it with [`set_raster_cache_byte_budget`].
pub const RASTER_CACHE_MAX_BYTES: usize = 192 * 1024 * 1024;

static RASTER_CACHE_BYTES: AtomicUsize = AtomicUsize::new(RASTER_CACHE_MAX_BYTES);

/// Adjust the decoded-pixel byte budget (memory-pressure plumbing for
/// embedders). Takes effect on the next raster registration.
pub fn set_raster_cache_byte_budget(bytes: usize) {
    RASTER_CACHE_BYTES.store(bytes, Ordering::Relaxed);
}

struct RasterStore {
    by_url: HashMap<String, QtsqRaster>,
    /// LRU order, oldest first.
    order: Vec<String>,
    next_id: u64,
    /// Sum of every cached raster's `pixels.len()`.
    bytes: usize,
}

fn qtsq_raster_store() -> &'static Mutex<RasterStore> {
    static STORE: OnceLock<Mutex<RasterStore>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(RasterStore {
            by_url: HashMap::new(),
            order: Vec::new(),
            next_id: 1,
            bytes: 0,
        })
    })
}

/// Store decoded pixels for a document URL, returning the raster (with
/// its payload id). Re-registering an existing URL keeps its id, replaces
/// the bytes and bumps recency. Evicts LRU past [`RASTER_CACHE_MAX`] and,
/// per the I3 purgeable-cache rule, past the byte budget in
/// [`RASTER_CACHE_MAX_BYTES`] — the newest raster is never evicted while it
/// is the only one, so a lone oversized image stays live.
fn qtsq_register_raster(url: &str, w: u32, h: u32, pixels: Vec<u8>) -> QtsqRaster {
    let mut store = qtsq_raster_store().lock().unwrap();
    let id = store.by_url.get(url).map(|r| r.id).unwrap_or_else(|| {
        let id = store.next_id;
        store.next_id += 1;
        id
    });
    if let Some(old) = store.by_url.get(url) {
        store.bytes = store.bytes.saturating_sub(old.pixels.len());
    }
    let raster = QtsqRaster {
        id,
        width: w,
        height: h,
        pixels: Arc::new(pixels),
    };
    store.bytes += raster.pixels.len();
    store.by_url.insert(url.to_string(), raster.clone());
    if let Some(i) = store.order.iter().position(|k| k == url) {
        store.order.remove(i);
    }
    store.order.push(url.to_string());
    let budget = RASTER_CACHE_BYTES.load(Ordering::Relaxed);
    while store.by_url.len() > RASTER_CACHE_MAX || (store.bytes > budget && store.by_url.len() > 1)
    {
        if let Some(oldest) = store.order.first().cloned() {
            if let Some(r) = store.by_url.remove(&oldest) {
                store.bytes = store.bytes.saturating_sub(r.pixels.len());
            }
            store.order.remove(0);
        } else {
            break;
        }
    }
    raster
}

/// Look up decoded pixels for an image URL (shell/renderer side of the
/// `<img>` raster contract). The cache is keyed by final URL, so
/// subresource fetches and direct navigations share entries. Bumps recency.
pub fn qtsq_raster_for_url(url: &str) -> Option<QtsqRaster> {
    let mut store = qtsq_raster_store().lock().unwrap();
    let hit = store.by_url.get(url).cloned()?;
    if let Some(i) = store.order.iter().position(|k| k == url) {
        store.order.remove(i);
        store.order.push(url.to_string());
    }
    Some(hit)
}

/// Resolve a `DisplayImage.payload` id to its decoded pixels (the frame's
/// pixel side-table on the host side).
pub fn qtsq_raster_for_id(id: u64) -> Option<QtsqRaster> {
    qtsq_raster_store()
        .lock()
        .unwrap()
        .by_url
        .values()
        .find(|r| r.id == id)
        .cloned()
}

/// Drop every cached raster (history-clear plumbing for hosts).
pub fn clear_qtsq_rasters() {
    let mut store = qtsq_raster_store().lock().unwrap();
    store.by_url.clear();
    store.order.clear();
    store.bytes = 0;
    anim_store().lock().unwrap().by_id.clear();
    anim_store().lock().unwrap().order.clear();
}

/// Live size of the decoded-raster cache: `(entries, decoded bytes)`.
/// Observability for the I3 purgeable-cache budget.
pub fn raster_cache_stats() -> (usize, usize) {
    let store = qtsq_raster_store().lock().unwrap();
    (store.by_url.len(), store.bytes)
}

pub fn clear_qtsq_raster_for(url: &str) {
    let removed_id = {
        let mut store = qtsq_raster_store().lock().unwrap();
        let removed = store.by_url.remove(url);
        if let Some(raster) = removed.as_ref() {
            store.bytes = store.bytes.saturating_sub(raster.pixels.len());
        }
        store.order.retain(|key| key != url);
        removed.map(|raster| raster.id)
    };
    if let Some(id) = removed_id {
        let mut anim = anim_store().lock().unwrap();
        anim.by_id.remove(&id);
        anim.order.retain(|key| *key != id);
    }
}

/// Purge every decoded image cache (I3 purgeable caches): decoded rasters,
/// animation frames and the network byte cache. Everything re-decodes (and
/// re-fetches, within HTTP cache rules) on demand, so this is safe to call
/// under memory pressure — it trades immediate footprint for re-decode work.
pub fn purge_image_caches() {
    clear_qtsq_rasters();
    clear_qtsq_videos();
    clear_qtsq_media();
    kitae_net::clear_cache();
}

// Animations (Q8): multi-frame rasters (animated GIFs) keyed by the same
// payload id as their frame-0 still above. The engine registers frames at
// decode time but never plays them (N8: no engine timeline) — hosts pull
// `qtsq_animation_for_id` / the C ABIs in ffi.rs and step frames on their
// own clock.

/// Bound for the animation side-table (animations are the heaviest rasters:
/// full-canvas RGBA × frame count).
pub const ANIM_CACHE_MAX: usize = 8;

/// One composited animation frame (Q8): full-canvas RGBA8 + delay.
#[derive(Debug, Clone)]
pub struct AnimFrame {
    /// `width * height * 4` bytes, straight alpha, ready to blit.
    pub pixels: Arc<Vec<u8>>,
    /// Presentation delay in milliseconds.
    pub delay_ms: u32,
}

/// The full frame sequence behind an animated `DisplayImage`. `loop_count`
/// is the file's repeat declaration: `None` = play once, `Some(n)` = n extra
/// repetitions, `Some(u16::MAX)` = forever.
#[derive(Debug, Clone)]
pub struct AnimationFrames {
    /// Same payload id as the frame-0 still in the raster store.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub loop_count: Option<u16>,
    /// Composited frames in presentation order.
    pub frames: Vec<AnimFrame>,
}

struct AnimStore {
    by_id: HashMap<u64, Arc<AnimationFrames>>,
    /// LRU order, oldest first (ids mirror the raster store's recency).
    order: Vec<u64>,
}

fn anim_store() -> &'static Mutex<AnimStore> {
    static STORE: OnceLock<Mutex<AnimStore>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(AnimStore {
            by_id: HashMap::new(),
            order: Vec::new(),
        })
    })
}

/// Register the frame sequence for a raster id (called from
/// `ensure_web_raster` when a decode yields more than one frame).
fn register_animation(
    id: u64,
    width: u32,
    height: u32,
    loop_count: Option<u16>,
    frames: Vec<AnimFrame>,
) {
    let mut store = anim_store().lock().unwrap();
    let anim = AnimationFrames {
        id,
        width,
        height,
        loop_count,
        frames,
    };
    store.by_id.insert(id, Arc::new(anim));
    if let Some(i) = store.order.iter().position(|k| *k == id) {
        store.order.remove(i);
    }
    store.order.push(id);
    while store.by_id.len() > ANIM_CACHE_MAX {
        if let Some(oldest) = store.order.first().copied() {
            store.by_id.remove(&oldest);
            store.order.remove(0);
        } else {
            break;
        }
    }
}

/// Pull an animation's frame sequence by payload id (host/renderer side).
/// Bumps recency in both stores: an animated image in use should not lose
/// its frames or its frame-0 still.
pub fn qtsq_animation_for_id(id: u64) -> Option<Arc<AnimationFrames>> {
    let hit = anim_store().lock().unwrap().by_id.get(&id).cloned()?;
    {
        let mut anim = anim_store().lock().unwrap();
        if let Some(i) = anim.order.iter().position(|k| *k == id) {
            anim.order.remove(i);
            anim.order.push(id);
        }
    }
    {
        let mut store = qtsq_raster_store().lock().unwrap();
        let url = store
            .by_url
            .iter()
            .find(|(_, r)| r.id == id)
            .map(|(u, _)| u.clone());
        if let Some(url) = url {
            if let Some(i) = store.order.iter().position(|k| *k == url) {
                store.order.remove(i);
                store.order.push(url);
            }
        }
    }
    Some(hit)
}

/// Serializes tests that touch the process-global raster store: parallel
/// test threads would otherwise clear each other's entries mid-test.
#[cfg(test)]
pub(crate) fn qtsq_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Fetch + decode a QTSQ image subresource on first sight, caching by URL.
/// Only image-strategy QTSQ payloads produce a raster; web images and
/// undecodable QTSQ (no device key for an encrypted file, wrong data type)
/// return `None` and the renderer keeps the placeholder box. File
/// subresources are gated on the `.qtsq` extension (web-format subresources
/// are Phase K, below); inline QTSQ rides a `data:image/qtsq;base64,` URL —
/// the start page's hero background ships this way — and decodes straight
/// from the payload with no fetch.
fn ensure_qtsq_raster(url: &str) -> Option<QtsqRaster> {
    if let Some(hit) = qtsq_raster_for_url(url) {
        return Some(hit);
    }
    let body: Vec<u8> = if let Some(payload) = url.strip_prefix("data:image/qtsq;base64,") {
        // Inline payload (R6 pattern, like the inline SVG data URLs): the
        // bytes ride the URL itself, no fetch.
        b64_decode(payload)?
    } else {
        if !url
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("qtsq"))
        {
            return None;
        }
        fetch_cached(url).ok()?.body
    };
    let info = kitae_qtsq::probe(&body)?;
    if !info.data_type.is_image() {
        return None;
    }
    // Encrypted files whose device key matches still decode below; the
    // decode failure is the single fail-closed point.
    let img = kitae_qtsq::decode_image(&body, None).ok()?;
    Some(qtsq_register_raster(url, img.width, img.height, img.pixels))
}

/// Natural sizes of every `background-image` the cascade referenced, keyed by
/// the same `src` text a `DisplayImage` carries. Layout never decodes, so
/// `background-size: auto`, `cover` and `contain` only resolve when the
/// embedder hands the sizes in. Already-registered rasters answer from the
/// store; a first-sight URL is fetched and decoded here (the publish pass
/// stamps the same raster's payload id onto the tile). Returns an empty map
/// when the page has no background images, which is the common case.
fn background_image_sizes(
    page: &Page,
    styles: &[(NodeId, ComputedStyle)],
) -> HashMap<String, (f32, f32)> {
    let mut out = HashMap::new();
    for (_, style) in styles {
        let BackgroundPaint::Image(src) = &style.background_paint else {
            continue;
        };
        if out.contains_key(src.as_str()) {
            continue;
        }
        let abs = resolve(&page.resource_base_url, src).unwrap_or_else(|| src.clone());
        if !kitae_net::mixed_content_allowed(&page.base_url, &abs) {
            continue;
        }
        if let Some(raster) = qtsq_raster_for_url(&abs).or_else(|| ensure_web_raster(&abs)) {
            out.insert(src.clone(), (raster.width as f32, raster.height as f32));
        }
    }
    out
}

/// Fetch + decode a web still (PNG/JPEG via `kitae_media`, Phase K3) on first
/// sight, caching by final URL in the shared raster store. Animated GIFs
/// (Q8) additionally register their full frame sequence under the same
/// payload id; the still is frame 0 and the display image is stamped
/// `animated` by the relayout caller. Anything else undecodable (errors,
/// CMYK) returns `None` and the renderer keeps the placeholder box the
/// layout already painted — SVG resolves upstream through
/// [`ensure_svg_raster`] (Q10) and never reaches this decode.
fn ensure_web_raster(url: &str) -> Option<QtsqRaster> {
    if let Some(hit) = qtsq_raster_for_url(url) {
        return Some(hit);
    }
    // Inline `data:` payloads decode from the URL itself, no fetch. The
    // canvas tier leans on this: `toDataURL()` output (a PNG data URL)
    // goes straight back into an `<img src>` or a `drawImage`. QTSQ and
    // SVG payloads were claimed earlier in the chain.
    if url.starts_with("data:") {
        let (mime, bytes) = data_url_bytes(url)?;
        let img = decode_image(&bytes, Some(&mime)).ok()?;
        return Some(qtsq_register_raster(url, img.width, img.height, img.pixels));
    }
    let resp = fetch_cached(url).ok()?;
    if resp.status >= 400 {
        return None;
    }
    // Animated GIF: register the frame sequence alongside the still. A
    // single-frame GIF takes the ordinary still path.
    let is_gif = resp.content_type.eq_ignore_ascii_case("image/gif")
        || resp.body.starts_with(b"GIF87a")
        || resp.body.starts_with(b"GIF89a");
    if is_gif {
        if let Ok((frames, loop_count)) = kitae_media::decode_frames(&resp.body) {
            if frames.len() > 1 {
                let raster = decode_image(&resp.body, Some("image/gif")).ok()?;
                let r = qtsq_register_raster(url, raster.width, raster.height, raster.pixels);
                let anim_frames: Vec<AnimFrame> = frames
                    .into_iter()
                    .map(|f| AnimFrame {
                        pixels: Arc::new(f.rgba),
                        delay_ms: f.delay_ms,
                    })
                    .collect();
                register_animation(r.id, raster.width, raster.height, loop_count, anim_frames);
                return Some(r);
            }
        }
    }
    // Animated APNG: same contract as GIF — a still raster is registered for
    // the image path and the frame sequence drives the animator.
    let is_apng = resp.content_type.eq_ignore_ascii_case("image/apng")
        || (resp.body.starts_with(b"\x89PNG\r\n\x1a\n")
            && kitae_media::is_animated_png(&resp.body));
    if is_apng {
        if let Ok((frames, loop_count)) = kitae_media::decode_apng_frames(&resp.body) {
            if frames.len() > 1 {
                let raster = decode_image(&resp.body, Some("image/png")).ok()?;
                let r = qtsq_register_raster(url, raster.width, raster.height, raster.pixels);
                let anim_frames: Vec<AnimFrame> = frames
                    .into_iter()
                    .map(|f| AnimFrame {
                        pixels: Arc::new(f.rgba),
                        delay_ms: f.delay_ms,
                    })
                    .collect();
                register_animation(r.id, raster.width, raster.height, loop_count, anim_frames);
                return Some(r);
            }
        }
    }
    let img = decode_image(&resp.body, Some(&resp.content_type)).ok()?;
    Some(qtsq_register_raster(url, img.width, img.height, img.pixels))
}

/// Fetch + rasterize an SVG image subresource (Q10) at the layout box size.
/// SVG has no intrinsic bitmap, so the raster is synthesized per box and
/// cached under a `url#WxH` key: the same document laid out at two sizes
/// gets two rasters, and a relayout that changes the box decodes a fresh
/// one (old sizes age out through the store's LRU). Non-SVG payloads
/// return `None` so the relayout chain falls through to
/// [`ensure_web_raster`]; a box of zero (image geometry not yet known)
/// clamps to 1px rather than failing the decode.
fn ensure_svg_raster(url: &str, box_w: f32, box_h: f32) -> Option<QtsqRaster> {
    let w = box_w.ceil().max(1.0) as u32;
    let h = box_h.ceil().max(1.0) as u32;
    let key = format!("{url}#{w}x{h}");
    if let Some(hit) = qtsq_raster_for_url(&key) {
        return Some(hit);
    }
    // R6: inline `<svg>` elements arrive as base64 data URLs (layout
    // serializes the subtree) — no fetch, decode straight from the payload.
    let body: Vec<u8> = if let Some(payload) = url.strip_prefix("data:image/svg+xml;base64,") {
        b64_decode(payload)?
    } else {
        let resp = fetch_cached(url).ok()?;
        if resp.status >= 400 {
            return None;
        }
        let is_svg = resp.content_type.eq_ignore_ascii_case("image/svg+xml")
            || sniff(&resp.body) == Some("image/svg+xml");
        if !is_svg {
            return None;
        }
        resp.body
    };
    let img = decode_svg(&body, w, h).ok()?;
    Some(qtsq_register_raster(
        &key, img.width, img.height, img.pixels,
    ))
}

/// Minimal standard base64 decoder (RFC 4648, padded) for data URLs (R6).
/// `None` on any non-alphabet character or bad padding.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if s.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    for chunk in s.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        if pad > 2 || chunk[..4 - pad].iter().any(|&c| val(c).is_none()) {
            return None;
        }
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            let v = if c == b'=' { 0 } else { val(c)? };
            n |= v << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// The MIME type and bytes of a `data:` URL payload. Base64 payloads
/// decode directly; anything else is percent-decoded (the form inline SVG
/// and other text payloads use).
fn data_url_bytes(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta.split(';').next().unwrap_or("").to_string();
    let bytes: Vec<u8> = if meta.ends_with(";base64") {
        b64_decode(payload)?
    } else {
        let raw = payload.as_bytes();
        let mut out = Vec::with_capacity(raw.len());
        let mut i = 0;
        while i < raw.len() {
            if raw[i] == b'%' && i + 2 < raw.len() {
                let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
                if let (Some(hi), Some(lo)) = (hex(raw[i + 1]), hex(raw[i + 2])) {
                    out.push(hi * 16 + lo);
                    i += 3;
                    continue;
                }
            }
            out.push(raw[i]);
            i += 1;
        }
        out
    };
    Some((mime, bytes))
}

/// Standard base64 (RFC 4648, `=`-padded). The canvas bridge carries pixel
/// buffers in both directions as text — one allocation per frame instead of
/// a per-byte JS-to-Rust conversion.
fn b64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// B11: concatenate every text node under `node` — used to decide which
/// `unicode-range` faces the page actually exercises.
fn collect_dom_text(node: Option<kitae_dom::NodeId>, dom: &kitae_dom::Dom, out: &mut String) {
    let Some(id) = node else { return };
    let n = dom.node(id);
    out.push_str(&n.text);
    for &c in &n.children {
        collect_dom_text(Some(c), dom, out);
    }
}

/// Whether one declared face still needs an install pass. Split out of
/// [`Browser::ensure_page_fonts`] so the descriptor-aware skip is testable
/// without a real Core Text install.
///
/// B11: an exact (family, weight, style) match against the registry
/// short-circuits — a second weight of a loaded family is *not* skipped.
/// Also, a `unicode-range` face whose code points the page never touches is
/// not downloaded (per-glyph selection among installed faces is left to the
/// shell's font fallback chain), and `font-display: optional` faces are
/// never fetched (the page draws with system fallbacks). `block`/`auto`
/// keep the N5 install-before-paint path; `swap`/`fallback` behave like
/// `block` here — the engine installs synchronously, so there is no second
/// paint to swap into.
fn face_needs_install(face: &kitae_css::FontFace, doc_text: &str) -> bool {
    if kitae_shape::font::web_font_registered(
        &face.family,
        face.font_weight,
        face.font_style.keyword(),
    ) {
        return false;
    }
    if !face.intersects_text(doc_text) {
        return false;
    }
    face.font_display != kitae_css::FontDisplay::Optional
}

/// Font formats the engine accepts from `@font-face src` (Phase N5).
/// `eot`/`svg` are legacy-only and skipped; anything unhinted is tried.
fn font_format_supported(format: Option<&str>) -> bool {
    match format.map(|f| f.to_ascii_lowercase()) {
        None => true,
        Some(f) => matches!(
            f.trim(),
            "woff2" | "woff" | "truetype" | "opentype" | "truetype-collection" | "collection"
        ),
    }
}

/// Session staging dir for downloaded web fonts (kept for the process
/// lifetime so registered faces stay mapped; the OS reclaims temp).
fn web_font_stage_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("kitae_fonts_{}", std::process::id()))
}

/// Decode a WOFF/WOFF2 container into raw sfnt (Q11); `None` for anything
/// else, which keeps the raw-sfnt registration paths.
fn decode_web_font_container(bytes: &[u8]) -> Option<Vec<u8>> {
    match bytes.get(0..4) {
        Some(b"wOF2") => kitae_media::decode_woff2(bytes).ok(),
        Some(b"wOFF") => kitae_media::decode_woff(bytes).ok(),
        _ => None,
    }
}

/// Register a decoded sfnt through the in-memory CGFont path and mark the
/// family live in the shaper.
fn register_decoded_web_font(face: &kitae_css::FontFace, sfnt: Vec<u8>) -> bool {
    #[cfg(target_os = "macos")]
    {
        if !ctfonts::register_font_bytes(sfnt) {
            return false;
        }
    }
    #[cfg(not(target_os = "macos"))]
    drop(sfnt);
    kitae_shape::font::register_web_font_face(
        &face.family,
        face.font_weight,
        face.font_style.keyword(),
    );
    true
}

/// Download, install and register one `@font-face` family (Phase N5,
/// Q11). Supported `src:` entries are tried in list order until one
/// registers — spec fallback behavior. WOFF/WOFF2 containers decode to
/// raw sfnt at fetch and register in memory (CGDataProvider + CGFont, no
/// temp files — Core Text cannot parse the containers); raw sfnt keeps
/// the file-URL path. Returns true when the family registered
/// (already-registered families short-circuit in the caller).
fn ensure_web_font(face: &kitae_css::FontFace, resource_base: &str, document_base: &str) -> bool {
    for src in face
        .srcs
        .iter()
        .filter(|s| font_format_supported(s.format.as_deref()))
    {
        if ensure_web_font_src(face, resource_base, document_base, &src.url) {
            return true;
        }
    }
    false
}

fn ensure_web_font_src(
    face: &kitae_css::FontFace,
    resource_base: &str,
    document_base: &str,
    src_url: &str,
) -> bool {
    let abs = resolve(resource_base, src_url).unwrap_or_else(|| src_url.to_string());
    // I6: mixed-content policy — web fonts are active content; an https page
    // never loads one over plaintext.
    if !kitae_net::mixed_content_allowed(document_base, &abs) {
        return false;
    }
    let path = if let Some(local) = abs.strip_prefix("file://") {
        std::path::PathBuf::from(local)
    } else {
        let resp = match fetch_cached(&abs) {
            Ok(r) if r.status < 400 => r,
            _ => return false,
        };
        if let Some(sfnt) = decode_web_font_container(&resp.body) {
            return register_decoded_web_font(face, sfnt);
        }
        let file = web_font_stage_dir().join(format!(
            "{}-{:x}{}",
            sanitize_font_stem(&face.family),
            fnv1a(&abs),
            font_extension(&abs)
        ));
        if std::fs::create_dir_all(web_font_stage_dir()).is_err()
            || std::fs::write(&file, &resp.body).is_err()
        {
            return false;
        }
        file
    };
    if !path.is_file() {
        return false;
    }
    // file:// fonts: a container decodes in memory; raw sfnt registers
    // in place via URL (no copy).
    if let Ok(bytes) = std::fs::read(&path) {
        if let Some(sfnt) = decode_web_font_container(&bytes) {
            return register_decoded_web_font(face, sfnt);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if !ctfonts::register_font_file(&path) {
            return false;
        }
    }
    kitae_shape::font::register_web_font_face(
        &face.family,
        face.font_weight,
        face.font_style.keyword(),
    );
    true
}

fn sanitize_font_stem(family: &str) -> String {
    let stem: String = family
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let stem = stem.trim_matches('_').to_string();
    if stem.is_empty() {
        "font".to_string()
    } else {
        stem
    }
}

fn fnv1a(s: &str) -> u64 {
    let mut h = 1469598103934665603u64;
    for b in s.bytes() {
        h = (h ^ b as u64).wrapping_mul(1099511628211);
    }
    h
}

fn font_extension(url: &str) -> &'static str {
    let base = url.split(['?', '#']).next().unwrap_or("");
    match base.rsplit('.').next().map(|e| e.to_ascii_lowercase()) {
        Some(e) if e == "woff2" => ".woff2",
        Some(e) if e == "woff" => ".woff",
        Some(e) if e == "ttc" => ".ttc",
        Some(e) if e == "otf" => ".otf",
        _ => ".ttf",
    }
}

/// Core Text font installation (macOS only; other platforms record the
/// family and let the host install from the staged file).
#[cfg(target_os = "macos")]
mod ctfonts {
    use std::ffi::c_void;

    type CFRef = *const c_void;

    #[link(name = "CoreText", kind = "framework")]
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: CFRef,
            buf: *const u8,
            len: isize,
            is_dir: i32,
        ) -> CFRef;
        fn CTFontManagerRegisterFontsForURL(url: CFRef, scope: u32, error: *mut CFRef) -> i32;
        fn CFRelease(obj: CFRef);
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGDataProviderCreateWithData(
            info: *mut c_void,
            data: *const c_void,
            size: isize,
            release: Option<unsafe extern "C" fn(*mut c_void, *const c_void, isize)>,
        ) -> CFRef;
        fn CGFontCreateWithDataProvider(provider: CFRef) -> CFRef;
        fn CTFontManagerRegisterGraphicsFont(font: CFRef, error: *mut CFRef) -> i32;
    }

    /// `kCTFontManagerScopeProcess` — visible to this process only.
    const PROCESS_SCOPE: u32 = 1;

    pub fn register_font_file(path: &std::path::Path) -> bool {
        let bytes = path.as_os_str().as_encoded_bytes();
        unsafe {
            struct Owned(CFRef);
            impl Drop for Owned {
                fn drop(&mut self) {
                    if !self.0.is_null() {
                        unsafe { CFRelease(self.0) };
                    }
                }
            }
            let url = Owned(CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                bytes.as_ptr(),
                bytes.len() as isize,
                0,
            ));
            if url.0.is_null() {
                return false;
            }
            CTFontManagerRegisterFontsForURL(url.0, PROCESS_SCOPE, std::ptr::null_mut()) != 0
        }
    }

    /// Q11: register an in-memory sfnt (decoded WOFF/WOFF2) via
    /// CGDataProvider → CGFont → CTFontManagerRegisterGraphicsFont — the
    /// URL-scoped registration cannot accept bytes. The buffer is leaked
    /// on purpose: registered faces must stay mapped for the process
    /// lifetime, matching the staging-dir behavior. A collection (ttcf)
    /// yields its first face.
    pub fn register_font_bytes(bytes: Vec<u8>) -> bool {
        let data = Box::leak(bytes.into_boxed_slice());
        unsafe {
            struct Owned(CFRef);
            impl Drop for Owned {
                fn drop(&mut self) {
                    if !self.0.is_null() {
                        unsafe { CFRelease(self.0) };
                    }
                }
            }
            let provider = Owned(CGDataProviderCreateWithData(
                std::ptr::null_mut(),
                data.as_ptr() as *const c_void,
                data.len() as isize,
                None,
            ));
            if provider.0.is_null() {
                return false;
            }
            let font = Owned(CGFontCreateWithDataProvider(provider.0));
            if font.0.is_null() {
                return false;
            }
            CTFontManagerRegisterGraphicsFont(font.0, std::ptr::null_mut()) != 0
        }
    }
}

// ---------------------------------------------------------------------------
// QTSQ media side-table (J4): `qtsq_viewer_page` is a free function (no
// `&mut self`), so media metadata flows to the Browser through this
// URL-keyed store, mirroring the raster store above. The audio branch
// decodes on the engine thread and writes the temp CAF that the embedder's
// system player plays (the engine decodes; the embedder plays).
// ---------------------------------------------------------------------------

struct MediaEntry {
    info: MediaInfo,
    refs: usize,
}

struct MediaStore {
    by_url: HashMap<String, MediaEntry>,
}

static MEDIA_FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn media_temp_path(prefix: &str) -> std::path::PathBuf {
    let sequence = MEDIA_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "kitae_{prefix}_{}_{sequence}.caf",
        std::process::id()
    ))
}

fn remove_media_file(path: &str) {
    if !path.is_empty() {
        let _ = std::fs::remove_file(path);
    }
}

fn qtsq_media_store() -> &'static Mutex<MediaStore> {
    static STORE: OnceLock<Mutex<MediaStore>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(MediaStore {
            by_url: HashMap::new(),
        })
    })
}

fn qtsq_register_media(info: MediaInfo) {
    let url = info.url.clone();
    let new_path = info.caf_path.clone();
    let old_path = {
        let mut store = qtsq_media_store().lock().unwrap();
        if let Some(entry) = store.by_url.get_mut(&url) {
            entry.refs = entry.refs.saturating_add(1);
            let old_path = entry.info.caf_path.clone();
            entry.info = info;
            Some(old_path)
        } else {
            store
                .by_url
                .insert(url.clone(), MediaEntry { info, refs: 1 });
            None
        }
    };
    if old_path.as_deref() != Some(new_path.as_str()) {
        if let Some(old_path) = old_path {
            remove_media_file(&old_path);
        }
    }
}

fn qtsq_media_for_url(url: &str) -> Option<MediaInfo> {
    qtsq_media_store()
        .lock()
        .unwrap()
        .by_url
        .get(url)
        .map(|entry| entry.info.clone())
}

pub fn clear_qtsq_media_for(url: &str) {
    let path = {
        let mut store = qtsq_media_store().lock().unwrap();
        let Some(entry) = store.by_url.get_mut(url) else {
            return;
        };
        if entry.refs > 1 {
            entry.refs -= 1;
            None
        } else {
            store.by_url.remove(url).map(|entry| entry.info.caf_path)
        }
    };
    if let Some(path) = path {
        remove_media_file(&path);
    }
}

pub fn clear_qtsq_media() {
    let paths = {
        let mut store = qtsq_media_store().lock().unwrap();
        store
            .by_url
            .drain()
            .filter_map(|(_, entry)| {
                (!entry.info.caf_path.is_empty()).then_some(entry.info.caf_path)
            })
            .collect::<Vec<_>>()
    };
    for path in paths {
        remove_media_file(&path);
    }
}

fn release_qtsq_page_state(url: &str) {
    if !url.is_empty() {
        clear_qtsq_media_for(url);
        clear_qtsq_videos_for(url);
        clear_qtsq_raster_for(url);
    }
}

/// Decode QTSQ audio on the engine thread and hand it to the embedder as a
/// temp CAF (Core Audio Format, lpcm 16-bit signed little-endian — the one
/// container AVAudioPlayer reads that is trivial to write with no deps).
/// Returns the registered `MediaInfo` with the CAF path filled in.
fn qtsq_prepare_audio(
    url: &str,
    body: &[u8],
    info: &kitae_qtsq::QtsqInfo,
    password: Option<&str>,
) -> Option<MediaInfo> {
    let header_rate = info.sample_rate.unwrap_or(0);
    let header_channels = match info.data_type {
        kitae_qtsq::QtsqDataType::AudioMono => 1,
        kitae_qtsq::QtsqDataType::AudioStereo => 2,
        _ => info.channels,
    };
    let mut pcm = kitae_qtsq::decode_audio(body, password).ok()?;
    let rate = pcm.sample_rate.max(header_rate);
    let channels = pcm.channels.max(header_channels);
    if rate == 0 || channels == 0 {
        return None;
    }
    pcm.sample_rate = rate;
    pcm.channels = channels;
    let caf_path = media_temp_path("qtsq_audio");
    write_caf(&caf_path, &pcm).ok()?;
    let duration = if channels > 0 {
        pcm.samples.len() as f32 / channels as f32 / rate as f32
    } else {
        0.0
    };
    Some(MediaInfo {
        url: url.to_string(),
        kind: MediaKind::Audio,
        duration,
        rate,
        channels,
        playing: false,
        position: 0.0,
        caf_path: caf_path.to_string_lossy().into_owned(),
    })
}

/// Write interleaved f32 samples (`[-1, 1]`) as a CAF file: 'caff' magic,
/// 'desc' (lpcm, little-endian int16), 'data'. Hand-rolled on purpose —
/// zero dependencies, and the format is a handful of fixed-size fields.
/// Container fields are big-endian per the CAF spec (verified against
/// afconvert -f caff -d LEI16 output); only the audio samples follow the
/// endianness the desc declares.
fn write_caf(path: &std::path::Path, pcm: &kitae_qtsq::PCMAudio) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    // 'caff' + version (1) + flags (0), big-endian.
    f.write_all(b"caff")?;
    f.write_all(&1u16.to_be_bytes())?;
    f.write_all(&0u16.to_be_bytes())?;
    // 'desc' chunk: 32-byte audio description. Note the spec layout has no
    // mBytesPerFrame (it's bytesPerPacket / framesPerPacket for lpcm).
    f.write_all(b"desc")?;
    f.write_all(&32u64.to_be_bytes())?;
    f.write_all(&(pcm.sample_rate as f64).to_be_bytes())?; // mSampleRate
    f.write_all(b"lpcm")?; // mFormatID
                           // mFormatFlags: kCAFLinearPCMFormatFlagIsLittleEndian (1 << 1) — the
                           // samples below are little-endian int16, exactly what afconvert's
                           // LEI16 produces (signed integer + packed are implied for lpcm).
    f.write_all(&2u32.to_be_bytes())?;
    let bytes_per_packet = 2 * pcm.channels as u32;
    f.write_all(&bytes_per_packet.to_be_bytes())?; // mBytesPerPacket
    f.write_all(&1u32.to_be_bytes())?; // mFramesPerPacket (lpcm)
    f.write_all(&(pcm.channels as u32).to_be_bytes())?; // mChannelsPerFrame
    f.write_all(&16u32.to_be_bytes())?; // mBitsPerChannel
                                        // 'data' chunk: real length, edit count 0, interleaved LE int16 frames.
    let data_len = (pcm.samples.len() * 2) as u64;
    f.write_all(b"data")?;
    f.write_all(&data_len.to_be_bytes())?;
    f.write_all(&0u32.to_be_bytes())?;
    let mut buf = Vec::with_capacity(pcm.samples.len() * 2);
    for &s in &pcm.samples {
        let clamped = s.clamp(-1.0, 1.0);
        buf.extend_from_slice(&((clamped * 32767.0) as i16).to_le_bytes());
    }
    f.write_all(&buf)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// QTSQ video stepping (J5): `qtsq_viewer_page` opens the decoder when the
// viewer page is built and parks it here, keyed by URL. `Browser::relayout`
// and `publish_current` step it to the transport position and register the
// decoded frame as an ordinary raster — video is literally images plus a
// clock, reusing the J3 raster path end to end.
// ---------------------------------------------------------------------------

struct QtsqVideoCache {
    bytes: Vec<u8>,
    decoder: kitae_qtsq::VideoDecoder,
    last_index: Option<u32>,
}

const QTSQ_VIDEO_CACHE_MAX_ENTRIES: usize = 8;
const QTSQ_VIDEO_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

struct QtsqVideoStore {
    by_url: HashMap<String, QtsqVideoCache>,
    order: Vec<String>,
    bytes: usize,
}

fn qtsq_video_store() -> &'static Mutex<QtsqVideoStore> {
    static STORE: OnceLock<Mutex<QtsqVideoStore>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(QtsqVideoStore {
            by_url: HashMap::new(),
            order: Vec::new(),
            bytes: 0,
        })
    })
}

fn qtsq_insert_video(
    store: &mut QtsqVideoStore,
    url: String,
    body: Vec<u8>,
    decoder: kitae_qtsq::VideoDecoder,
) {
    if let Some(old) = store.by_url.remove(&url) {
        store.bytes = store.bytes.saturating_sub(old.bytes.len());
        store.order.retain(|key| key != &url);
    }
    store.bytes = store.bytes.saturating_add(body.len());
    store.by_url.insert(
        url.clone(),
        QtsqVideoCache {
            bytes: body,
            decoder,
            last_index: None,
        },
    );
    store.order.push(url);
    while store.by_url.len() > QTSQ_VIDEO_CACHE_MAX_ENTRIES
        || (store.bytes > QTSQ_VIDEO_CACHE_MAX_BYTES && store.by_url.len() > 1)
    {
        let Some(oldest) = store.order.first().cloned() else {
            break;
        };
        if let Some(old) = store.by_url.remove(&oldest) {
            store.bytes = store.bytes.saturating_sub(old.bytes.len());
        }
        store.order.remove(0);
    }
}

/// Park a freshly opened decoder and its source bytes for a QTSQ video URL.
fn qtsq_register_video(url: &str, body: &[u8], decoder: kitae_qtsq::VideoDecoder) {
    let mut store = qtsq_video_store().lock().unwrap();
    qtsq_insert_video(&mut store, url.to_string(), body.to_vec(), decoder);
}

fn qtsq_touch_video(store: &mut QtsqVideoStore, url: &str) {
    store.order.retain(|key| key != url);
    if store.by_url.contains_key(url) {
        store.order.push(url.to_string());
    }
}

pub fn clear_qtsq_videos_for(url: &str) {
    let mut store = qtsq_video_store().lock().unwrap();
    if let Some(old) = store.by_url.remove(url) {
        store.bytes = store.bytes.saturating_sub(old.bytes.len());
    }
    store.order.retain(|key| key != url);
}

/// Decode the frame at `position_secs` for the QTSQ video at `url` and
/// register it as a raster under the same URL. The decoder is cached (opened
/// once per navigation; `frame_index = position × fps` clamped to the last
/// frame), so stepping only re-decodes pixels. Returns `None` when the URL
/// has no decoder, the cached bytes are gone, or decode fails — the stamping
/// pass then leaves the frame image as a placeholder.
fn qtsq_step_video(url: &str, position_secs: f32) -> Option<QtsqRaster> {
    let mut store = qtsq_video_store().lock().unwrap();
    if !store.by_url.contains_key(url) {
        let body = fetch_cached(url).ok()?.body;
        let info = kitae_qtsq::probe(&body)?;
        if !info.data_type.is_video() {
            return None;
        }
        let decoder = kitae_qtsq::VideoDecoder::open(&body, None).ok()?;
        qtsq_insert_video(&mut store, url.to_string(), body, decoder);
    }
    qtsq_touch_video(&mut store, url);
    let index = store.by_url.get(url)?.decoder.frame_index(position_secs)?;
    let reuse = store
        .by_url
        .get(url)
        .and_then(|cached| (cached.last_index == Some(index)).then_some(()))
        .is_some();
    if reuse {
        if let Some(raster) = qtsq_raster_for_url(url) {
            return Some(raster);
        }
    }
    let frame = {
        let cached = store.by_url.get_mut(url)?;
        if cached.bytes.is_empty() {
            return None;
        }
        let frame = cached.decoder.frame(index).ok()?;
        cached.last_index = Some(index);
        frame
    };
    drop(store);
    Some(qtsq_register_raster(
        url,
        frame.width,
        frame.height,
        frame.pixels,
    ))
}

/// Drop every cached video decoder (load-path reset + history-clear plumbing;
/// rasters they produced go through `clear_qtsq_rasters`).
pub fn clear_qtsq_videos() {
    let mut store = qtsq_video_store().lock().unwrap();
    store.by_url.clear();
    store.order.clear();
    store.bytes = 0;
}

// ---------------------------------------------------------------------------
// Web media (F1–F5): the same pipeline the QTSQ path uses, fed by page
// subresources instead of archive payloads. `<video>` demuxes through
// kitae_media and the embedder decodes frames through the video FFI (the
// engine never decodes video pixels); `<audio>` decodes through symphonia
// to a temp CAF exactly like QTSQ audio; `<track>` captions parse to VTT
// cues. A media element's resource is picked by HTML's selection algorithm
// over its `<source>` children (see `media_candidates`/`select_web_media`):
// first candidate whose bytes decode, the element's own `src` last, and the
// winner is stamped into the media item's `src` so the frame's v20 media
// section and the engine's decode state agree. Everything is keyed by that
// resolved media URL, and `web_media_tried` keeps a relayout from re-fetching
// a candidate the pipeline already failed on. Documented limitation: one
// transport session per page (the first media item); extra items show their
// first frame only.
// ---------------------------------------------------------------------------

/// Demuxed web video for one URL (F2/F3): track metadata + compressed
/// samples. The embedder's VideoToolbox decoder pulls samples by index and
/// feeds decoded RGBA frames back through [`web_video_feed_frame`].
struct WebVideoMeta {
    codec: kitae_media::video::VideoCodec,
    width: u32,
    height: u32,
    extradata: Vec<u8>,
    fps: f64,
    duration_ms: f64,
    samples: Vec<kitae_media::video::VideoSample>,
}

fn web_video_meta() -> &'static Mutex<HashMap<String, WebVideoMeta>> {
    static STORE: OnceLock<Mutex<HashMap<String, WebVideoMeta>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Transport metadata for web audio/video (the docked bar's source), keyed
/// by resolved media URL.
fn web_media_store() -> &'static Mutex<HashMap<String, MediaInfo>> {
    static STORE: OnceLock<Mutex<HashMap<String, MediaInfo>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Which media document each page drives (page URL → resolved media URL).
/// The first media item to select a resource wins, and that item keeps
/// re-pointing the entry as it re-selects (see [`associate_web_media`]);
/// cleared on navigation with [`clear_web_media_for`].
fn web_page_media() -> &'static Mutex<HashMap<String, String>> {
    static MAP: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Every media URL each page has ensured (page URL → all media URLs), so a
/// navigation can drop exactly that page's decode state — the stores are
/// keyed by media URL, but the association lives here. Concurrent engines
/// (tabs, parallel tests) must not wipe each other's pages.
fn web_page_media_items() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static MAP: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The page's transport media (its first media item — see
/// [`web_page_media`]) with the attributes that shape loading and playback
/// and the candidates a deferred load chooses from. First item wins, exactly
/// like the association map, so `media_play` can honor `loop` and run the
/// resource selection for a `preload="none"` source on demand.
#[derive(Clone)]
struct TransportMedia {
    attrs: MediaAttrs,
    is_audio: bool,
    /// The element's candidate resources — `<source>` children in document
    /// order, its own `src` last — resolved and mixed-content gated, exactly
    /// as the eager path built them. `preload="none"` walks this list when
    /// playback asks for the data.
    candidates: Vec<String>,
    /// The media element's DOM node, so a deferred load can re-stamp the
    /// display list's item with the candidate it ended up choosing.
    node: u32,
}

fn web_page_transport() -> &'static Mutex<HashMap<String, TransportMedia>> {
    static MAP: OnceLock<Mutex<HashMap<String, TransportMedia>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// VTT cues per video URL (F5).
fn web_captions() -> &'static Mutex<HashMap<String, Vec<kitae_media::vtt::Cue>>> {
    static STORE: OnceLock<Mutex<HashMap<String, Vec<kitae_media::vtt::Cue>>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Media URLs this session already attempted, so relayouts never re-fetch
/// (or re-decode) a source that failed. Cleared on navigation.
fn web_media_tried() -> &'static Mutex<std::collections::HashSet<String>> {
    static TRIED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    TRIED.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Drop every web media entry (test isolation; production resets go through
/// [`clear_web_media_for`] so concurrent engines keep their pages' media).
/// Fed frame rasters live in the shared raster store and age out through its
/// LRU.
pub fn clear_web_media() {
    let paths = {
        let mut store = web_media_store().lock().unwrap();
        let paths = store
            .drain()
            .filter_map(|(_, info)| (!info.caf_path.is_empty()).then_some(info.caf_path))
            .collect::<Vec<_>>();
        paths
    };
    web_video_meta().lock().unwrap().clear();
    web_page_media().lock().unwrap().clear();
    web_page_media_items().lock().unwrap().clear();
    web_page_transport().lock().unwrap().clear();
    web_captions().lock().unwrap().clear();
    web_media_tried().lock().unwrap().clear();
    for path in paths {
        remove_media_file(&path);
    }
}

/// Drop the web media state one page owns: its page→media associations, the
/// media-URL entries it registered, and the captions/tried marks keyed by
/// those URLs. Other engines' entries are untouched (multi-tab, parallel
/// tests share these global stores).
pub fn clear_web_media_for(page_url: &str) {
    let items = web_page_media_items()
        .lock()
        .unwrap()
        .remove(page_url)
        .unwrap_or_default();
    web_page_media().lock().unwrap().remove(page_url);
    web_page_transport().lock().unwrap().remove(page_url);
    if items.is_empty() {
        return;
    }
    {
        let mut meta = web_video_meta().lock().unwrap();
        for url in &items {
            meta.remove(url);
        }
    }
    let paths = {
        let mut store = web_media_store().lock().unwrap();
        items
            .iter()
            .filter_map(|url| store.remove(url).map(|info| info.caf_path))
            .filter(|path| !path.is_empty())
            .collect::<Vec<_>>()
    };
    for path in paths {
        remove_media_file(&path);
    }
    {
        let mut caps = web_captions().lock().unwrap();
        for url in &items {
            caps.remove(url);
        }
    }
    {
        let mut tried = web_media_tried().lock().unwrap();
        for url in &items {
            tried.remove(url);
        }
    }
}

/// File extension of a URL path (lowercased, query/fragment stripped).
fn url_ext(url: &str) -> String {
    url.split(['?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Register `url` as a media resource this page owns, so a navigation drops
/// its decode state and its "already tried" mark (see
/// [`clear_web_media_for`]). Deduped, and deliberately separate from
/// [`associate_web_media`]: a candidate the engine fetched and that failed to
/// decode is still the page's to clean up, but it never becomes the page's
/// transport.
fn own_web_media_url(page_url: &str, url: &str) {
    let mut items = web_page_media_items().lock().unwrap();
    let list = items.entry(page_url.to_string()).or_default();
    if !list.iter().any(|u| u == url) {
        list.push(url.to_string());
    }
}

/// Point the page at the media resource element `node` plays. The first
/// media item wins the transport, so another item never stomps an
/// association; but a *re-selection for the same element* replaces the URL it
/// had — that is a `<source media>` condition that started or stopped
/// matching across a relayout, or a deferred load whose first candidate
/// turned out to be undecodable.
fn associate_web_media(page_url: &str, url: &str, node: u32) {
    let transport_node = web_page_transport()
        .lock()
        .unwrap()
        .get(page_url)
        .map(|transport| transport.node);
    let mut page_media = web_page_media().lock().unwrap();
    match transport_node {
        // This element is the transport: its selection owns the URL.
        Some(owner) if owner == node => {
            page_media.insert(page_url.to_string(), url.to_string());
        }
        // Another media item is the transport (first item wins).
        Some(_) => {}
        // No transport recorded (a `preload="none"` element is associated
        // before it is fetched): the first claim wins.
        None => {
            page_media
                .entry(page_url.to_string())
                .or_insert_with(|| url.to_string());
        }
    }
}

/// Associate a page with one of its media items *and* register the URL as
/// its own: used when the engine already knows the URL, so there is nothing
/// to choose between (a single `src`, or a `preload="none"` item whose
/// selection waits for playback). Split out of [`select_web_media`] for the
/// same reason — a deferred item must be associated before it is fetched.
fn claim_web_media(page_url: &str, url: &str, node: u32) {
    own_web_media_url(page_url, url);
    associate_web_media(page_url, url, node);
}

/// Run the resource selection algorithm over `candidates` (in HTML's order):
/// the first one that fetches *and decodes* is the resource the element
/// plays. Every candidate tried is registered as the page's own, so a
/// navigation clears their fetch marks; the winner becomes the page's
/// transport. `None` when no candidate decodes — the element publishes no
/// media, which is also what an undecodable `src` did before `<source>`
/// children existed.
fn select_web_media(
    page_url: &str,
    node: u32,
    candidates: &[String],
    is_audio: bool,
) -> Option<String> {
    for url in candidates {
        own_web_media_url(page_url, url);
        if !load_web_media(url, is_audio) {
            continue;
        }
        associate_web_media(page_url, url, node);
        return Some(url.clone());
    }
    None
}

/// Fetch + decode a web media source once (F1/F2/F3), reporting whether the
/// bytes are usable: audio → CAF handoff, video → demux (samples stay here
/// for the embedder's decoder). `true` when the resource is loaded (or was
/// already), `false` when the fetch or the decode failed — which is what
/// makes the next `<source>` candidate worth trying. A URL is attempted at
/// most once per page visit (`web_media_tried`), so a relayout never re-fetches
/// a source that already failed.
fn load_web_media(url: &str, is_audio: bool) -> bool {
    if web_media_store().lock().unwrap().contains_key(url) {
        return true;
    }
    if !web_media_tried().lock().unwrap().insert(url.to_string()) {
        return false;
    }
    let Ok(resp) = fetch_cached(url) else {
        return false;
    };
    if is_audio {
        let Ok(dec) = kitae_media::audio::decode_audio(&resp.body, &url_ext(url)) else {
            return false;
        };
        let caf_path = media_temp_path("web_audio");
        let (dec_rate, dec_channels, dec_duration) = (dec.rate, dec.channels as u8, dec.duration());
        let pcm = kitae_qtsq::PCMAudio {
            sample_rate: dec_rate,
            channels: dec_channels,
            samples: dec.samples,
        };
        if write_caf(&caf_path, &pcm).is_err() {
            return false;
        }
        let info = MediaInfo {
            url: url.to_string(),
            kind: MediaKind::Audio,
            duration: dec_duration,
            rate: dec_rate,
            channels: dec_channels,
            playing: false,
            position: 0.0,
            caf_path: caf_path.to_string_lossy().into_owned(),
        };
        web_media_store()
            .lock()
            .unwrap()
            .insert(url.to_string(), info);
        return true;
    }
    let Ok(dv) = kitae_media::video::demux(&resp.body) else {
        return false;
    };
    let meta = WebVideoMeta {
        codec: dv.track.codec,
        width: dv.track.width,
        height: dv.track.height,
        extradata: dv.track.extradata,
        fps: dv.track.fps,
        duration_ms: dv.duration_ms,
        samples: dv.samples,
    };
    web_video_meta()
        .lock()
        .unwrap()
        .insert(url.to_string(), meta);
    let info = MediaInfo {
        url: url.to_string(),
        kind: MediaKind::Video,
        duration: (dv.duration_ms / 1000.0) as f32,
        rate: 0,
        channels: 0,
        playing: false,
        position: 0.0,
        caf_path: String::new(),
    };
    web_media_store()
        .lock()
        .unwrap()
        .insert(url.to_string(), info);
    true
}

/// `<track kind=captions|subtitles src>` children of a video element (F5):
/// fetch the first usable WebVTT sidecar and store its cues keyed by the
/// resolved video URL (an empty vec also counts as done — no refetch).
fn ensure_web_captions(video_url: &str, dom: &Dom, video: NodeId, base_url: &str) {
    if web_captions().lock().unwrap().contains_key(video_url) {
        return;
    }
    let mut cues = Vec::new();
    for child in dom.children(video) {
        let Some(el) = dom.element(*child) else {
            continue;
        };
        if el.tag != "track" {
            continue;
        }
        let kind = el.attr("kind").unwrap_or("");
        if kind != "captions" && kind != "subtitles" {
            continue;
        }
        let Some(src) = el.attr("src") else { continue };
        let abs = resolve(base_url, src).unwrap_or_else(|| src.to_string());
        if let Ok(resp) = fetch_cached(&abs) {
            cues.extend(kitae_media::vtt::parse_vtt(&String::from_utf8_lossy(
                &resp.body,
            )));
        }
        break;
    }
    web_captions()
        .lock()
        .unwrap()
        .insert(video_url.to_string(), cues);
}

/// The web media the current page drives (the `media_state` fallback after
/// the QTSQ store misses).
fn web_media_for_page(page_url: &str) -> Option<MediaInfo> {
    let media_url = web_page_media().lock().unwrap().get(page_url).cloned()?;
    web_media_store().lock().unwrap().get(&media_url).cloned()
}

/// The current frame raster for a web `<video>` (F2): the embedder decodes
/// compressed samples through its platform decoder and feeds pixels back via
/// [`web_video_feed_frame`], which registers them as an ordinary raster
/// keyed by the media URL — the stamping pass's `.or_else` fallback after
/// `qtsq_step_video`. `None` leaves the placeholder box (before the first
/// fed frame, or when the codec has no embedder decoder).
fn web_fed_video(url: &str) -> Option<QtsqRaster> {
    qtsq_raster_for_url(url)
}

/// The raster to stamp into a `<video>`'s frame image: the QTSQ archive frame
/// stepped to the transport position, else the frame the embedder fed for web
/// media, else the element's `poster` image. `poster` is the unresolved
/// attribute value; `base_url` resolves it and `document_base` gates it
/// (I6 mixed content), matching the `<img>` subresource rules — a poster
/// still loads under `preload="none"` because it is not media data.
fn video_frame_raster(
    url: &str,
    position_secs: f32,
    poster: &str,
    base_url: &str,
    document_base: &str,
) -> Option<QtsqRaster> {
    qtsq_step_video(url, position_secs)
        .or_else(|| web_fed_video(url))
        .or_else(|| {
            let abs = resolve(base_url, poster.trim())?;
            if !kitae_net::mixed_content_allowed(document_base, &abs) {
                return None;
            }
            ensure_web_raster(&abs)
        })
}

/// The cue active at `position_secs` (F5), if any.
fn active_cue<'a>(
    cues: &'a [kitae_media::vtt::Cue],
    position_secs: f32,
) -> Option<&'a kitae_media::vtt::Cue> {
    let ms = position_secs as f64 * 1000.0;
    cues.iter().find(|c| ms >= c.start_ms && ms < c.end_ms)
}

/// `m:ss` for transport labels and scrub math.
fn fmt_media_time(secs: f32) -> String {
    let s = secs.max(0.0).round() as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// The builtin chrome band for a media item (F4): a 40px strip docked to
/// the bottom of the element's rect, clamped when the element is shorter.
/// Shared by the paint pass and the click hit-test so they never drift.
fn media_chrome_band(m: &kitae_layout::DisplayMedia) -> (f32, f32, f32, f32) {
    let bh = m.h.min(40.0);
    (m.x, m.y + m.h - bh, m.w, bh)
}

/// Web video decode state for the embedder's platform decoder (F2/F6 wire).
/// All keyed by the resolved media URL; `0` codec means "no web video".
/// Codec ids match the `kitae_core.h` v20 table: 1 H264, 2 HEVC, 3 VP8,
/// 4 VP9, 5 AV1.
pub fn web_video_codec(url: &str) -> u32 {
    use kitae_media::video::VideoCodec;
    web_video_meta()
        .lock()
        .unwrap()
        .get(url)
        .map(|v| match v.codec {
            VideoCodec::H264 => 1,
            VideoCodec::Hevc => 2,
            VideoCodec::Vp8 => 3,
            VideoCodec::Vp9 => 4,
            VideoCodec::Av1 => 5,
        })
        .unwrap_or(0)
}

pub fn web_video_dims(url: &str) -> Option<(u32, u32)> {
    web_video_meta()
        .lock()
        .unwrap()
        .get(url)
        .map(|v| (v.width, v.height))
}

pub fn web_video_fps(url: &str) -> f64 {
    web_video_meta()
        .lock()
        .unwrap()
        .get(url)
        .map(|v| v.fps)
        .unwrap_or(0.0)
}

/// Copy the track's codec extradata (avcC/hvcC/av1C) into `buf`; returns the
/// full length so the caller can retry with a larger buffer.
pub fn web_video_extradata(url: &str, buf: &mut [u8]) -> usize {
    let store = web_video_meta().lock().unwrap();
    let Some(v) = store.get(url) else { return 0 };
    let n = v.extradata.len().min(buf.len());
    buf[..n].copy_from_slice(&v.extradata[..n]);
    v.extradata.len()
}

pub fn web_video_sample_count(url: &str) -> usize {
    web_video_meta()
        .lock()
        .unwrap()
        .get(url)
        .map(|v| v.samples.len())
        .unwrap_or(0)
}

/// Copy compressed sample `index` into `buf`; returns the full sample length
/// (0 when the URL or index misses) so the caller can size and retry.
pub fn web_video_sample(url: &str, index: usize, buf: &mut [u8]) -> usize {
    let store = web_video_meta().lock().unwrap();
    let Some(v) = store.get(url) else { return 0 };
    let Some(s) = v.samples.get(index) else {
        return 0;
    };
    let n = s.data.len().min(buf.len());
    buf[..n].copy_from_slice(&s.data[..n]);
    s.data.len()
}

pub fn web_video_sample_keyframe(url: &str, index: usize) -> bool {
    web_video_meta()
        .lock()
        .unwrap()
        .get(url)
        .and_then(|v| v.samples.get(index))
        .map(|s| s.keyframe)
        .unwrap_or(false)
}

/// The embedder decoded one frame of the web video at `url` (F2): register
/// the RGBA pixels as an ordinary raster under the media URL so the stamping
/// pass picks them up on the next publish. The raster store's stable ids
/// mean hosts just re-resolve the payload — video is images plus a clock.
pub fn web_video_feed_frame(url: &str, width: u32, height: u32, rgba: Vec<u8>) {
    qtsq_register_raster(url, width, height, rgba);
}

impl Browser {
    /// F4/F5: builtin transport chrome + active caption overlay for the
    /// page's media, painted in `decorate_caret` (the last decoration pass)
    /// with `node == u32::MAX` sentinel stamps. The pass strips its own
    /// previous output first, so republishes never accumulate duplicates;
    /// the sentinel is engine-internal and never serialized on the wire.
    fn paint_media_chrome(&self, out: &mut DisplayList) {
        out.rects.retain(|r| r.node != u32::MAX);
        out.texts.retain(|t| t.node != u32::MAX);
        let Some(info) = self.media_state() else {
            return;
        };
        let base_url = self
            .page
            .as_ref()
            .map(|p| p.resource_base_url.clone())
            .unwrap_or_else(|| self.page_url.clone());
        let bands: Vec<(f32, f32, f32, f32, String, bool)> = out
            .media
            .iter()
            .filter(|m| m.controls)
            .map(|m| {
                let url = resolve(&base_url, &m.src).unwrap_or_else(|| m.src.clone());
                (
                    media_chrome_band(m),
                    url,
                    m.kind == kitae_layout::MediaKind::Video,
                )
            })
            .map(|((bx, by, bw, bh), url, is_video)| (bx, by, bw, bh, url, is_video))
            .collect();
        for (bx, by, bw, bh, url, is_video) in bands {
            // Backdrop strip.
            out.rects
                .push(kitae_layout::Rect::solid(bx, by, bw, bh, 0x000000B4));
            out.rects.last_mut().unwrap().node = u32::MAX;
            // Play/pause glyph, left 44px zone: five centered vertical
            // slices form a right-pointing triangle; two bars when playing.
            if info.playing {
                for dx in [14.0, 22.0] {
                    out.rects.push(kitae_layout::Rect::solid(
                        bx + dx,
                        by + bh / 2.0 - 8.0,
                        4.0,
                        16.0,
                        0xFFFFFFFF,
                    ));
                    out.rects.last_mut().unwrap().node = u32::MAX;
                }
            } else {
                for i in 0..5 {
                    let h = 16.0 - i as f32 * 3.2;
                    out.rects.push(kitae_layout::Rect::solid(
                        bx + 14.0 + i as f32 * 2.0,
                        by + bh / 2.0 - h / 2.0,
                        2.0,
                        h,
                        0xFFFFFFFF,
                    ));
                    out.rects.last_mut().unwrap().node = u32::MAX;
                }
            }
            // Scrubber: dim track, white fill, square thumb. The right
            // ~64px stays clear for the time label.
            let track_x = bx + 44.0;
            let track_w = (bw - 116.0).max(20.0);
            out.rects.push(kitae_layout::Rect::solid(
                track_x,
                by + bh / 2.0 - 2.0,
                track_w,
                4.0,
                0xFFFFFF59,
            ));
            out.rects.last_mut().unwrap().node = u32::MAX;
            let progress = if info.duration > 0.0 {
                (info.position / info.duration).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let fill_w = track_w * progress;
            out.rects.push(kitae_layout::Rect::solid(
                track_x,
                by + bh / 2.0 - 2.0,
                fill_w,
                4.0,
                0xFFFFFFFF,
            ));
            out.rects.last_mut().unwrap().node = u32::MAX;
            out.rects.push(kitae_layout::Rect::solid(
                track_x + fill_w - 4.0,
                by + bh / 2.0 - 6.0,
                8.0,
                8.0,
                0xFFFFFFFF,
            ));
            out.rects.last_mut().unwrap().node = u32::MAX;
            // Time label, right-aligned in the right zone.
            let msg = format!(
                "{} / {}",
                fmt_media_time(info.position),
                fmt_media_time(info.duration)
            );
            let tw = self
                .measurer
                .measure_family(&msg, 12.0, 400, false, false, "");
            let (asc, _) = self.measurer.metrics_family(12.0, false, "");
            let tx = (bx + bw - tw - 12.0).max(track_x + track_w + 8.0);
            out.texts.push(kitae_layout::TextRun {
                x: tx,
                y: by + bh * 0.5 + asc * 0.35,
                text: msg,
                size: 12.0,
                color: 0xFFFFFFFF,
                node: u32::MAX,
                ..Default::default()
            });
            // F5: the active caption rides above the chrome band — a black
            // offset duplicate under white text for legibility. VTT newlines
            // flatten to spaces (single-line overlay, documented).
            if is_video {
                let cues = web_captions().lock().unwrap();
                if let Some(cue) = cues.get(&url).and_then(|c| active_cue(c, info.position)) {
                    let text = cue.text.replace('\n', " ");
                    if !text.is_empty() {
                        let size = 14.0;
                        let cw = self
                            .measurer
                            .measure_family(&text, size, 400, false, false, "");
                        let _ = self.measurer.metrics_family(size, false, "");
                        let cx = (bx + (bw - cw) / 2.0).max(bx);
                        let cy = by - 10.0;
                        out.texts.push(kitae_layout::TextRun {
                            x: cx + 1.0,
                            y: cy + 1.0,
                            text: text.clone(),
                            size,
                            color: 0x000000FF,
                            node: u32::MAX,
                            ..Default::default()
                        });
                        out.texts.push(kitae_layout::TextRun {
                            x: cx,
                            y: cy,
                            text,
                            size,
                            color: 0xFFFFFFFF,
                            node: u32::MAX,
                            ..Default::default()
                        });
                    }
                }
            }
        }
    }

    /// F4: hit-test + act on the builtin transport chrome (the band geometry
    /// mirrors [`Self::paint_media_chrome`]). Returns true when the click was
    /// consumed: the left zone toggles play/pause, the scrubber seeks, the
    /// time zone swallows without action. Runs after form controls and
    /// toggles, before links.
    fn click_media_chrome(&mut self, x: f32, y: f32) -> bool {
        let Some(info) = self.media_state() else {
            return false;
        };
        let Some(m) = self
            .last_dl
            .as_ref()
            .and_then(|dl| dl.media.iter().find(|m| m.controls))
            .cloned()
        else {
            return false;
        };
        let (bx, by, bw, bh) = media_chrome_band(&m);
        if x < bx || x > bx + bw || y < by || y > by + bh {
            return false;
        }
        let track_x = bx + 44.0;
        let track_w = (bw - 116.0).max(20.0);
        if x < bx + 40.0 {
            // Play/pause toggle: a play request at the end of a `loop`ed
            // resource restarts it, so the rewind applies here.
            self.set_transport(!info.playing, info.position, true);
        } else if x >= track_x && x <= track_x + track_w && info.duration > 0.0 {
            // Scrubber: a seek to the end is a seek, not the end of playback.
            let p = ((x - track_x) / track_w).clamp(0.0, 1.0);
            self.set_transport(info.playing, p * info.duration, false);
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Headless helpers
// ---------------------------------------------------------------------------

/// A [`TextMeasurer`] good enough for tests and headless dumps: 0.5×size per
/// character (0.6× for monospace), 0.8/0.2 ascent/descent.
pub struct ApproxMeasurer;

impl TextMeasurer for ApproxMeasurer {
    fn measure(&self, text: &str, size: f32, _weight: u32, _italic: bool, monospace: bool) -> f32 {
        text.chars().count() as f32 * size * if monospace { 0.6 } else { 0.5 }
    }
    fn metrics(&self, size: f32, _monospace: bool) -> (f32, f32) {
        (size * 0.8, size * 0.2)
    }
}

/// Headless render for `kitae --dump <url>`: prints the visible text (line by
/// line) and the resolved links.
pub fn run_dump(raw: &str) {
    let url = normalize_input(raw);
    // The dump shares the engine's base rule: a saved-page archive keeps
    // the .qtsq file URL as the session URL but resolves links and the
    // DOM environment against the archived page's original base.
    let ((html, session_base), doc_base, _outcome, _locked, _sig) = fetch_result_state(&url, None);
    let base = doc_base.unwrap_or(session_base);
    let (mut dom, root, mut styles) = prepare_in(&html, &base, 640.0);
    let resource_base = resource_base_url(&dom, root, &base);
    if let Some(r) = root {
        if needs_interstitial(&dom, r) {
            let interstitial = interstitial_html(&url);
            let (d, _r, s) = prepare_in(&interstitial, &url, 640.0);
            dom = d;
            styles = s;
            println!("[JavaScript-only page — showing the Kitae interstitial]");
        }
    }

    let dl = kitae_layout::layout(&dom, &styles, 640.0, 480.0, &ApproxMeasurer);

    let mut runs = dl.texts.clone();
    runs.sort_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)));
    let mut line_y = f32::MIN;
    for t in &runs {
        if (t.y - line_y).abs() > 1.0 {
            println!();
            line_y = t.y;
        }
        print!("{} ", t.text);
    }
    println!();
    println!("--- links ---");
    let mut last = String::new();
    for l in &dl.links {
        let abs = resolve(&resource_base, &l.href).unwrap_or_else(|| l.href.clone());
        if abs != last {
            println!("{}", abs);
            last = abs;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests (pure pipeline logic — no window, no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use kitae_style::ColorScheme;

    #[test]
    fn noscript_swap_replaces_a_shell_body() {
        // Body has < 200 visible chars; the real content hides in noscript.
        let html = "<html><head><script src=\"a.js\"></script></head><body>\
<div id=\"app\">Loading…</div>\
<noscript><h1>Real Article</h1><p>The full text lives here.</p></noscript>\
</body></html>";
        let (mut dom, root) = parse(html);
        let root = root.unwrap();
        assert!(prefer_noscript(&mut dom, root));
        let body = find_element(&dom, root, "body").unwrap();
        assert!(text_len(&dom, body) > 30, "noscript text moved into body");
        assert!(find_element(&dom, root, "noscript").is_none());
        // The h1 is now a direct part of the body subtree.
        let h1 = find_element(&dom, root, "h1").unwrap();
        let kid = dom.children(h1)[0];
        assert_eq!(dom.node(kid).text, "Real Article");
    }

    #[test]
    fn noscript_swap_skipped_when_body_has_real_text() {
        let html = "<html><body><p>Quite a lot of visible content is already present \
on this page — well over the two hundred visible characters that would ever \
justify preferring the noscript fallback. A reader browser must trust the \
author's own markup whenever the served document actually carries the words \
people came here for, and only reach for the fallback when it does not.</p>\
<noscript><p>fallback</p></noscript></body></html>";
        let (mut dom, root) = parse(html);
        let root = root.unwrap();
        assert!(!prefer_noscript(&mut dom, root));
        assert!(find_element(&dom, root, "noscript").is_some());
    }

    #[test]
    fn interstitial_triggers_on_empty_scripted_bodies() {
        let shell = "<html><body><div id=\"root\"></div><script>a</script>\
<script>b</script></body></html>";
        let (dom, root) = parse(shell);
        assert!(needs_interstitial(&dom, root.unwrap()));

        let normal = "<html><body><p>Real content lives here.</p></body></html>";
        let (dom, root) = parse(normal);
        assert!(!needs_interstitial(&dom, root.unwrap()));

        let one_script = "<html><body><script>a</script></body></html>";
        let (dom, root) = parse(one_script);
        assert!(
            !needs_interstitial(&dom, root.unwrap()),
            "needs ≥ 2 scripts"
        );
    }

    // ── H8: per-tab crash isolation ───────────────────────────────────────

    #[test]
    fn crash_page_renders_and_its_reload_link_refetches() {
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html("<h1>before</h1>", "about:crash-origin");
        assert_eq!(b.history.len(), 1, "the page itself, not the crash page");
        b.show_crash_page("unit test");
        assert_eq!(b.page_title, "This tab crashed — Kitae");
        assert_eq!(
            b.history.len(),
            1,
            "the crash page is history-free: Reload must refetch the crashed URL"
        );
        let link = b
            .links
            .iter()
            .find(|l| l.href == "kitae:reload")
            .expect("crash page offers a reload link");
        let (lx, ly, lw, lh) = (link.x, link.y, link.w, link.h);
        b.click(lx + lw / 2.0, ly + lh / 2.0);
        assert_eq!(
            b.state().url,
            "about:crash-origin",
            "reload returns to the crashed page's URL"
        );
        assert_ne!(
            b.page_title, "This tab crashed — Kitae",
            "the visible page moved on from the crash page"
        );
    }

    // ── H1: downloads ──────────────────────────────────────────────────────

    struct NotifyHost {
        messages: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Host for NotifyHost {
        fn publish(&mut self, _state: &PublicState, _dl: &DisplayList) {}
        fn scroll_updated(&mut self, _state: &PublicState) {}
        fn set_loading(&mut self, _loading: bool) {}
        fn open_external(&mut self, _url: &str) {}
        fn notify(&mut self, message: &str) {
            self.messages.lock().unwrap().push(message.to_string());
        }
    }

    struct CaptureHost {
        opened: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        messages: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Host for CaptureHost {
        fn publish(&mut self, _state: &PublicState, _dl: &DisplayList) {}
        fn scroll_updated(&mut self, _state: &PublicState) {}
        fn set_loading(&mut self, _loading: bool) {}
        fn open_external(&mut self, url: &str) {
            self.opened.lock().unwrap().push(url.to_string());
        }
        fn notify(&mut self, message: &str) {
            self.messages.lock().unwrap().push(message.to_string());
        }
    }

    #[test]
    fn transformed_links_hit_test_in_pre_transform_space() {
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut b = Browser::new(
            Box::new(CaptureHost {
                opened: opened.clone(),
                messages: messages.clone(),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            "<html><body><div style='transform-origin:0 0; transform:rotate(90deg)'><a id='rotated' href='mailto:rotated@example.com'>go</a></div></body></html>",
            "https://example.com/",
        );

        let link = b.links().first().expect("rotated link").clone();
        let transform = link.transform.expect("rotated link matrix");
        let physical = transform.apply(link.x + link.w / 2.0, link.y + link.h / 2.0);
        assert!(link.contains(physical.0, physical.1));
        assert!(!link.contains(link.x + link.w / 2.0, link.y + link.h / 2.0));
        b.click(link.x + link.w / 2.0, link.y + link.h / 2.0);
        assert!(opened.lock().unwrap().is_empty());
        b.click(physical.0, physical.1);
        assert_eq!(
            opened.lock().unwrap().as_slice(),
            ["mailto:rotated@example.com"]
        );

        b.pick_inspect(physical.0, physical.1);
        assert!(messages
            .lock()
            .unwrap()
            .iter()
            .any(|message| message.starts_with("inspect\t")));
    }

    #[test]
    fn is_download_response_matrix() {
        let resp = |ct: &str, cd: &str, body: &[u8]| kitae_net::Response {
            final_url: "https://ex.test/f".into(),
            status: 200,
            body: body.to_vec(),
            content_type: ct.into(),
            cache_control: String::new(),
            etag: String::new(),
            last_modified: String::new(),
            protocol: "HTTP/1.1".into(),
            content_disposition: cd.into(),
            www_authenticate: String::new(),
        };
        // An explicit attachment wins even for types the engine renders.
        assert!(is_download_response(&resp(
            "text/html",
            "attachment; filename=\"a.html\"",
            b"<p>x</p>"
        )));
        // The renderable set stays renderable.
        assert!(!is_download_response(&resp("text/html", "", b"<p>x</p>")));
        assert!(!is_download_response(&resp(
            "text/plain; charset=utf-8",
            "",
            b"hi"
        )));
        assert!(!is_download_response(&resp("image/png", "", b"\x89PNG")));
        assert!(!is_download_response(&resp("application/json", "", b"{}")));
        assert!(!is_download_response(&resp("text/csv", "", b"a,b")));
        // Non-renderable types download — PDF has no engine renderer.
        assert!(is_download_response(&resp("application/pdf", "", b"%PDF")));
        assert!(is_download_response(&resp(
            "application/zip",
            "",
            b"PK\x03\x04"
        )));
        // Empty content type: UTF-8 sniff.
        assert!(!is_download_response(&resp("", "", b"just text")));
        assert!(is_download_response(&resp(
            "",
            "",
            &[0xff, 0xfe, 0x00, 0x01]
        )));
    }

    #[test]
    fn binary_navigations_become_downloads_with_notifications() {
        let dir = std::env::temp_dir().join(format!("kitae_core_dl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        kitae_net::set_download_dir(out.clone());
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let src = dir.join("payload.bin");
        std::fs::write(&src, &payload).unwrap();

        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut b = Browser::new(
            Box::new(NotifyHost {
                messages: messages.clone(),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        let url = format!("file://{}", src.display());
        b.navigate(&url);
        // The tab shows the download info page, not binary mojibake (the
        // document title stays the URL, like the plain-text view).
        assert!(b
            .page
            .as_ref()
            .unwrap()
            .html
            .contains("Downloading payload.bin"));

        // Progress reaches the host through poll_downloads, ending in done.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut done_msg = None;
        while std::time::Instant::now() < deadline {
            b.poll_downloads();
            let found = messages
                .lock()
                .unwrap()
                .iter()
                .find(|m| m.contains("\tdone\t"))
                .cloned();
            if let Some(m) = found {
                done_msg = Some(m);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let msg = done_msg.expect("a done notification must arrive");
        let fields: Vec<&str> = msg.split('\t').collect();
        assert_eq!(fields[0], "download");
        assert_eq!(fields[2], "done");
        assert_eq!(fields[3], "200000", "received == body length");
        assert_eq!(fields[4], "200000", "total == body length");
        assert_eq!(fields[5], "payload.bin");
        // The file landed in the download directory with identical bytes.
        assert_eq!(std::fs::read(out.join("payload.bin")).unwrap(), payload);
        // Every notification uses the download vocabulary.
        assert!(messages
            .lock()
            .unwrap()
            .iter()
            .all(|m| m.starts_with("download\t")));
        let mut home = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        home.push("Downloads");
        kitae_net::set_download_dir(home);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── H2: file uploads ───────────────────────────────────────────────────

    /// A clicked `input[type=file]` asks the host to run its open panel via
    /// `filepick\t{id-or-name}\t{multiple}` — but only when the control is
    /// addressable (an id or a name); an anonymous control is skipped,
    /// because the host would have nothing to answer with.
    #[test]
    fn file_input_click_asks_the_host_to_pick() {
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut b = Browser::new(
            Box::new(NotifyHost {
                messages: messages.clone(),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            "<html><body><form id='f'>\
             <input type='file' id='up'>\
             <input type='file' name='byname' multiple>\
             <input type='file'>\
             </form></body></html>",
            "https://ex.test/",
        );
        let id_node = b
            .page
            .as_ref()
            .unwrap()
            .dom
            .get_element_by_id("up")
            .unwrap();
        let name_node = b.first_element_by_name("byname").unwrap();
        let unnamed = {
            let page = b.page.as_ref().unwrap();
            let root = page.dom.root().unwrap();
            let mut found = None;
            page.dom.walk(root, &mut |n| {
                let is_file = page
                    .dom
                    .element(n)
                    .map(|e| e.tag.as_str() == "input" && e.attr("type").unwrap_or("") == "file")
                    .unwrap_or(false);
                if is_file && n != id_node && n != name_node {
                    found = Some(n);
                }
            });
            found.expect("third file input")
        };
        let box_for = |b: &Browser, node: NodeId| -> kitae_layout::Rect {
            let dl = b.last_display_list().expect("display list");

            dl.rects
                .iter()
                .find(|r| r.node == node.0)
                .cloned()
                .expect("file input box")
        };
        for node in [id_node, name_node, unnamed] {
            let r = box_for(&b, node);
            b.click(r.x + r.w / 2.0, r.y + r.h / 2.0);
        }
        let msgs = messages.lock().unwrap().clone();
        assert!(
            msgs.contains(&"filepick\tup\t0".to_string()),
            "id-keyed pick, single"
        );
        assert!(
            msgs.contains(&"filepick\tbyname\t1".to_string()),
            "name fallback, multiple"
        );
        assert_eq!(
            msgs.iter().filter(|m| m.starts_with("filepick\t")).count(),
            2,
            "the anonymous control cannot be answered and never asks"
        );
    }

    /// set_file_files resolves by id or name, refuses non-file controls,
    /// mirrors the display string into the value attribute the layout
    /// paints, and clears on an empty pick list.
    #[test]
    fn set_file_files_mirrors_display_and_resolves_by_name() {
        let mut b = test_browser(
            "<html><body>\
             <input type='file' id='up'>\
             <input type='file' name='byname'>\
             <input id='txt' name='txt'>\
             </body></html>",
        );
        assert!(
            !b.set_file_files("txt", &["/tmp/a.png".to_string()]),
            "text inputs are not file targets"
        );
        assert!(!b.set_file_files("missing", &["/tmp/a.png".to_string()]));
        assert!(b.set_file_files("up", &["/home/u/roster.csv".to_string()]));
        assert_eq!(
            b.input_value("up").as_deref(),
            Some("roster.csv"),
            "a single pick shows the basename"
        );
        assert!(
            b.set_file_files(
                "byname",
                &["/tmp/a.png".to_string(), "/tmp/b.png".to_string()]
            ),
            "name-only controls resolve"
        );
        let node = b.first_element_by_name("byname").unwrap();
        let shown = b
            .page
            .as_ref()
            .unwrap()
            .dom
            .element(node)
            .and_then(|e| e.attr("value"))
            .unwrap_or("")
            .to_string();
        assert_eq!(shown, "2 files", "multi picks show the count");
        assert!(b.set_file_files("up", &[]));
        assert_eq!(
            b.input_value("up").as_deref(),
            Some(""),
            "an empty list clears the pick"
        );
    }

    /// multipart_body mirrors the urlencoded field rules (disabled/unnamed
    /// skipped, checked-only booleans, textarea/select values) and adds one
    /// part per picked file with its basename, sniffed type and raw bytes.
    #[test]
    fn multipart_body_encodes_fields_and_picked_files() {
        let dir = std::env::temp_dir().join(format!("kitae_core_up_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let notes = dir.join("notes.txt");
        std::fs::write(&notes, b"hello upload\n").unwrap();
        let blob = dir.join("data.bin");
        std::fs::write(&blob, [7u8; 64]).unwrap();

        let mut b = test_browser(
            "<html><body><form id='f' method='post' enctype='multipart/form-data'>\
             <input id='who' name='who' value='kitae'>\
             <input type='checkbox' name='on' value='yes' checked>\
             <input type='checkbox' name='off' value='no'>\
             <textarea name='msg'>hi there</textarea>\
             <select name='pick'><option value='a'>a</option><option value='b' selected>b</option></select>\
             <input type='file' id='up' name='up' multiple>\
             </form></body></html>",
        );
        assert!(b.set_file_files(
            "up",
            &[notes.display().to_string(), blob.display().to_string()]
        ));
        let (ctype, body) = b.multipart_body("f").expect("multipart body");
        assert!(
            ctype.starts_with("multipart/form-data; boundary=kitae-"),
            "the header names the delimiter: {ctype}"
        );
        let boundary = ctype.rsplit('=').next().unwrap().to_string();
        let has = |needle: &[u8]| body.windows(needle.len()).any(|w| w == needle);
        let text = String::from_utf8_lossy(&body);
        assert!(
            has(format!("--{boundary}\r\n").as_bytes()),
            "parts open with the delimiter"
        );
        assert!(text.contains("Content-Disposition: form-data; name=\"who\"\r\n\r\nkitae\r\n"));
        assert!(
            text.contains("name=\"on\"\r\n\r\nyes\r\n"),
            "a checked checkbox keeps its value"
        );
        assert!(
            !text.contains("name=\"off\""),
            "unchecked checkboxes stay out"
        );
        assert!(text.contains("name=\"msg\"\r\n\r\nhi there\r\n"));
        assert!(
            text.contains("name=\"pick\"\r\n\r\nb\r\n"),
            "the selected option wins"
        );
        assert!(
            text.contains(
                "Content-Disposition: form-data; name=\"up\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\n"
            ),
            "file parts carry the basename and sniffed type"
        );
        assert!(has(b"hello upload\n"), "file bytes ride along verbatim");
        assert!(
            text.contains(
                "name=\"up\"; filename=\"data.bin\"\r\nContent-Type: application/octet-stream\r\n"
            ),
            "unknown extensions fall back to octet-stream"
        );
        assert!(
            text.ends_with(&format!("--{boundary}--\r\n")),
            "the body terminates with the closing delimiter"
        );

        // An empty pick list still sends the field, as an empty file part.
        assert!(b.set_file_files("up", &[]));
        let (_, body) = b.multipart_body("f").expect("multipart body after clear");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"up\"; filename=\"\"\r\n\r\n\r\n"));
        assert!(
            !body.windows(13).any(|w| w == b"hello upload\n"),
            "cleared bytes are gone"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The full submit path: a `multipart/form-data` POST navigates to the
    /// action and renders the response. The file:// target keeps the test
    /// network-free (the body's bytes are verified by the test above).
    #[test]
    fn multipart_submit_navigates_to_the_action() {
        let dir = std::env::temp_dir().join(format!("kitae_core_up2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("target.html"),
            "<html><head><title>Uploaded</title></head><body><h1>got it</h1></body></html>",
        )
        .unwrap();
        let mut b = test_browser_at(
            "<html><body>\
             <form id='f' method='post' enctype='multipart/form-data' action='target.html'>\
             <input type='file' id='up' name='up'>\
             <input name='note' value='hi'>\
             </form></body></html>",
            &format!("file://{}/page.html", dir.display()),
        );
        let upload = dir.join("up.txt");
        std::fs::write(&upload, b"body\n").unwrap();
        assert!(b.set_file_files("up", &[upload.display().to_string()]));
        let landed = b.submit_form("f").expect("submit navigates");
        assert!(
            landed.ends_with("target.html"),
            "the action URL is the landing: {landed}"
        );
        assert!(
            b.page.as_ref().unwrap().html.contains("got it"),
            "the response rendered"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── H6: 401 sign-in prompt, autofill, password save ───────────────────

    fn auth_response(status: u16, www_authenticate: &str) -> kitae_net::Response {
        kitae_net::Response {
            final_url: "https://vault.example/x".into(),
            status,
            body: Vec::new(),
            content_type: "text/html".into(),
            cache_control: String::new(),
            etag: String::new(),
            last_modified: String::new(),
            protocol: "HTTP/1.1".into(),
            content_disposition: String::new(),
            www_authenticate: www_authenticate.into(),
        }
    }

    /// A 401 carrying a Basic/Digest challenge renders a sign-in page (not
    /// the generic error page) and queues the challenge for the host; a
    /// scheme the engine does not speak stays an error page.
    #[test]
    fn forty_one_challenge_renders_a_sign_in_page_and_queues_the_prompt() {
        take_auth_challenges(); // drain any leftovers from earlier tests
        let ((html, _base), _, outcome, _, _) = render_response_state(
            "https://vault.example/x",
            auth_response(401, "Basic realm=\"Vault\""),
            None,
        );
        assert!(outcome.is_ok(), "a sign-in page is not an error page");
        assert!(html.contains("Sign in"));
        assert!(
            html.contains("vault.example"),
            "the challenged host is named: {html}"
        );
        assert!(html.contains("Vault"), "the realm is named");
        let prompts = take_auth_challenges();
        assert_eq!(prompts.len(), 1, "exactly one challenge queued");
        assert_eq!(prompts[0].url, "https://vault.example/x");
        assert_eq!(prompts[0].host, "vault.example");
        assert_eq!(prompts[0].realm, "Vault");
        assert_eq!(prompts[0].scheme, "basic");
        // Digest parses too; NTLM/Negotiate never become sign-in pages.
        let (digest, ..) = render_response_state(
            "https://vault.example/y",
            auth_response(401, "Digest realm=\"D\", nonce=\"n\", qop=\"auth\""),
            None,
        );
        assert!(digest.0.contains("Digest authentication"));
        assert_eq!(
            take_auth_challenges().len(),
            1,
            "the Digest challenge queued too"
        );
        let (ntlm, _, ntlm_outcome, ..) = render_response_state(
            "https://vault.example/z",
            auth_response(401, "Negotiate"),
            None,
        );
        assert!(ntlm_outcome.is_err(), "unspoken schemes stay error pages");
        assert!(!ntlm.0.contains("Sign in"));
        assert!(
            take_auth_challenges().is_empty(),
            "Negotiate queues nothing"
        );
    }

    /// `set_auth` stores the session credentials for the pending challenge's
    /// (host, realm) — the session provider then feeds kitae_net's automatic
    /// 401 retry — and re-fetches the challenged URL. No pending challenge:
    /// nothing happens.
    #[test]
    fn set_auth_stores_session_credentials_and_reloads() {
        kitae_net::clear_session_credentials();
        // Fixture secrets, assembled — never literals in source.
        let user = ["ki", "tae"].concat();
        let pass = ["v4", "ult", "9"].concat();
        let mut b = test_browser("nothing");
        b.pending_auth = Some(AuthPrompt {
            url: "about:start".into(),
            host: "vault.example".into(),
            realm: "Vault".into(),
            scheme: "basic".into(),
        });
        assert!(b.set_auth(&user, &pass), "a pending challenge is answered");
        assert_eq!(b.page_url, "about:start", "the challenged URL re-fetched");
        let got =
            kitae_net::session_credentials("vault.example", "Vault").expect("credentials stored");
        assert_eq!(got.username, user);
        assert_eq!(got.password, pass);
        assert!(
            kitae_net::session_credentials("vault.example", "Other").is_none(),
            "the realm scopes the store"
        );
        assert!(!b.set_auth("x", "y"), "no challenge: nothing to answer");
        kitae_net::clear_session_credentials();
    }

    /// A page with a login form (password input + preceding text/email
    /// input, both id'd) notifies `loginform\t{form}\t{user}\t{pass}` so
    /// the host can fill from its store; password-only or id-less forms
    /// stay silent.
    #[test]
    fn login_form_pages_notify_the_host() {
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut b = Browser::new(
            Box::new(NotifyHost {
                messages: messages.clone(),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            "<html><body>\
             <form id='login'><input id='user' type='text'>\
             <input id='pw' type='password'><input type='submit'></form>\
             <form id='nokey'><input id='nouser' type='text'>\
             <input type='password'></form>\
             <form id='passonly'><input id='p2' type='password'></form>\
             <form id='plain'><input id='q' type='text'></form>\
             </body></html>",
            "https://example.com/",
        );
        // load_html bypasses the load pipeline (no load_with_password), so
        // the real navigate path's autofill offer is invoked directly here.
        b.notify_login_form();
        let msgs = messages.lock().unwrap();
        assert!(
            msgs.contains(&"loginform\tlogin\tuser\tpw".to_string()),
            "the login form is offered: {msgs:?}"
        );
        assert_eq!(
            msgs.iter().filter(|m| m.starts_with("loginform\t")).count(),
            1,
            "no user field / no user field id / no password → silent"
        );
    }

    /// Submitting a form with a filled password offers the host a save —
    /// `pwsave\t{origin}\t{user}\t{pass}` — with the nearest preceding
    /// filled text input as the user name. Empty passwords stay silent.
    /// The file:// origin keys on the scheme alone.
    #[test]
    fn submitting_a_password_form_offers_to_save_it() {
        let dir = std::env::temp_dir().join(format!("kitae_core_pw_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("target.html"),
            "<html><body><h1>welcome</h1></body></html>",
        )
        .unwrap();
        // Fixture secrets, assembled — never literals in source.
        let user = ["ka", "ren"].concat();
        let pass = ["s3", "cr", "e", "t"].concat();
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut b = Browser::new(
            Box::new(NotifyHost {
                messages: messages.clone(),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            &format!(
                "<html><body>\
                 <form id='f' method='post' action='target.html'>\
                 <input id='user' name='user' type='text' value='{user}'>\
                 <input id='pw' name='pw' type='password' value='{pass}'>\
                 <input type='submit'></form>\
                 <form id='empty'><input id='e1' type='text' value='{user}'>\
                 <input id='e2' type='password' value=''></form>\
                 </body></html>"
            ),
            &format!("file://{}/page.html", dir.display()),
        );
        b.submit_form("f").expect("submit navigates");
        let msgs = messages.lock().unwrap();
        assert!(
            msgs.contains(&format!("pwsave\tfile://\t{user}\t{pass}")),
            "the filled pair is offered: {msgs:?}"
        );
        assert_eq!(
            msgs.iter().filter(|m| m.starts_with("pwsave\t")).count(),
            1,
            "an empty password never offers a save"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// H5: an inspector pick resolves the element under the point — a text
    /// band hit reports the run's element, a painted-box hit reports the
    /// box's element — and a point over nothing stays silent.
    #[test]
    fn inspector_pick_names_the_element_under_the_point() {
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut b = Browser::new(
            Box::new(NotifyHost {
                messages: messages.clone(),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            "<html><body>\
             <div id='top' style='background:#ff0000;height:100px'>alpha</div>\
             <div id='bottom' style='background:#00ff00;height:100px'>beta</div>\
             </body></html>",
            "file:///pick.html",
        );
        let dom = &b.page.as_ref().expect("page laid out").dom;
        let top = dom.get_element_by_id("top").expect("top");
        let bottom = dom.get_element_by_id("bottom").expect("bottom");
        b.pick_inspect(350.0, 20.0); // over the first box (its text band or paint)
        b.pick_inspect(350.0, 180.0); // below the text, on the second box's paint
        b.pick_inspect(10.0, 550.0); // under both boxes — nothing to report
        let msgs = messages.lock().unwrap();
        assert!(
            msgs.contains(&format!("inspect\t{}", top.0)),
            "the first box's pick reports its node: {msgs:?}"
        );
        assert!(
            msgs.contains(&format!("inspect\t{}", bottom.0)),
            "the second box's pick reports its node: {msgs:?}"
        );
        assert_eq!(
            msgs.iter().filter(|m| m.starts_with("inspect\t")).count(),
            2,
            "a point over nothing stays silent: {msgs:?}"
        );
    }

    /// H5: the report names the element (tag#id.classes), lists its
    /// attributes, and carries the computed properties the cascade produced;
    /// command 39 narrates the same text to the host.
    #[test]
    fn inspect_info_reports_tag_attributes_and_computed_style() {
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut b = Browser::new(
            Box::new(NotifyHost {
                messages: messages.clone(),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            "<html><body>\
             <div id='top' class='card wide' \
             style='background:#ff0000;height:100px'>alpha</div>\
             </body></html>",
            "file:///info.html",
        );
        let top = b
            .page
            .as_ref()
            .expect("page laid out")
            .dom
            .get_element_by_id("top")
            .expect("top");
        let info = b.inspect_info(top.0);
        assert!(
            info.starts_with("div#top.card.wide"),
            "the header names the element: {info:?}"
        );
        assert!(
            info.contains("class=\"card wide\""),
            "attributes are listed: {info:?}"
        );
        assert!(
            info.contains("display: block"),
            "computed display: {info:?}"
        );
        assert!(
            info.contains("background: rgb(255, 0, 0)"),
            "computed background: {info:?}"
        );
        // Command 39's answer rides the notification stream.
        b.report_inspect_info(top.0);
        let msgs = messages.lock().unwrap();
        assert!(
            msgs.iter().any(|m| m.starts_with("inspectinfo\tdiv#top")),
            "the report is narrated to the host: {msgs:?}"
        );
    }

    #[test]
    fn ddg_results_decode_to_their_real_target() {
        let wrapped =
            "https://html.duckduckgo.com/html/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1";
        assert_eq!(
            ddg_result_target(wrapped).unwrap(),
            "https://example.com/a?b=1"
        );
        assert_eq!(ddg_result_target("https://example.com/"), None);
    }

    #[test]
    fn titles_collapse_whitespace() {
        let (dom, root) = parse("<html><head><title>  Hello\n  World </title></head></html>");
        assert_eq!(page_title(&dom, root.unwrap()).unwrap(), "Hello World");
    }

    #[test]
    fn style_blocks_and_links_are_collected_in_order() {
        let html = "<html><head>\
<link rel='stylesheet' href='/a.css'>\
<style>p { color: #111111 }</style>\
<link rel='preload' href='/x.css'>\
<link rel='stylesheet' href='https://cdn.example.com/b.css'>\
</head><body><p>x</p></body></html>";
        let (dom, root) = parse(html);
        let root = root.unwrap();
        let sources = collect_styles(&dom, root, "https://ex.test/docs/");
        assert_eq!(
            sources,
            vec![
                StyleSource::External("https://ex.test/a.css".into()),
                StyleSource::Inline("p { color: #111111 }".into()),
                StyleSource::External("https://cdn.example.com/b.css".into()),
            ],
            "document order, rel=stylesheet only, hrefs resolved against the base"
        );
    }

    #[test]
    fn first_active_base_href_resolves_stylesheet_and_import_urls() {
        let dir = std::env::temp_dir().join(format!("kitae_base_css_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let assets = dir.join("assets");
        std::fs::create_dir_all(&assets).expect("mkdir");
        std::fs::write(assets.join("site.css"), "p { color: #010101 }").expect("site");
        std::fs::write(assets.join("imported.css"), "p { color: #abcdef }").expect("import");
        let page = format!("file://{}/page.html", dir.display());
        let html = "<html><head>\
                    <template><base href='template/'></template>\
                    <base href='assets/'><base href='wrong/'>\
                    <link rel='stylesheet' href='site.css'>\
                    <style>@import 'imported.css';</style>\
                    </head><body><p id='target'>x</p></body></html>";
        let (dom, root, styles) = prepare_in_env(html, &page, 640.0, &EnvironmentState::default());
        let root = root.unwrap();
        assert_eq!(
            collect_styles(&dom, root, &page),
            vec![
                StyleSource::External(format!(
                    "{}/assets/site.css",
                    page.trim_end_matches("/page.html")
                )),
                StyleSource::Inline("@import 'imported.css';".into()),
            ]
        );
        let target = dom.get_element_by_id("target").unwrap();
        let color = styles
            .iter()
            .find(|(node, _)| *node == target)
            .unwrap()
            .1
            .color;
        assert_eq!(color, 0xABCDEFFF);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn base_href_resolves_image_and_link_navigation_urls() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let dir = std::env::temp_dir().join(format!("kitae_base_res_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let assets = dir.join("assets");
        std::fs::create_dir_all(&assets).expect("mkdir");
        std::fs::write(
            assets.join("next.html"),
            "<html><body><h1>arrived</h1></body></html>",
        )
        .expect("target");
        std::fs::write(
            assets.join("pic.svg"),
            "<svg xmlns='http://www.w3.org/2000/svg' width='2' height='2'>\
             <rect width='2' height='2' fill='#123456'/></svg>",
        )
        .expect("image");
        let page = format!("file://{}/page.html", dir.display());
        let mut b = test_browser_at(
            "<html><head><base href='assets/'><base href='wrong/'></head>\
             <body><a id='go' href='next.html'>go</a>\
             <img id='pic' src='pic.svg' alt='pic'></body></html>",
            &page,
        );
        assert_eq!(
            b.state().resource_base_url,
            format!("file://{}/assets/", dir.display())
        );
        assert!(
            b.last_display_list().unwrap().images[0].payload.is_some(),
            "relative image resolved and decoded through the document base"
        );
        let link = b.links().first().expect("link area").clone();
        b.click(link.x + link.w / 2.0, link.y + link.h / 2.0);
        assert_eq!(
            b.state().url,
            format!("file://{}/assets/next.html", dir.display())
        );
        clear_qtsq_rasters();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn base_href_resolves_form_action_navigation() {
        let dir = std::env::temp_dir().join(format!("kitae_base_form_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let assets = dir.join("assets");
        std::fs::create_dir_all(&assets).expect("mkdir");
        std::fs::write(
            assets.join("post.html"),
            "<html><body><h1>posted</h1></body></html>",
        )
        .expect("target");
        let page = format!("file://{}/page.html", dir.display());
        let mut b = test_browser_at(
            "<html><head><base href='assets/'></head>\
             <body><form id='f' action='post.html'><input name='q' value='x'></form>\
             </body></html>",
            &page,
        );
        let landed = b.submit_form("f").expect("submit");
        assert_eq!(
            landed,
            format!("file://{}/assets/post.html?q=x", dir.display())
        );
        assert_eq!(b.state().url, landed);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn style_and_link_media_attributes_gate_by_viewport() {
        fn color(browser: &Browser) -> u32 {
            let page = browser.page.as_ref().unwrap();
            let target = page.dom.get_element_by_id("target").unwrap();
            page.styles
                .iter()
                .find(|(node, _)| *node == target)
                .unwrap()
                .1
                .color
        }

        let dir = std::env::temp_dir().join(format!("kitae_media_attr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("print.css"), "p { color: #010101 }").expect("print");
        std::fs::write(dir.join("wide.css"), "p { color: #778899 }").expect("wide");
        let page = format!("file://{}/page.html", dir.display());
        let mut b = test_browser_at(
            "<html><head>\
             <style media='(max-width: 499px)'>p { color: #112233 }</style>\
             <style media='(min-width: 500px)'>p { color: #445566 }</style>\
             <link rel='stylesheet' href='wide.css' media='screen and (min-width: 500px)'>\
             <link rel='stylesheet' href='print.css' media='print'>\
             </head><body><p id='target'>x</p></body></html>",
            &page,
        );
        assert_eq!(color(&b), 0x112233FF);
        b.resize(600.0, 600.0);
        assert_eq!(color(&b), 0x778899FF);
        std::fs::remove_dir_all(dir).ok();
    }

    /// H3: the media-type switch is what a print run is built on — print
    /// blocks apply on paper, screen blocks (and print-gated sheets) do not,
    /// and the frame says which environment it carries.
    #[test]
    fn print_media_switch_restyles_the_page_in_place() {
        fn color(browser: &Browser) -> u32 {
            let page = browser.page.as_ref().unwrap();
            let target = page.dom.get_element_by_id("target").unwrap();
            page.styles
                .iter()
                .find(|(node, _)| *node == target)
                .unwrap()
                .1
                .color
        }
        let mut b = test_browser(
            "<html><head><style>\
             @media screen { #target { color: #020202 } }\
             @media print { #target { color: #010101 } }\
             @media not print { #target { letter-spacing: 5px } }\
             </style></head><body><p id='target'>x</p></body></html>",
        );
        assert_eq!(color(&b), 0x020202FF, "screen block on screen");
        let base = b.repaint_count();

        assert!(b.set_media_type(MediaType::Print), "change republishes");
        assert_eq!(b.repaint_count(), base + 1, "one repaint, no reload");
        assert_eq!(color(&b), 0x010101FF, "print block applies on paper");
        assert_eq!(find_style(&b, "p").letter_spacing, 0.0, "`not print` drops");
        assert!(b.state().print_media, "state advertises the print frame");

        // Setting the same environment again is a no-op (no repaint storm).
        assert!(!b.set_media_type(MediaType::Print));
        assert_eq!(b.repaint_count(), base + 1);

        assert!(b.set_media_type(MediaType::Screen), "back to screen");
        assert_eq!(color(&b), 0x020202FF);
        assert_eq!(find_style(&b, "p").letter_spacing, 5.0);
        assert!(!b.state().print_media);
    }

    /// The same switch also re-collects `<style media>` / `<link media>`
    /// sheets, and survives navigation like the rest of the environment.
    #[test]
    fn print_media_gates_media_attributed_sheets_and_survives_navigation() {
        fn color(browser: &Browser) -> u32 {
            let page = browser.page.as_ref().unwrap();
            let target = page.dom.get_element_by_id("target").unwrap();
            page.styles
                .iter()
                .find(|(node, _)| *node == target)
                .unwrap()
                .1
                .color
        }
        let dir = std::env::temp_dir().join(format!("kitae_print_media_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("print.css"), "p { color: #0a0a0a }").expect("print");
        std::fs::write(dir.join("screen.css"), "p { color: #0b0b0b }").expect("screen");
        let page = format!("file://{}/page.html", dir.display());
        let mut b = test_browser_at(
            "<html><head>\
             <link rel='stylesheet' href='screen.css' media='screen'>\
             <link rel='stylesheet' href='print.css' media='print'>\
             </head><body><p id='target'>x</p></body></html>",
            &page,
        );
        assert_eq!(color(&b), 0x0B0B0BFF, "only the screen sheet loads");
        assert!(b.set_media_type(MediaType::Print));
        assert_eq!(color(&b), 0x0A0A0AFF, "only the print sheet loads");
        b.load_html(
            "<style media='screen'>p { color: #0c0c0c }</style>\
             <style media='print'>p { color: #0d0d0d }</style><p id='target'>x</p>",
            &page,
        );
        assert_eq!(
            color(&b),
            0x0D0D0DFF,
            "a page loaded while print media is active lays out for paper"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// The wire's SET_ENVIRONMENT carries only the `prefers-*` half, so an
    /// appearance change mid-print must not knock the page back to screen.
    #[test]
    fn preference_updates_leave_the_media_type_alone() {
        let mut b = test_browser(
            "<html><head><style>@media print { #t { color: #010101 } }</style>\
             </head><body><p id='t'>x</p></body></html>",
        );
        assert!(b.set_media_type(MediaType::Print));
        let dark = EnvironmentState {
            color_scheme: ColorScheme::Dark,
            ..EnvironmentState::default()
        };
        assert!(b.set_preferences(&dark), "the scheme still moves");
        assert_eq!(
            b.environment.media_type,
            MediaType::Print,
            "print survives a preference update"
        );
        // A repeated update is a no-op; a full environment set does move the
        // media type, since that is what it carries.
        assert!(!b.set_preferences(&dark));
        assert!(b.set_environment(EnvironmentState::default()));
        assert_eq!(b.environment.media_type, MediaType::Screen);
    }

    /// A3/env: the page's environment reaches layout, so the images a frame
    /// publishes follow the same media switch its stylesheets do — the print
    /// run picks the paper sources, and the `prefers-*` half of the
    /// environment moves them too.
    #[test]
    fn print_media_switch_also_moves_picture_and_img_sources() {
        fn srcs(b: &Browser) -> Vec<String> {
            b.last_dl
                .as_ref()
                .expect("frame published")
                .images
                .iter()
                .map(|i| i.src.clone())
                .collect()
        }
        let mut b = test_browser(
            "<html><body>\
             <picture>\
             <source media='print' srcset='paper.png'>\
             <source media='screen' srcset='glass.png'>\
             <img src='default.png'>\
             </picture>\
             <img media='print' src='onpaper.png'>\
             </body></html>",
        );
        assert_eq!(
            srcs(&b),
            ["glass.png", ""],
            "on screen: the screen source, and no print-gated img"
        );

        assert!(b.set_media_type(MediaType::Print));
        assert_eq!(
            srcs(&b),
            ["paper.png", "onpaper.png"],
            "on paper: the print source and the print-gated img"
        );

        assert!(b.set_media_type(MediaType::Screen));
        assert_eq!(srcs(&b), ["glass.png", ""], "and back again");

        // The `prefers-*` half rides in the same way: a dark page picks the
        // dark source without touching the media type.
        let mut d = test_browser(
            "<html><body><picture>\
             <source media='(prefers-color-scheme: dark)' srcset='night.png'>\
             <img src='day.png'>\
             </picture></body></html>",
        );
        assert_eq!(srcs(&d), ["day.png"]);
        assert!(d.set_preferences(&EnvironmentState {
            color_scheme: ColorScheme::Dark,
            ..EnvironmentState::default()
        }));
        assert_eq!(srcs(&d), ["night.png"]);
    }

    /// A3/env: an environment change must not be served from the fragment
    /// cache — the cached paint of a body-level block was laid out for the
    /// other environment, and would keep the old source.
    #[test]
    fn print_media_switch_repaints_picture_sources_through_the_fragment_cache() {
        let mut b = test_browser(
            "<html><body><div><picture>\
             <source media='print' srcset='paper.png'>\
             <img src='glass.png'>\
             </picture></div></body></html>",
        );
        // The load's own relayout already populated the cache.
        assert_eq!(b.page.as_ref().expect("page").frag_cache.len(), 1);
        assert_eq!(
            b.last_dl
                .as_ref()
                .and_then(|dl| dl.images.first())
                .map(|i| i.src.clone())
                .as_deref(),
            Some("glass.png")
        );

        assert!(b.set_media_type(MediaType::Print));
        let src = b
            .last_dl
            .as_ref()
            .and_then(|dl| dl.images.first())
            .map(|i| i.src.clone());
        assert_eq!(
            src.as_deref(),
            Some("paper.png"),
            "the stale fragment was dropped, not re-merged"
        );
    }

    /// C2: `aspect-ratio` reaches a replaced box through the same style
    /// value the block path already read, and rides the frame like any other
    /// image geometry — no wire change.
    #[test]
    fn aspect_ratio_sizes_a_published_image() {
        let b = test_browser(
            "<html><body><img style='width:160px;aspect-ratio:2' src='a.png' alt='x'>\
             </body></html>",
        );
        let im = &b.last_dl.as_ref().expect("frame published").images[0];
        assert_eq!((im.w, im.h), (160.0, 80.0));
    }

    #[test]
    fn stylesheet_policy_bounds_external_count_and_bytes() {
        let dir = std::env::temp_dir().join(format!("kitae_sheet_limits_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let mut sources = Vec::new();
        for index in 0..3 {
            let name = format!("count-{index}.css");
            std::fs::write(dir.join(&name), format!("p{{color:#{index:06x}}}")).expect("count");
            sources.push(StyleSource::External(format!(
                "file://{}/{name}",
                dir.display()
            )));
        }
        let (count_sheets, count_stats) = load_stylesheets_in_env(
            &sources,
            "",
            "",
            1280.0,
            0.0,
            &EnvironmentState::default(),
            2,
            1024,
        );
        assert_eq!(count_sheets.len(), 2);
        assert_eq!(count_stats.external_candidates, 2);
        assert_eq!(count_stats.external_loaded, 2);
        assert_eq!(count_stats.skipped_count, 1);

        let bodies = ["a{x:1}", "b{x:12345}", "c{x:2}"];
        let mut byte_sources = Vec::new();
        for (index, body) in bodies.iter().enumerate() {
            let name = format!("bytes-{index}.css");
            std::fs::write(dir.join(&name), body).expect("bytes");
            byte_sources.push(StyleSource::External(format!(
                "file://{}/{name}",
                dir.display()
            )));
        }
        let byte_budget = bodies[0].len() + bodies[2].len();
        let (byte_sheets, byte_stats) = load_stylesheets_in_env(
            &byte_sources,
            "",
            "",
            1280.0,
            0.0,
            &EnvironmentState::default(),
            10,
            byte_budget,
        );
        assert_eq!(byte_sheets, vec![bodies[0], bodies[2]]);
        assert_eq!(byte_stats.external_loaded, 2);
        assert_eq!(byte_stats.external_bytes, byte_budget as u64);
        assert_eq!(byte_stats.skipped_count, 1);
        assert_eq!(byte_stats.skipped_bytes, bodies[1].len() as u64);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn stylesheet_count_limit_is_raised_and_observable() {
        let dir = std::env::temp_dir().join(format!("kitae_sheet_report_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let sheet = "p { color: #010203 }";
        let mut html = String::from("<html><head>");
        for index in 0..=MAX_EXTERNAL_STYLESHEETS {
            let name = format!("sheet-{index}.css");
            std::fs::write(dir.join(&name), sheet).expect("sheet");
            html.push_str(&format!("<link rel='stylesheet' href='{name}'>"));
        }
        html.push_str("</head><body><p>x</p></body></html>");
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut b = Browser::new(
            Box::new(NotifyHost {
                messages: messages.clone(),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(&html, &format!("file://{}/page.html", dir.display()));
        let stats = b.stylesheet_load_stats().expect("page stats");
        assert_eq!(stats.external_loaded as usize, MAX_EXTERNAL_STYLESHEETS);
        assert_eq!(stats.skipped_count, 1);
        let notes = messages.lock().unwrap();
        assert_eq!(notes.len(), 1, "one cap report for one load: {notes:?}");
        assert_eq!(
            notes[0],
            format!(
                "stylesheet-limit\t{}\t{}\t1\t0",
                MAX_EXTERNAL_STYLESHEETS,
                MAX_EXTERNAL_STYLESHEETS * sheet.len()
            )
        );
        drop(notes);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_stylesheets_skips_failures_and_duplicates() {
        let sources = vec![
            StyleSource::Inline("p { color: #111111 }".into()),
            StyleSource::External("about:definitely-not-a-real-scheme".into()),
            StyleSource::External("about:definitely-not-a-real-scheme".into()),
        ];
        let sheets = load_stylesheets(&sources, "");
        assert_eq!(
            sheets,
            vec!["p { color: #111111 }"],
            "fetch failure ignored once and twice"
        );
    }

    /// I6: a throwaway loopback HTTP server for fetch-policy tests. Serves
    /// each requested path from `routes` (exact match, `text/css`), then
    /// closes the connection. Returns the `http://127.0.0.1:port` base.
    fn loopback_css_server(routes: Vec<(&'static str, &'static str)>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                match routes.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    None => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
            }
        });
        url
    }

    #[test]
    fn mixed_content_stylesheets_are_refused_on_https_pages() {
        // External sheet: refused before any fetch. A real fetch would stall
        // on the reserved `.example` TLD — this test staying instant is part
        // of the point.
        let sources = vec![
            StyleSource::Inline("p { color: #111111 }".into()),
            StyleSource::External("http://insecure.example/s.css".into()),
        ];
        let sheets = load_stylesheets(&sources, "https://secure.example/");
        assert_eq!(
            sheets,
            vec!["p { color: #111111 }"],
            "plaintext sheet refused"
        );

        // `@import` inside an inline block is subject to the same policy.
        let sources = vec![StyleSource::Inline(
            "@import url(http://insecure.example/i.css); p { color: #111111 }".into(),
        )];
        let sheets = load_stylesheets(&sources, "https://secure.example/");
        assert_eq!(sheets.len(), 1, "plaintext import refused");
    }

    #[test]
    fn mixed_content_allows_loopback_stylesheets_on_https_pages() {
        // Loopback hosts are potentially trustworthy (secure-contexts spec):
        // an https page may still fetch `http://127.0.0.1` — this is a real
        // fetch over the loopback listener.
        let server = loopback_css_server(vec![("/s.css", "p { color: #222222 }")]);
        let sources = vec![StyleSource::External(format!("{server}/s.css"))];
        let sheets = load_stylesheets(&sources, "https://secure.example/");
        assert_eq!(
            sheets,
            vec!["p { color: #222222 }"],
            "loopback sheet loads from an https page"
        );
    }

    #[test]
    fn mixed_content_image_is_refused_on_https_page() {
        // The raster store is process-global: hold the test lock and clear it
        // so parallel decode tests can't leak entries into this assertion.
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            "<html><body><p><img src=\"http://insecure.example/pic.png\" alt=\"x\"></p></body></html>",
            "https://secure.example/",
        );
        let dl = b.last_dl.clone().expect("frame published");
        assert_eq!(dl.images.len(), 1);
        assert!(dl.images[0].payload.is_none(), "plaintext image refused");
        assert_eq!(b.raster_len(), 0, "nothing was decoded or cached");
        clear_qtsq_rasters();
    }

    #[test]
    fn mixed_content_font_is_refused_on_https_page() {
        let html = "<html><head><style>\
            @font-face { font-family: 'I6BlockedFace'; src: url('http://insecure.example/f.woff2'); }\
            p { font-family: 'I6BlockedFace' }\
            </style></head><body><p>x</p></body></html>";
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(html, "https://secure.example/");
        assert!(
            !kitae_shape::font::registered_web_fonts()
                .iter()
                .any(|f| f.eq_ignore_ascii_case("I6BlockedFace")),
            "plaintext web font on an https page is refused"
        );
    }

    #[test]
    fn import_expands_recursively_with_cycle_guard() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;
        // Local HTTP servers are heavy; exercise the cycle/depth logic
        // against file:// sheets written into a temp dir.
        let dir = std::env::temp_dir().join(format!("kitae_r4imp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("a.css"),
            "@import \"b.css\";\np { color: #111111 }",
        )
        .expect("a");
        std::fs::write(
            dir.join("b.css"),
            "@import \"a.css\";\ndiv { color: #222222 }",
        )
        .expect("b");
        let a = format!("file://{}/a.css", dir.display());
        let sources = vec![StyleSource::External(a.clone())];
        let sheets = load_stylesheets(&sources, "");
        // a → b → (a again: cycle, skipped). Both sheets land once; order is
        // imported-first: b's rules, then a's own.
        assert_eq!(
            sheets,
            vec!["div { color: #222222 }", "p { color: #111111 }"],
            "import expands, cycle guarded"
        );
        let _ = Arc::new(AtomicUsize::new(0)); // keep imports referenced
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn import_in_style_block_resolves_against_page_base() {
        let dir = std::env::temp_dir().join(format!("kitae_r4inline_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("x.css"), "p { color: #334455 }").expect("x");
        let html = format!(
            "<html><head><style>@import \"x.css\";</style></head><body><p id='t'>x</p></body></html>"
        );
        let (dom, root, styles) =
            prepare_in(&html, &format!("file://{}/page.html", dir.display()), 640.0);
        let root = root.unwrap();
        let p = find_element(&dom, root, "p").unwrap();
        let st = &styles.iter().find(|(n, _)| *n == p).unwrap().1;
        assert_eq!(st.color, 0x334455FF, "imported sheet applies via page base");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prepare_applies_style_block_css() {
        let html = "<html><head><style>p { color: #223344; font-size: 21px } \
div > p { text-decoration: underline }</style></head>\
<body><div><p id='t'>x</p></div></body></html>";
        let (dom, root, styles) = prepare(html);
        let root = root.unwrap();
        let p = find_element(&dom, root, "p").unwrap();
        let st = &styles.iter().find(|(n, _)| *n == p).unwrap().1;
        assert_eq!(st.color, 0x223344FF);
        assert!((st.font_size - 21.0).abs() < 0.01);
        assert!(st.underline, "child combinator rule from <style> applies");
    }

    struct NullHost {
        frames: usize,
    }

    impl Host for NullHost {
        fn publish(&mut self, _state: &PublicState, _dl: &DisplayList) {
            self.frames += 1;
        }
        fn scroll_updated(&mut self, _state: &PublicState) {}
        fn set_loading(&mut self, _loading: bool) {}
        fn open_external(&mut self, _url: &str) {}
    }

    #[test]
    fn insecure_navigation_waits_for_host_confirmation() {
        kitae_net::set_warn_insecure_http(true);
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut b = Browser::new(
            Box::new(NotifyHost {
                messages: messages.clone(),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.navigate("http://example.com/private");
        assert!(b.history.is_empty());
        assert!(b.pending_navigation.is_some());
        assert_eq!(
            messages.lock().unwrap().as_slice(),
            ["insecure\thttp://example.com/private"]
        );
        assert!(b.resolve_insecure_navigation(false));
        assert!(b.pending_navigation.is_none());
        assert!(b.history.is_empty());
        kitae_net::set_warn_insecure_http(false);
    }

    // ── Phase 0: background (pooled) top-level loads ────────────────────────

    /// Serves `{path → (delay_ms, body)}` routes as text/html on a loopback
    /// port, forever. Returns the base URL. The delay lets a test hold a
    /// fetch in flight on the shared pool deterministically.
    fn loopback_html_server(routes: Vec<(&'static str, u64, &'static str)>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let Some((delay, body)) = routes
                    .iter()
                    .find(|(p, _, _)| *p == path)
                    .map(|(_, d, b)| (*d, *b))
                else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                };
                if delay > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        url
    }

    fn page_text_of(b: &Browser) -> String {
        b.last_dl
            .as_ref()
            .map(|dl| {
                dl.texts
                    .iter()
                    .map(|t| t.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    }

    /// Evaluate a pure-JS expression in the page's runtime, stringified on
    /// the JS side so booleans and numbers read back as text.
    #[cfg(feature = "page-js")]
    fn read_js(b: &mut Browser, expr: &str) -> String {
        b.page_js
            .as_mut()
            .and_then(|p| p.host_eval_for_test(&format!("'' + ({expr})")))
            .unwrap_or_default()
    }

    /// A loopback server that also reads request bodies: known routes are
    /// served as HTML, everything else answers 200 text/plain with
    /// `[METHOD] <body>` echoed back. Records (method, path, content-type,
    /// body) so POST tests can assert what went on the wire.
    #[allow(clippy::type_complexity)]
    fn loopback_echo_server(
        routes: Vec<(&'static str, &'static str)>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, String, String, String)>>>,
    ) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_thread = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 2048];
                let mut header_end = None;
                while header_end.is_none() {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
                }
                let head_end = header_end.unwrap_or(buf.len());
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.split("\r\n");
                let request_line = lines.next().unwrap_or("").to_string();
                let method = request_line
                    .split_whitespace()
                    .next()
                    .unwrap_or("GET")
                    .to_string();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let mut content_length = 0usize;
                let mut content_type = String::new();
                for line in lines {
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            content_length = v.trim().parse().unwrap_or(0);
                        } else if k.eq_ignore_ascii_case("content-type") {
                            content_type = v.trim().to_string();
                        }
                    }
                }
                let mut body = buf[head_end..].to_vec();
                while body.len() < content_length {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&tmp[..n]);
                }
                body.truncate(content_length);
                let body = String::from_utf8_lossy(&body).to_string();
                seen_thread.lock().unwrap().push((
                    method.clone(),
                    path.clone(),
                    content_type,
                    body.clone(),
                ));
                let (ctype, payload) = match routes.iter().find(|(p, _)| *p == path) {
                    Some((_, html)) => ("text/html", (*html).to_string()),
                    None => ("text/plain", format!("[{method}] {body}")),
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (url, seen)
    }

    /// Drive `poll_fetches` until it applies the pending load, with a
    /// deadline — the pool finishes real HTTP waits off this thread, so
    /// the test polls like a host's idle tick does.
    fn drain_until_completes(b: &mut Browser) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !b.poll_fetches() {
            assert!(
                std::time::Instant::now() < deadline,
                "the pooled load never completed"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    struct LoadingHost {
        flags: std::sync::Arc<std::sync::Mutex<Vec<bool>>>,
        frames: usize,
    }

    impl Host for LoadingHost {
        fn publish(&mut self, _state: &PublicState, _dl: &DisplayList) {
            self.frames += 1;
        }
        fn scroll_updated(&mut self, _state: &PublicState) {}
        fn set_loading(&mut self, loading: bool) {
            self.flags.lock().unwrap().push(loading);
        }
        fn open_external(&mut self, _url: &str) {}
    }

    #[test]
    fn background_load_publishes_on_poll_fetches() {
        let server = loopback_html_server(vec![(
            "/doc",
            60,
            "<html><head><title>Pooled</title></head><body><h1>pooled body words</h1></body></html>",
        )]);
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.set_background_fetch(true);
        b.navigate(&format!("{server}/doc"));
        // Submit semantics: the fetch is in flight, nothing rendered yet.
        assert!(b.pending_load.is_some(), "a load rides the pool");
        assert!(b.last_dl.is_none(), "no pipeline ran before the response");
        drain_until_completes(&mut b);
        // Completion semantics: full pipeline replay.
        assert!(b.pending_load.is_none(), "the load is no longer pending");
        assert_eq!(b.page_title, "Pooled");
        assert_eq!(b.state().url, format!("{server}/doc"));
        assert!(
            page_text_of(&b).contains("pooled body words"),
            "the fetched page rendered: {:?}",
            page_text_of(&b)
        );
        // Quiescence: no second completion, no new work.
        std::thread::sleep(std::time::Duration::from_millis(40));
        assert!(!b.poll_fetches());
        assert_eq!(b.page_title, "Pooled");
    }

    #[test]
    fn default_mode_stays_synchronous() {
        let server = loopback_html_server(vec![(
            "/doc",
            0,
            "<html><body><p>inline body words</p></body></html>",
        )]);
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.navigate(&format!("{server}/doc"));
        // Without the mode flag nothing pooled: the page is fully rendered
        // by the time navigate returns (the direct-API/test contract).
        assert!(b.pending_load.is_none());
        assert!(
            page_text_of(&b).contains("inline body words"),
            "synchronous load rendered: {:?}",
            page_text_of(&b)
        );
    }

    #[test]
    fn local_schemes_stay_inline_in_background_mode() {
        let dir = std::env::temp_dir().join(format!("kitae_bg_local_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let page = dir.join("local.html");
        std::fs::write(&page, "<html><body><p>local file words</p></body></html>").expect("write");
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.set_background_fetch(true);
        b.navigate(&format!("file://{}", page.display()));
        assert!(b.pending_load.is_none(), "file:// never enters the pool");
        assert!(page_text_of(&b).contains("local file words"));
        b.navigate("about:blank");
        assert!(b.pending_load.is_none(), "about: never enters the pool");
        let _ = page_text_of(&b);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn superseded_loads_cannot_apply() {
        let server = loopback_html_server(vec![
            (
                "/slow-a",
                220,
                "<html><body><p>first page words</p></body></html>",
            ),
            (
                "/b",
                0,
                "<html><body><p>second page words</p></body></html>",
            ),
        ]);
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.set_background_fetch(true);
        b.navigate(&format!("{server}/slow-a"));
        b.navigate(&format!("{server}/b"));
        assert!(
            b.history.len() == 2,
            "history pushes stay synchronous: {:?}",
            b.history
        );
        drain_until_completes(&mut b);
        let text = page_text_of(&b);
        assert!(
            text.contains("second page words"),
            "the newer load wins: {:?}",
            text
        );
        // Quiescence: the abandoned slow load's late completion (if it
        // even arrived) must stay unclaimed — the page still shows B.
        std::thread::sleep(std::time::Duration::from_millis(120));
        assert!(!b.poll_fetches());
        assert!(page_text_of(&b).contains("second page words"));
    }

    // ── SCRIPTING_DECISION.md: page JavaScript (feature page-js) ──────────

    #[cfg(feature = "page-js")]
    fn js_browser() -> Browser {
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.set_page_js(true);
        b
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_renders_scripted_content() {
        let mut b = js_browser();
        b.load_html(
            "<html><body><p id=\"x\"></p><script>\
             document.getElementById('x').textContent = 'scripted words';\
             </script></body></html>",
            "https://example.test/js",
        );
        // The pipeline ran on the MUTATED tree: layout paints what the
        // script produced, not the empty paragraph.
        assert!(
            page_text_of(&b).contains("scripted words"),
            "scripted content rendered: {:?}",
            page_text_of(&b)
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_off_by_default() {
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.set_page_js(false);
        b.load_html(
            "<html><body><p id=\"x\"></p><script>\
             document.getElementById('x').textContent = 'never runs';\
             </script></body></html>",
            "https://example.test/nojs",
        );
        assert!(
            !page_text_of(&b).contains("never runs"),
            "script output must not render without the opt-in"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_all_failed_but_textful_renders_statically() {
        let mut b = js_browser();
        b.load_html(
            "<html><body><p>plenty of static words survive here</p><script>broken {{</script></body></html>",
            "https://example.test/fail",
        );
        assert!(
            page_text_of(&b).contains("plenty of static words"),
            "static content survives syntax failures"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_failing_only_scripts_fall_back_to_interstitial() {
        let server = loopback_html_server(vec![(
            "/shell",
            0,
            "<html><body><div id=\"app\"></div><script>a {{</script><script>b {{</script></body></html>",
        )]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/shell"));
        assert_eq!(
            b.page_title, "JavaScript required — Kitae",
            "an unrenderable scripted shell keeps the honest interstitial"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_document_click_listener_mutates() {
        let mut b = js_browser();
        b.load_html(
            "<html><body><div style=\"width:300px;height:200px\" id=\"area\"></div>\
             <p id=\"x\">before</p><script>\
             document.addEventListener('click', function () {\
               document.getElementById('x').textContent = 'clicked words';\
             });\
             </script></body></html>",
            "https://example.test/click",
        );
        assert!(page_text_of(&b).contains("before"));
        b.click(50.0, 30.0); // inside the area rect; page-space
        assert!(
            page_text_of(&b).contains("clicked words"),
            "document-level click listener mutated the page: {:?}",
            page_text_of(&b)
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_fetch_updates_the_page_on_poll() {
        let server = loopback_html_server(vec![
            (
                "/page",
                0,
                "<html><body><p id=\"x\">static</p><script>\
              document.write('FSUB');\
              fetch('/api').then(function (r) { return r.text(); }).then(function (t) {\
                document.write('FGOT:' + t);\
                document.getElementById('x').textContent = t;\
              });\
              </script></body></html>",
            ),
            ("/api", 0, "fetched words"),
        ]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/page"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(10));
            b.poll_page_js();
            if page_text_of(&b).contains("fetched words") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fetch never settled into the DOM; body: {:?}; console: {:?}; trace: {:?} (flag: {})",
                page_text_of(&b),
                b.page_js.as_ref().map(|p| p.take_console()).unwrap_or_default(),
                b.page_js.as_ref().map(|p| p.trace_snapshot()).unwrap_or_default(),
                b.page_js_enabled
            );
        }
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_inline_module_runs_in_strict_mode() {
        let mut b = js_browser();
        b.load_html(
            "<html><body><p id=\"x\">static</p>\
             <script type=\"module\">\
             document.getElementById('x').textContent = this === undefined ? 'strict confirmed' : 'sloppy';\
             </script></body></html>",
            "https://example.test/mod",
        );
        assert!(
            page_text_of(&b).contains("strict confirmed"),
            "inline module evaluated: {:?}",
            page_text_of(&b)
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_module_static_imports_fetch_over_the_cache() {
        let server = loopback_html_server(vec![
            (
                "/shell",
                0,
                "<html><body><p id=\"x\">static</p>\
                 <script type=\"module\" src=\"/app.js\"></script></body></html>",
            ),
            (
                "/app.js",
                0,
                "import { answer } from './dep.js';\
                 document.getElementById('x').textContent = 'module said ' + answer;",
            ),
            ("/dep.js", 0, "export const answer = 42;"),
        ]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/shell"));
        assert!(
            page_text_of(&b).contains("module said 42"),
            "external module chain resolved: {:?}",
            page_text_of(&b)
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_module_only_failure_keeps_the_interstitial() {
        let server = loopback_html_server(vec![(
            "/shell",
            0,
            "<html><body><div id=\"app\"></div>\
             <script type=\"module\">import './nope-a.js';</script>\
             <script type=\"module\">import './nope-b.js';</script></body></html>",
        )]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/shell"));
        assert_eq!(
            b.page_title, "JavaScript required — Kitae",
            "a module-only shell that cannot import keeps the honest interstitial"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_style_property_writes_reach_the_layout() {
        let mut b = js_browser();
        b.load_html(
            "<html><head><title>init</title></head><body><p id=\"x\">styled marker</p><script>\
             var el = document.getElementById('x');\
             el.style.color = 'red';\
             el.style.setProperty('background-color', 'yellow');\
             el.style.backgroundColor = 'blue';\
             document.title = el.style.backgroundColor;\
             el.style.removeProperty('background-color');\
             </script></body></html>",
            "https://example.test/style",
        );
        let page = b.page.as_ref().expect("page-js page mounted");
        let node = page.dom.get_element_by_id("x").unwrap();
        let style = page
            .dom
            .element(node)
            .and_then(|e| e.attr("style"))
            .unwrap_or_default()
            .to_string();
        assert!(
            style.contains("color: red") && !style.contains("color: blue"),
            "plain property writes land in the inline style: {style:?}"
        );
        assert!(
            !style.contains("background-color"),
            "removeProperty clears the declaration: {style:?}"
        );
        // The style object read camelCase state back into kebab-case:
        // the getter's answer became the title.
        assert_eq!(b.page_title, "blue", "camelCase getter round-tripped");
        // The DOM mutation re-laid out with the new color.
        b.poll_page_js();
        assert!(
            b.last_dl
                .as_ref()
                .map(|dl| dl.texts.iter().any(|t| t.color == 0xFF0000FF))
                .unwrap_or(false),
            "red text re-rendered after the style write"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_click_prevent_default_cancels_the_default_action() {
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut b = Browser::new(
            Box::new(CaptureHost {
                opened: opened.clone(),
                messages: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.set_page_js(true);
        b.load_html(
            "<html><body><a href=\"mailto:hi@example.com\" id=\"a\">mail marker</a><script>\
             document.getElementById('a').addEventListener('click', function (e) { e.preventDefault(); });\
             </script></body></html>",
            "https://example.test/pd",
        );
        assert!(page_text_of(&b).contains("mail marker"), "anchor rendered");
        let link = b
            .links()
            .first()
            .expect("the anchor became a link area")
            .clone();
        b.click(link.x + link.w / 2.0, link.y + link.h / 2.0);
        assert!(
            opened.lock().unwrap().is_empty(),
            "preventDefault stood the mailto open down: {:?}",
            opened.lock().unwrap()
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_fired_listeners_no_longer_own_the_click() {
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut b = Browser::new(
            Box::new(CaptureHost {
                opened: opened.clone(),
                messages: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.set_page_js(true);
        b.load_html(
            "<html><body><a href=\"mailto:x@example.com\" id=\"a\">delegated marker</a>\
             <p id=\"m\">init</p><script>\
             document.addEventListener('click', function () {\
               document.getElementById('m').textContent = 'tracked';\
             });\
             </script></body></html>",
            "https://example.test/deleg",
        );
        let link = b
            .links()
            .first()
            .expect("the anchor became a link area")
            .clone();
        b.click(link.x + link.w / 2.0, link.y + link.h / 2.0);
        assert!(
            page_text_of(&b).contains("tracked"),
            "the delegated listener ran: {:?}",
            page_text_of(&b)
        );
        assert_eq!(
            opened.lock().unwrap().as_slice(),
            ["mailto:x@example.com"],
            "the link still opened — a fired listener owns nothing"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_element_queries_scoping_closest_and_nth_child() {
        let mut b = js_browser();
        b.load_html(
            "<html><body>\
             <ul id=\"list\">\
             <li>alpha</li><li class=\"pick\">beta</li><li>gamma</li>\
             </ul>\
             <section><em id=\"deep\">deep marker</em></section>\
             <script>\
             var picks = document.querySelectorAll('li:nth-child(-n+2)');\
             if (picks.length !== 2) throw new Error('nth-child got ' + picks.length);\
             var mid = document.querySelector('.pick');\
             if (mid.closest('ul').id !== 'list') throw new Error('closest');\
             var scope = document.getElementById('list');\
             if (scope.querySelectorAll('li').length !== 3) throw new Error('scope count');\
             if (scope.querySelectorAll('em').length !== 0) throw new Error('scope must not escape');\
             document.getElementById('deep').textContent = 'counted ' + picks.length;\
             </script></body></html>",
            "https://example.test/q",
        );
        b.poll_page_js();
        assert!(
            page_text_of(&b).contains("counted 2"),
            "the scripted assertions did not all pass: {:?}",
            page_text_of(&b)
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_animation_frames_channels_and_microtasks_run() {
        let mut b = js_browser();
        b.load_html(
            "<html><body><p id=\"x\">static</p><script>             var out = [];             queueMicrotask(function () { out.push('micro'); });             requestAnimationFrame(function (ts) { out.push('raf' + (ts > 0)); });             var ch = new MessageChannel();             ch.port2.onmessage = function (e) { out.push('chan' + e.data); };             ch.port1.postMessage('42');             setInterval(function () { document.getElementById('x').textContent = out.join(' '); }, 10);             </script></body></html>",
            "https://example.test/async",
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(10));
            b.poll_page_js();
            let text = page_text_of(&b);
            if text.contains("micro") && text.contains("raf") && text.contains("chan42") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "async primitives never ran: {text:?}"
            );
        }
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_get_bounding_client_rect_answers_from_layout() {
        let mut b = js_browser();
        b.load_html(
            "<html><body>             <div id=\"box\" style=\"width:300px;height:200px;background:#dddddd\"></div>             <p id=\"m\">init</p><script>             setTimeout(function () {               var r = document.getElementById('box').getBoundingClientRect();               document.getElementById('m').textContent =                 Math.round(r.width) + 'x' + Math.round(r.height) + ' v' + window.innerWidth;             }, 0);             </script></body></html>",
            "https://example.test/geo",
        );
        b.poll_page_js();
        assert!(
            page_text_of(&b).contains("300x200 v400"),
            "rect + viewport answers come from the rendered layout: {:?}",
            page_text_of(&b)
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_push_state_moves_the_url_without_reloading() {
        let server = loopback_html_server(vec![(
            "/base",
            0,
            "<html><body><p id=\"keep\">base marker</p><p id=\"x\">init</p><script>             history.pushState({ page: 1 }, '', '/route-a');             document.getElementById('x').textContent = 'state ' + (history.state && history.state.page) + ' moved';             </script></body></html>",
        )]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/base"));
        b.poll_page_js();
        assert!(
            b.page_url.ends_with("/route-a"),
            "the address bar followed the state move: {}",
            b.page_url
        );
        let text = page_text_of(&b);
        assert!(
            text.contains("state 1 moved") && text.contains("base marker"),
            "the document survived (no reload): {text:?}"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_back_fires_popstate_on_the_loaded_document() {
        let server = loopback_html_server(vec![
            (
                "/a",
                0,
                "<html><body><p id=\"x\">a marker</p><script>                 document.addEventListener('popstate', function () {                   document.getElementById('x').textContent = 'popstate seen';                 });                 </script></body></html>",
            ),
            ("/b", 0, "<html><body><p>b marker</p></body></html>"),
        ]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/a"));
        b.navigate(&format!("{server}/b"));
        b.go(-1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let _ = b.poll_fetches();
            b.poll_page_js();
            if page_text_of(&b).contains("popstate seen") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "popstate never fired; url {:?}, body {:?}",
                b.page_url,
                page_text_of(&b)
            );
        }
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_click_bubbles_with_capture_and_stop_propagation() {
        let mut b = js_browser();
        b.load_html(
            "<html><body>             <div id=\"outer\" style=\"width:300px;height:200px;background:#eeeeee\">             <div id=\"inner\" style=\"width:100px;height:100px;background:#cccccc\"></div></div>             <p id=\"m\">init</p><script>             var log = [];             var stop = false;             document.getElementById('outer').addEventListener('click', function () { log.push('cap'); }, true);             document.getElementById('inner').addEventListener('click', function (e) {               log.push('in'); if (stop) e.stopPropagation();             });             document.getElementById('outer').addEventListener('click', function () { log.push('bub'); });             document.addEventListener('click', function () { log.push('doc'); });             window.__kitaeLog = log;             window.__kitaeStop = function () { stop = true; };             </script></body></html>",
            "https://example.test/bubble",
        );
        // First click: full capture → target → bubble → document chain.
        // The point must be inside #inner's box (body margin shifts it to
        // x 150..250).
        b.click(200.0, 60.0);
        let read = |b: &mut Browser| -> String {
            b.page_js
                .as_mut()
                .map(|p| {
                    p.host_eval_for_test("JSON.stringify(window.__kitaeLog)")
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            read(&mut b),
            "[\"cap\",\"in\",\"bub\",\"doc\"]",
            "capture/target/bubble order"
        );
        // Second click with stopPropagation at the target: no bubble, no doc.
        b.page_js
            .as_mut()
            .map(|p| p.host_eval_for_test("window.__kitaeStop()"));
        b.click(200.0, 60.0);
        assert_eq!(
            read(&mut b),
            "[\"cap\",\"in\",\"bub\",\"doc\",\"cap\",\"in\"]",
            "stopPropagation honored"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_typing_dispatches_input_and_key_events() {
        let mut b = js_browser();
        b.load_html(
            "<html><body>             <input id=\"t\" type=\"text\" style=\"width:200px;height:24px\">             <p id=\"m\">init</p><script>             var keys = 0;             var t = document.getElementById('t');             t.addEventListener('keydown', function () { keys++; });             t.addEventListener('input', function (e) {               document.getElementById('m').textContent = 'input:' + e.data + ':keys' + keys;             });             </script></body></html>",
            "https://example.test/typing",
        );
        let input_node = b
            .page
            .as_ref()
            .and_then(|p| p.dom.get_element_by_id("t"))
            .expect("input node");
        // Focus by click (engine form-control path), then type through the
        // engine's own caret insertion.
        b.click(20.0, 30.0);
        assert_eq!(b.focused, Some(input_node), "typing needs the caret");
        assert!(b.type_into_focused("hi"), "engine inserted");
        assert!(
            page_text_of(&b).contains("input:hi:keys1"),
            "input event carried the data; keydown fired first: {:?}",
            page_text_of(&b)
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_change_fires_for_discrete_controls() {
        let mut b = js_browser();
        b.load_html(
            "<html><body>             <input id=\"c\" type=\"checkbox\" style=\"width:20px;height:20px\">             <select id=\"s\" style=\"width:120px;height:24px\">             <option value=\"a\" selected>alpha</option><option value=\"b\">beta</option></select>             <script>             var log = [];             document.getElementById('c').addEventListener('change', function (e) {               log.push('c:' + (e.target.checked ? 'on' : 'off'));             });             document.getElementById('s').addEventListener('change', function (e) {               log.push('s:' + e.target.value);             });             window.__kitaeLog = log;             </script></body></html>",
            "https://example.test/change",
        );
        let cb = b
            .page
            .as_ref()
            .and_then(|p| p.dom.get_element_by_id("c"))
            .expect("checkbox node");
        let spot = b
            .last_dl
            .as_ref()
            .and_then(|dl| dl.rects.iter().find(|r| r.node == cb.0))
            .map(|r| (r.x + r.w / 2.0, r.y + r.h / 2.0))
            .expect("checkbox painted a box");
        b.click(spot.0, spot.1);
        let read = |b: &mut Browser| -> String {
            b.page_js
                .as_mut()
                .map(|p| {
                    p.host_eval_for_test("JSON.stringify(window.__kitaeLog)")
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            read(&mut b),
            "[\"c:on\"]",
            "a toggle commits: change carries the new checked state"
        );
        // The host dropdown reports through set_input_value — same commit.
        assert!(b.set_input_value("s", "b"), "option b exists");
        assert_eq!(
            read(&mut b),
            "[\"c:on\",\"s:b\"]",
            "select.value follows the choice"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_construct_events_with_options() {
        let mut b = js_browser();
        b.load_html(
            "<html><body>             <div id=\"outer\"><div id=\"inner\"></div></div>             <p id=\"m\">init</p><script>             var log = [];             var outer = document.getElementById('outer');             var inner = document.getElementById('inner');             outer.addEventListener('ping', function (e) { log.push('o:' + e.detail); });             inner.addEventListener('ping', function (e) { log.push('i:' + e.detail + ':' + (e.target === inner)); });             var hits = 0;             inner.addEventListener('tick', function () { hits += 1; }, { once: true });             var ctl = new AbortController();             inner.addEventListener('tick', function () { hits += 100; }, { signal: ctl.signal });             inner.addEventListener('block', function (e) { e.preventDefault(); }, { once: true });             var nonCancel = new Event('quiet', { bubbles: true });             inner.addEventListener('quiet', function (e) { e.preventDefault(); }, { once: true });             var r1 = inner.dispatchEvent(new CustomEvent('ping', { detail: 7, bubbles: true }));             inner.dispatchEvent(new CustomEvent('tick'));             ctl.abort();             inner.dispatchEvent(new CustomEvent('tick'));             var r2 = inner.dispatchEvent(new CustomEvent('block', { cancelable: true }));             inner.dispatchEvent(nonCancel);             var kb = new KeyboardEvent('keydown', { key: 'Enter' }).key;             var cap = 0;             outer.addEventListener('plain', function () { cap++; }, true);             var bub = 0;             outer.addEventListener('plain', function () { bub++; });             inner.dispatchEvent(new Event('plain', { bubbles: false }));             window.__r = [log.join('|'), r1, r2, hits, nonCancel.defaultPrevented, kb, cap, bub].join(' ');             </script></body></html>",
            "https://example.test/events",
        );
        let read = b
            .page_js
            .as_mut()
            .and_then(|p| p.host_eval_for_test("window.__r"))
            .unwrap_or_default();
        assert_eq!(
            read, "i:7:true|o:7 true false 101 false Enter 1 0",
            "constructors + propagation + once/signal/cancel semantics"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_url_search_params_and_clone() {
        let mut b = js_browser();
        b.load_html(
            "<html><body>             <p id=\"m\">init</p><script>             var u = new URL('/search?q=ada&q=lace#frag', location.href);             var sp = u.searchParams;             sp.set('q', 'one');             sp.append('extra', '2 words');             var list = sp.getAll('q').join(',');             var round = sp.toString();             var cyc = { a: 1 };             cyc.self = cyc;             var c = structuredClone(cyc);             var thrown = '';             try { structuredClone(function () {}); } catch (e) { thrown = e.name; }             var copy = structuredClone({ list: [1, 2, { deep: true }] });             window.__r = [u.pathname, u.host, u.hash, u.search, list, round, c.self === c, c.a, copy.list[2].deep, thrown].join(' ');             </script></body></html>",
            "https://example.test/urls",
        );
        let read = b
            .page_js
            .as_mut()
            .and_then(|p| p.host_eval_for_test("window.__r"))
            .unwrap_or_default();
        assert_eq!(
            read, "/search example.test #frag ?q=one&extra=2+words one q=one&extra=2+words true 1 true DataCloneError",
            "URL components, live searchParams linkage, structuredClone"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_dataset_reads_and_writes() {
        let mut b = js_browser();
        b.load_html(
            "<html><body>             <div id=\"d\" data-user-id=\"42\" data-flag></div>             <p id=\"m\">init</p><script>             var d = document.getElementById('d');             var a = d.dataset.userId;             d.dataset.role = 'admin';             var b2 = d.getAttribute('data-role');             delete d.dataset.flag;             var c = d.hasAttribute('data-flag');             var keys = Object.keys(d.dataset).join(',');             window.__r = a + ' ' + b2 + ' ' + c + ' ' + keys;             </script></body></html>",
            "https://example.test/data",
        );
        let read = b
            .page_js
            .as_mut()
            .and_then(|p| p.host_eval_for_test("window.__r"))
            .unwrap_or_default();
        assert_eq!(
            read, "42 admin false userId,role",
            "dataset camelCase mapping + deletion + enumeration"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_computed_style_reads() {
        let mut b = js_browser();
        b.load_html(
            "<html><body>             <div id=\"box\" style=\"width:50%;height:80px;color:#ff0000;background:#eeeeee\"></div>             <script>             setTimeout(function () {               var box = document.getElementById('box');               var cs = getComputedStyle(box);               var r = box.getBoundingClientRect();               window.__r = [                 cs.getPropertyValue('color'),                 cs.getPropertyValue('width'),                 cs.width === Math.round(r.width * 100) / 100 + 'px',                 cs.getPropertyValue('height'),                 '[' + cs.getPropertyValue('nonexistent') + ']'               ].join('|');             }, 0);             </script></body></html>",
            "https://example.test/style",
        );
        b.poll_page_js();
        let read = b
            .page_js
            .as_mut()
            .and_then(|p| p.host_eval_for_test("window.__r"))
            .unwrap_or_default();
        assert!(
            read.starts_with("rgb(255, 0, 0)|"),
            "computed color: {read:?}"
        );
        assert!(
            read.contains("|true|"),
            "used width answered in painted px: {read:?}"
        );
        assert!(read.contains("80px"), "computed height: {read:?}");
        assert!(
            read.ends_with("|[]"),
            "unknown property reads empty: {read:?}"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_scroll_focus_and_selection_apis() {
        let mut b = js_browser();
        b.load_html(
            "<html><body>             <div style=\"height:1200px;background:#eeeeee\"></div>             <input id=\"t\" type=\"text\" value=\"hello\" style=\"width:120px;height:24px\">             <script>             window.__t = document.getElementById('t');             window.__h = document.documentElement;             window.scrollTo(0, 250);             window.__t.focus();             </script></body></html>",
            "https://example.test/scroll",
        );
        b.poll_page_js();
        let input_node = b
            .page
            .as_ref()
            .and_then(|p| p.dom.get_element_by_id("t"))
            .expect("input node");
        assert!((b.scroll_y() - 250.0).abs() < 0.5, "scrollTo applied");
        assert_eq!(b.focused, Some(input_node), "focus() applied");
        assert_eq!(b.select_all(), "hello", "engine selection");
        // Read through the page's own wrappers: `host_eval_for_test` runs
        // outside the DOM scope, so the expression touches only JS state
        // (the wrappers were minted during the script phase and are cached).
        let read = b
            .page_js
            .as_mut()
            .and_then(|p| {
                p.host_eval_for_test(
                    "window.scrollY + ' ' + (document.activeElement === window.__t) + ' ' + window.__h.scrollTop + ' ' + window.getSelection().toString()",
                )
            })
            .unwrap_or_default();
        assert_eq!(
            read, "250 true 250 hello",
            "the view state reads back through the JS surface"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_mutation_observer_records_attributes_and_children() {
        let mut b = js_browser();
        b.load_html(
            "<html><body><p>seed</p>\
             <div id=\"host\"></div>\
             <script>\
             window.__log = [];\
             var host = document.getElementById('host');\
             host.setAttribute('a', '1');\
             var mo = new MutationObserver(function (recs) {\
               for (var i = 0; i < recs.length; i++) {\
                 var r = recs[i];\
                 window.__log.push(r.type + ':' + (r.attributeName === null ? '-' : r.attributeName) + ':' + (r.oldValue === null ? '-' : r.oldValue) + ':' + r.addedNodes.length + ':' + r.removedNodes.length + ':' + r.target.tagName);\
               }\
             });\
             mo.observe(host, { childList: true, attributes: true, attributeOldValue: true, subtree: true });\
             host.setAttribute('a', '2');\
             var span = document.createElement('span');\
             host.appendChild(span);\
             span.textContent = 'x';\
             host.removeAttribute('a');\
             </script></body></html>",
            "https://example.test/mut",
        );
        let read = |b: &mut Browser| -> String {
            b.page_js
                .as_mut()
                .and_then(|p| p.host_eval_for_test("window.__log.join('|')"))
                .unwrap_or_default()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !read(&mut b).contains("attributes:a:2") {
            b.poll_page_js();
            std::thread::sleep(std::time::Duration::from_millis(3));
            assert!(
                std::time::Instant::now() < deadline,
                "records: {:?}",
                read(&mut b)
            );
        }
        assert_eq!(
            read(&mut b),
            "attributes:a:1:0:0:DIV|childList:-:-:1:0:DIV|childList:-:-:1:0:SPAN|attributes:a:2:0:0:DIV",
            "journaled records arrive in mutation order with old values and subtree coverage"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_mutation_observer_take_records_and_disconnect() {
        let mut b = js_browser();
        b.load_html(
            "<html><body><p>seed</p>\
             <div id=\"host\"></div>\
             <script>\
             window.__calls = 0;\
             var host = document.getElementById('host');\
             var mo = new MutationObserver(function () { window.__calls++; });\
             mo.observe(host, { attributes: true });\
             host.setAttribute('a', '1');\
             window.__taken = mo.takeRecords().length;\
             host.setAttribute('b', '2');\
             mo.disconnect();\
             host.setAttribute('c', '3');\
             window.__r = window.__taken + ':' + window.__calls;\
             </script></body></html>",
            "https://example.test/mut2",
        );
        for _ in 0..8 {
            b.poll_page_js();
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        let read = b
            .page_js
            .as_mut()
            .and_then(|p| p.host_eval_for_test("window.__r"))
            .unwrap_or_default();
        assert_eq!(
            read, "1:0",
            "takeRecords drains synchronously; disconnect silences the observer"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_intersection_observer_tracks_scrolling() {
        let mut b = js_browser();
        b.load_html(
            "<html><body style=\"margin:0\">\
             <div style=\"height:2000px\"></div>\
             <div id=\"t\" style=\"height:100px;background:#dddddd\"></div>\
             <script>\
             window.__seq = [];\
             var t = document.getElementById('t');\
             var io = new IntersectionObserver(function (es) {\
               for (var i = 0; i < es.length; i++) window.__seq.push(es[i].isIntersecting ? 'in' : 'out');\
             });\
             io.observe(t);\
             setTimeout(function () { window.__seq.push('scrolling'); window.scrollTo(0, 1700); }, 30);\
             setTimeout(function () { window.__r = window.__seq.join('|'); }, 90);\
             </script></body></html>",
            "https://example.test/io",
        );
        let read = |b: &mut Browser| -> String {
            b.page_js
                .as_mut()
                .and_then(|p| p.host_eval_for_test("window.__r || ''"))
                .unwrap_or_default()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !read(&mut b).contains("scrolling") {
            b.poll_page_js();
            std::thread::sleep(std::time::Duration::from_millis(3));
            assert!(
                std::time::Instant::now() < deadline,
                "seq: {:?}",
                read(&mut b)
            );
        }
        let seq = read(&mut b);
        assert!(
            seq.starts_with("out|scrolling|in"),
            "below the fold first, intersecting after the script's scroll: {seq:?}"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_resize_observer_reports_size_changes() {
        let mut b = js_browser();
        b.load_html(
            "<html><body style=\"margin:0\">\
             <div id=\"t\" style=\"width:200px;height:100px;background:#dddddd\"></div>\
             <script>\
             window.__seq = [];\
             var t = document.getElementById('t');\
             var ro = new ResizeObserver(function (es) {\
               for (var i = 0; i < es.length; i++) window.__seq.push(Math.round(es[i].contentRect.width) + 'x' + Math.round(es[i].contentRect.height));\
             });\
             ro.observe(t);\
             setTimeout(function () { t.style.width = '400px'; }, 30);\
             setTimeout(function () { window.__r = window.__seq.join('|'); }, 90);\
             </script></body></html>",
            "https://example.test/ro",
        );
        let read = |b: &mut Browser| -> String {
            b.page_js
                .as_mut()
                .and_then(|p| p.host_eval_for_test("window.__r || ''"))
                .unwrap_or_default()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !read(&mut b).contains('|') {
            b.poll_page_js();
            std::thread::sleep(std::time::Duration::from_millis(3));
            assert!(
                std::time::Instant::now() < deadline,
                "seq: {:?}",
                read(&mut b)
            );
        }
        let seq = read(&mut b);
        let last_w: f32 = seq
            .rsplit('|')
            .next()
            .and_then(|box_| box_.split('x').next())
            .and_then(|w| w.parse().ok())
            .unwrap_or(0.0);
        assert!(
            seq.starts_with("200x100") && last_w > 300.0,
            "initial size then the scripted resize (width grew): {seq:?}"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_xhr_reads_a_response_through_the_pool() {
        let (server, _seen) = loopback_echo_server(vec![(
            "/page",
            "<html><body><p>xhr</p><script>\
             var xhr = new XMLHttpRequest();\
             window.__seq = [];\
             xhr.onreadystatechange = function () { window.__seq.push(xhr.readyState); };\
             xhr.open('GET', '/data');\
             xhr.setRequestHeader('X-Probe', '1');\
             xhr.onload = function () {\
               window.__r = xhr.status + '|' + xhr.responseText + '|' + xhr.getResponseHeader('content-type') + '|' + (xhr.getAllResponseHeaders().indexOf('content-length: ') >= 0);\
             };\
             xhr.onerror = function () { window.__r = 'error'; };\
             xhr.send();\
             </script></body></html>",
        )]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/page"));
        let read = |b: &mut Browser| -> String {
            b.page_js
                .as_mut()
                .and_then(|p| p.host_eval_for_test("'' + (window.__r || '')"))
                .unwrap_or_default()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while read(&mut b).is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(5));
            b.poll_page_js();
            assert!(
                std::time::Instant::now() < deadline,
                "xhr never settled; r {:?} trace {:?}",
                read(&mut b),
                b.page_js
                    .as_ref()
                    .map(|p| p.trace_snapshot().join(" / "))
                    .unwrap_or_default()
            );
        }
        let text = read(&mut b);
        assert!(
            text.starts_with("200|[GET] "),
            "status and echoed body arrived: {text:?}"
        );
        assert!(
            text.contains("text/plain") && text.ends_with("true"),
            "response headers readable: {text:?}"
        );
        let seq = b
            .page_js
            .as_mut()
            .and_then(|p| p.host_eval_for_test("window.__seq.join(',')"))
            .unwrap_or_default();
        assert!(
            seq.starts_with("1,") && seq.ends_with(",4"),
            "readyState ladder reports OPENED then DONE: {seq:?}"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_worker_round_trips_messages() {
        let (server, _seen) = loopback_echo_server(vec![
            (
                "/page",
                "<html><body><p>worker</p><script>\
                 window.__msgs = [];\
                 var w = new Worker('/worker.js');\
                 w.onmessage = function (e) {\
                   window.__msgs.push('echo:' + e.data.n);\
                   if (window.__msgs.length === 2) { window.__r = window.__msgs.join(','); w.terminate(); }\
                 };\
                 w.onerror = function (e) { window.__r = 'worker-error:' + e.message; };\
                 w.postMessage({ n: 41 });\
                 </script></body></html>",
            ),
            (
                "/worker.js",
                "self.postMessage({ n: 1 });\
                 self.onmessage = function (e) { self.postMessage({ n: e.data.n + 1 }); };",
            ),
        ]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/page"));
        let read = |b: &mut Browser| -> String {
            b.page_js
                .as_mut()
                .and_then(|p| p.host_eval_for_test("window.__r || ''"))
                .unwrap_or_default()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while read(&mut b).is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(5));
            b.poll_page_js();
            assert!(
                std::time::Instant::now() < deadline,
                "worker never answered"
            );
        }
        assert_eq!(
            read(&mut b),
            "echo:1,echo:42",
            "worker init message then the round trip"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_storage_is_origin_scoped_and_ordered() {
        pagejs::reset_web_storage();
        let (server, _seen) = loopback_echo_server(vec![
            (
                "/one",
                "<html><body><p>storage one</p><script>\
                 localStorage.setItem('alpha', 'first');\
                 localStorage.setItem('beta', 'second');\
                 localStorage.beta = 'third';\
                 window.__one = [\
                   localStorage.getItem('alpha'),\
                   localStorage.length,\
                   localStorage.key(0),\
                   localStorage.key(1),\
                   localStorage.beta,\
                   ('beta' in localStorage),\
                   Object.keys(localStorage).join(','),\
                 ].join('|');\
                 </script></body></html>",
            ),
            (
                "/two",
                "<html><body><p>storage two</p><script>\
                 window.__two = localStorage.getItem('beta') + '|' + localStorage.length;\
                 localStorage.removeItem('alpha');\
                 localStorage.setItem('alpha', 'readded');\
                 window.__order = localStorage.key(0) + ',' + localStorage.key(1);\
                 delete localStorage.alpha;\
                 window.__after = localStorage.getItem('alpha') + '|' + localStorage.length + '|' + ('alpha' in localStorage);\
                 </script></body></html>",
            ),
        ]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/one"));
        let one = read_js(&mut b, "window.__one");
        assert_eq!(
            one, "first|2|alpha|beta|third|true|alpha,beta",
            "writes, the named-property read and key order on the first page"
        );
        // A second browser at the same origin sees the same localStorage:
        // the area is per origin, not per page.
        let mut c = js_browser();
        c.navigate(&format!("{server}/two"));
        assert_eq!(
            read_js(&mut c, "window.__two"),
            "third|2",
            "same-origin localStorage is shared across browsers"
        );
        assert_eq!(
            read_js(&mut c, "window.__order"),
            "beta,alpha",
            "removeItem + setItem moves the key to the end (spec order)"
        );
        assert_eq!(
            read_js(&mut c, "window.__after"),
            "null|1|false",
            "delete removes the named property and shortens the area"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_session_storage_is_private_to_one_browser() {
        pagejs::reset_web_storage();
        let (server, _seen) = loopback_echo_server(vec![
            (
                "/write",
                "<html><body><p>session one</p><script>\
                 sessionStorage.setItem('token', 'abc');\
                 window.__r = sessionStorage.getItem('token') + '|' + sessionStorage.length;\
                 </script></body></html>",
            ),
            (
                "/read",
                "<html><body><p>session two</p><script>\
                 window.__r = sessionStorage.getItem('token') + '|' + sessionStorage.length;\
                 </script></body></html>",
            ),
        ]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/write"));
        assert_eq!(read_js(&mut b, "window.__r"), "abc|1", "session write");
        // Same browser, same origin, new document: the session area
        // survives navigation inside the tab.
        b.navigate(&format!("{server}/read"));
        assert_eq!(
            read_js(&mut b, "window.__r"),
            "abc|1",
            "sessionStorage survives same-tab navigation"
        );
        // A different browser is a different session: its area is empty.
        let mut c = js_browser();
        c.navigate(&format!("{server}/read"));
        assert_eq!(
            read_js(&mut c, "window.__r"),
            "null|0",
            "sessionStorage is private to its browser"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_storage_refuses_an_opaque_origin() {
        pagejs::reset_web_storage();
        let mut b = js_browser();
        // file: URLs have an opaque origin; every operation must refuse.
        b.load_html(
            "<html><body><p>opaque</p><script>\
             var out = [];\
             try { localStorage.getItem('x'); out.push('get'); } catch (e) { out.push(e.name); }\
             try { localStorage.setItem('x', '1'); out.push('set'); } catch (e) { out.push(e.name); }\
             try { localStorage.length; out.push('length'); } catch (e) { out.push(e.name); }\
             window.__r = out.join(',');\
             </script></body></html>",
            "file:///tmp/kitae-storage.html",
        );
        assert_eq!(
            read_js(&mut b, "window.__r"),
            "SecurityError,SecurityError,SecurityError",
            "opaque origins cannot touch Web Storage"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn web_raster_decodes_a_png_data_url() {
        // The raster store is process-global: hold the test lock and clear
        // it so parallel decode tests can't leak entries into these
        // assertions (and vice versa).
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let img = kitae_media::RgbaImage {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 255, 0, 0, 255, 255],
        };
        let png = kitae_media::encode_png(&img).expect("encode");
        let url = format!("data:image/png;base64,{}", b64_encode(&png));
        let raster = ensure_web_raster(&url).expect("a PNG data URL decodes");
        assert_eq!((raster.width, raster.height), (2, 1));
        assert_eq!(&raster.pixels[0..4], &[255, 0, 0, 255]);
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_canvas_paints_shapes_into_the_raster_cache() {
        // The raster store is process-global: hold the test lock and clear
        // it so parallel decode tests can't leak entries into these
        // assertions (and vice versa).
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let mut b = js_browser();
        b.load_html(
            "<html><body><p>canvas</p>\
             <canvas id=\"c\" width=\"120\" height=\"60\"></canvas>\
             <script>\
             var c = document.getElementById('c');\
             var g = c.getContext('2d');\
             g.fillStyle = '#ff0000';\
             g.fillRect(0, 0, 10, 10);\
             g.fillStyle = 'rgb(0, 128, 255)';\
             g.fillRect(20, 10, 5, 5);\
             window.__r = g.canvas.width + 'x' + g.canvas.height + '|' + c.width + 'x' + c.height;\
             </script></body></html>",
            "https://canvas.test/page",
        );
        b.poll_page_js();
        assert_eq!(
            read_js(&mut b, "window.__r"),
            "120x60|120x60",
            "the context reports the canvas element's IDL size"
        );
        let dl = b.last_dl.as_ref().expect("published frame");
        let im = dl
            .images
            .iter()
            .find(|im| im.src.starts_with("\u{0}canvas:"))
            .expect("the canvas is a display-list image");
        assert_eq!((im.w, im.h), (120.0, 60.0), "the box is the attribute size");
        let payload = im.payload.expect("the flushed buffer registered a raster");
        let raster = qtsq_raster_for_id(payload).expect("registered pixels");
        assert_eq!((raster.width, raster.height), (120, 60));
        let px = |x: usize, y: usize| {
            let i = (y * 120 + x) * 4;
            [
                raster.pixels[i],
                raster.pixels[i + 1],
                raster.pixels[i + 2],
                raster.pixels[i + 3],
            ]
        };
        assert_eq!(px(1, 1), [255, 0, 0, 255], "a hex fillRect landed");
        assert_eq!(px(22, 12), [0, 128, 255, 255], "an rgb() fillRect landed");
        assert_eq!(
            px(50, 30),
            [0, 0, 0, 0],
            "untouched pixels stay transparent"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_canvas_image_data_and_png_round_trip() {
        // The raster store is process-global: hold the test lock and clear
        // it so parallel decode tests can't leak entries into these
        // assertions (and vice versa).
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let mut b = js_browser();
        b.load_html(
            "<html><body><p>canvas data</p>\
             <canvas id=\"c\" width=\"8\" height=\"4\"></canvas>\
             <script>\
             var c = document.getElementById('c');\
             var g = c.getContext('2d');\
             g.fillStyle = 'blue';\
             g.fillRect(0, 0, 4, 2);\
             var img = g.getImageData(0, 0, 4, 2);\
             window.__read = [img.data[0], img.data[1], img.data[2], img.data[3]].join(',') + '|' + img.width + 'x' + img.height;\
             g.putImageData(img, 4, 2);\
             var back = g.getImageData(4, 2, 1, 1).data;\
             window.__put = [back[0], back[1], back[2], back[3]].join(',');\
             window.__url = c.toDataURL();\
             </script></body></html>",
            "https://canvas.test/data",
        );
        b.poll_page_js();
        assert_eq!(
            read_js(&mut b, "window.__read"),
            "0,0,255,255|4x2",
            "getImageData reads the drawn pixels"
        );
        assert_eq!(
            read_js(&mut b, "window.__put"),
            "0,0,255,255",
            "putImageData writes them back"
        );
        // The data URL is a PNG the engine's own decoder accepts.
        let url = read_js(&mut b, "window.__url");
        let b64 = url
            .strip_prefix("data:image/png;base64,")
            .unwrap_or_else(|| panic!("canvas PNG data URL: {url:?}"));
        let png = b64_decode(b64).expect("base64 payload");
        let decoded = kitae_media::decode_png(&png).expect("engine decodes its own canvas PNG");
        assert_eq!((decoded.width, decoded.height), (8, 4));
        assert_eq!(
            &decoded.pixels[0..4],
            &[0, 0, 255, 255],
            "the round-tripped pixel is the drawn blue"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_canvas_draws_images_and_honours_transforms() {
        // The raster store is process-global: hold the test lock and clear
        // it so parallel decode tests can't leak entries into these
        // assertions (and vice versa).
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        // A 2x2 PNG: red, green / blue, white.
        let img = kitae_media::RgbaImage {
            width: 2,
            height: 2,
            pixels: vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ],
        };
        let png = kitae_media::encode_png(&img).expect("encode");
        let src = format!("data:image/png;base64,{}", b64_encode(&png));
        let mut b = js_browser();
        b.load_html(
            &format!(
                "<html><body><p>canvas draw</p>\
                 <img id=\"i\" src=\"{src}\">\
                 <canvas id=\"c\" width=\"20\" height=\"20\"></canvas>\
                 <script>\
                 var c = document.getElementById('c');\
                 var g = c.getContext('2d');\
                 g.drawImage(document.getElementById('i'), 2, 2, 4, 4);\
                 g.translate(10, 10);\
                 g.fillStyle = '#00ff00';\
                 g.fillRect(0, 0, 2, 2);\
                 var d = g.getImageData(3, 3, 1, 1).data;\
                 var t = g.getImageData(11, 11, 1, 1).data;\
                 window.__r = [d[0], d[1], d[2], d[3]].join(',') + '|' + [t[0], t[1], t[2], t[3]].join(',');\
                 </script></body></html>"
            ),
            "https://canvas.test/draw",
        );
        b.poll_page_js();
        assert_eq!(
            read_js(&mut b, "window.__r"),
            "255,0,0,255|0,255,0,255",
            "drawImage scaled the PNG (top-left source pixel) and translate moved the fill"
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_fetch_post_carries_formdata_over_the_pool() {
        let (server, seen) = loopback_echo_server(vec![(
            "/page",
            "<html><body><p id=\"m\">init</p><script>             var fd = new FormData();             fd.append('name', 'ada');             fd.append('note', 'two words');             fetch('/submit', { method: 'POST', body: fd })               .then(function (r) {                 var head = r.headers.get('content-type') + '|' + r.status;                 return r.text().then(function (t) { return head + '|' + t; });               })               .then(function (t) { window.__r = t; })               .catch(function (e) { window.__r = 'caught:' + e; });             </script></body></html>",
        )]);
        let mut b = js_browser();
        b.navigate(&format!("{server}/page"));
        let read = |b: &mut Browser| -> String {
            b.page_js
                .as_mut()
                .and_then(|p| p.host_eval_for_test("window.__r || ''"))
                .unwrap_or_default()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !read(&mut b).contains("[POST]") {
            std::thread::sleep(std::time::Duration::from_millis(5));
            b.poll_page_js();
            assert!(
                std::time::Instant::now() < deadline,
                "the POST never landed: {:?} seen {:?}",
                read(&mut b),
                seen.lock().unwrap()
            );
        }
        let text = read(&mut b);
        assert!(
            text.contains("text/plain|200|[POST]"),
            "Response shape came back: {text:?}"
        );
        assert!(
            text.contains("name=\"name\"") && text.contains("two words"),
            "multipart fields rode the body: {text:?}"
        );
        let requests = seen.lock().unwrap();
        let post = requests
            .iter()
            .find(|(m, p, _, _)| m == "POST" && p == "/submit")
            .expect("the POST was recorded");
        assert!(
            post.2.starts_with("multipart/form-data; boundary="),
            "FormData set its own content type: {}",
            post.2
        );
        assert!(
            post.3.contains("name=\"note\"") && post.3.contains("two words"),
            "the wire body carried the fields: {:?}",
            post.3
        );
    }

    #[cfg(feature = "page-js")]
    #[test]
    fn page_js_submit_prevent_default_blocks_engine_submission() {
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut b = Browser::new(
            Box::new(CaptureHost {
                opened: opened.clone(),
                messages: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.set_page_js(true);
        b.load_html(
            "<html><body>             <form id=\"f\" action=\"https://elsewhere.test/go\" method=\"get\">             <input type=\"text\" name=\"q\" value=\"x\">             <input id=\"go\" type=\"submit\" value=\"go\" style=\"width:80px;height:24px\">             </form><p id=\"m\">init</p><script>             document.getElementById('f').addEventListener('submit', function (e) {               e.preventDefault();               document.getElementById('m').textContent = 'held by script';             });             </script></body></html>",
            "https://example.test/form",
        );
        let submit_node = b
            .page
            .as_ref()
            .and_then(|p| p.dom.get_element_by_id("go"))
            .expect("submit button node");
        let submit_area = b
            .last_dl
            .as_ref()
            .and_then(|dl| {
                dl.rects
                    .iter()
                    .find(|r| r.node == submit_node.0)
                    .map(|r| (r.x + r.w / 2.0, r.y + r.h / 2.0))
            })
            .expect("submit button rect");
        b.click(submit_area.0, submit_area.1);
        assert!(
            page_text_of(&b).contains("held by script"),
            "submit listener ran: {:?}",
            page_text_of(&b)
        );
        assert!(
            opened.lock().unwrap().is_empty(),
            "preventDefault kept the engine from submitting: {:?}",
            opened.lock().unwrap()
        );
        assert_eq!(b.page_url, "https://example.test/form", "no navigation");
    }

    #[test]
    fn loading_events_match_the_synchronous_path() {
        // The pooled replay must be observable-identical to the inline
        // load it replaces: same loading-event bracket, including the
        // load paths' own internal flags. Two routes on one server, two
        // browsers, one sequence comparison.
        let server = loopback_html_server(vec![
            ("/sync", 0, "<html><body><p>parity words</p></body></html>"),
            ("/bg", 60, "<html><body><p>parity words</p></body></html>"),
        ]);
        let flags_sync = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut sync_b = Browser::new(
            Box::new(LoadingHost {
                flags: flags_sync.clone(),
                frames: 0,
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        sync_b.navigate(&format!("{server}/sync"));
        let sync_seq = flags_sync.lock().unwrap().clone();

        let flags_bg = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut bg_b = Browser::new(
            Box::new(LoadingHost {
                flags: flags_bg.clone(),
                frames: 0,
            }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        bg_b.set_background_fetch(true);
        bg_b.navigate(&format!("{server}/bg"));
        assert_eq!(
            flags_bg.lock().unwrap().as_slice(),
            [true],
            "loading turns on at submit"
        );
        drain_until_completes(&mut bg_b);
        assert_eq!(
            flags_bg.lock().unwrap().as_slice(),
            &sync_seq[..],
            "the pooled load emits the exact synchronous event stream"
        );
        assert_eq!(sync_seq.first(), Some(&true), "starts loading");
        assert_eq!(sync_seq.last(), Some(&false), "ends loaded");
    }

    #[test]
    fn clean_resize_skips_relayout_but_dirty_repaints() {
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 300.0),
        );
        b.navigate("about:blank");
        let base = b.repaint_count();
        assert!(base >= 1, "navigate relayouts");
        b.resize(400.0, 300.0);
        assert_eq!(
            b.repaint_count(),
            base,
            "same viewport is clean: no repaint"
        );
        b.resize(800.0, 300.0);
        assert_eq!(b.repaint_count(), base + 1, "new width dirties layout");
    }

    // ── B1: CSS transitions ───────────────────────────────────────────────

    /// Hover flips `background-color` under a 1s linear transition: the
    /// mid-tick frame paints the interpolated color, the finished tick
    /// releases the override and paints the end value.
    #[test]
    fn transition_interpolates_background_color_over_ticks() {
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            "<style>#box{transition: background-color 1s linear; background-color: #000000}\
             #box:hover{background-color: #FFFFFF}</style><div id=box>x</div>",
            "about:blank",
        );
        assert!(!b.tick_transitions_at(0.0), "anchor clock (no runs yet)");
        let nid = b
            .page
            .as_ref()
            .unwrap()
            .dom
            .get_element_by_id("box")
            .expect("box node");
        b.set_hover(Some(nid));
        // Mid-tick: black → white at k=0.5.
        assert!(b.tick_transitions_at(0.5), "still running at half");
        let dl = b.last_dl.clone().expect("published");
        let rect = dl.rects.iter().find(|r| r.node == nid.0).expect("bg rect");
        assert_eq!(
            rect.color & 0xFFFFFF00,
            0x80808000,
            "midpoint must be #808080, got {:08X}",
            rect.color
        );
        // End: override released, true end value paints.
        assert!(!b.tick_transitions_at(1.5), "finished → inactive");
        let dl = b.last_dl.clone().unwrap();
        let rect = dl.rects.iter().find(|r| r.node == nid.0).unwrap();
        assert_eq!(rect.color & 0xFFFFFF00, 0xFFFFFF00, "end value #FFFFFF");
    }

    /// `opacity` transitions ride the layout alpha bake: the eased value
    /// overrides the style, the bake multiplies it into item alphas.
    #[test]
    fn transition_interpolates_opacity_over_ticks() {
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            "<style>#box{transition: opacity 1s linear; opacity: 1; background: #0000FF}\
             #box:hover{opacity: 0}</style><div id=box style='width:40px;height:20px'>x</div>",
            "about:blank",
        );
        b.tick_transitions_at(0.0);
        let nid = b
            .page
            .as_ref()
            .unwrap()
            .dom
            .get_element_by_id("box")
            .unwrap();
        b.set_hover(Some(nid));
        b.tick_transitions_at(0.5);
        let dl = b.last_dl.clone().unwrap();
        let rect = dl.rects.iter().find(|r| r.node == nid.0).expect("bg rect");
        assert_eq!(
            rect.color & 0xFF,
            128,
            "opacity 0.5 × 255 alpha, got {:08X}",
            rect.color
        );
    }

    /// `transform` transitions flow through layout's matrix path: at the
    /// midpoint of rotate(0deg)→rotate(90deg) the rect carries cos 45°.
    #[test]
    fn transition_interpolates_transform_over_ticks() {
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            "<style>#box{transition: transform 1s linear; display: block; width: 40px; height: 20px;\
             background: #00FF00}#box:hover{transform: rotate(90deg)}</style><div id=box>x</div>",
            "about:blank",
        );
        b.tick_transitions_at(0.0);
        let nid = b
            .page
            .as_ref()
            .unwrap()
            .dom
            .get_element_by_id("box")
            .unwrap();
        b.set_hover(Some(nid));
        b.tick_transitions_at(0.5);
        let dl = b.last_dl.clone().unwrap();
        let rect = dl
            .rects
            .iter()
            .find(|r| r.node == nid.0)
            .expect("transformed rect");
        let m = rect.transform.expect("mid-rotation matrix");
        assert!(
            (m.0[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01,
            "cos 45°, got {}",
            m.0[0]
        );
    }

    /// Without a `transition` declaration the same restyle jumps instantly —
    /// the guard against accidentally animating everything.
    #[test]
    fn restyle_without_transition_declaration_is_instant() {
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            "<style>#box{background-color: #000000}#box:hover{background-color: #FFFFFF}</style>\
             <div id=box>x</div>",
            "about:blank",
        );
        b.tick_transitions_at(0.0);
        let nid = b
            .page
            .as_ref()
            .unwrap()
            .dom
            .get_element_by_id("box")
            .unwrap();
        b.set_hover(Some(nid));
        assert!(!b.tick_transitions_at(0.5), "no runs");
        let dl = b.last_dl.clone().unwrap();
        let rect = dl.rects.iter().find(|r| r.node == nid.0).unwrap();
        assert_eq!(rect.color & 0xFFFFFF00, 0xFFFFFF00, "instant end value");
    }

    // ── Phase 1 acceptance (docs/UI_LIBRARY_PLAN.md) ──────────────────────

    /// A frame-counting host: the repaint-counter proof needs to know a
    /// splice published a frame without a full relayout.
    struct CountingHost {
        frames: usize,
    }
    impl Host for CountingHost {
        fn publish(&mut self, _state: &PublicState, _dl: &DisplayList) {
            self.frames += 1;
        }
        fn scroll_updated(&mut self, _state: &PublicState) {}
        fn set_loading(&mut self, _loading: bool) {}
        fn open_external(&mut self, _url: &str) {}
    }

    /// The start page mounts as a kitae_ui app and publishes normally.
    #[test]
    fn start_page_mounts_and_publishes() {
        let mut b = Browser::new(
            Box::new(CountingHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.navigate(START_URL);
        assert_eq!(b.state().url, START_URL);
        assert_eq!(b.state().title, "Start");
        assert!(b.content_height() > 100.0, "start page has content");
        assert!(!b.links().is_empty(), "start page has link areas");
        assert_eq!(b.repaint_count(), 1, "initial mount = one full layout");
    }

    /// The hero's words must be laid out one space apart even inside a
    /// styled run: with `ApproxMeasurer` every character — the space
    /// included — is 0.5×size, so consecutive words of the 64px h1 sit
    /// exactly n×32 + 32 px apart. (In the real app this caught the Q11
    /// regression where named-family runs measured in the host default
    /// font: painted Georgia glyphs overflowed their layout slots and
    /// the title rendered as one glued word.)
    #[test]
    fn start_page_hero_words_keep_their_spaces() {
        let mut b = Browser::new(
            Box::new(CountingHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (1892.0, 989.0),
        );
        b.navigate(START_URL);
        let dl = b.last_display_list().expect("start page published");
        // The h1 is the topmost text on the page.
        let top = |needle: &str| {
            dl.texts
                .iter()
                .filter(|t| t.text == needle)
                .min_by(|a, b| a.y.total_cmp(&b.y).then(a.x.total_cmp(&b.x)))
                .unwrap_or_else(|| panic!("run {needle:?} missing from the display list"))
                .clone()
        };
        let kitae = top("Kitae,");
        let built = top("built");
        // "Kitae," is 6 chars × 32px; the separating space is 32px.
        let advance = built.x - kitae.x;
        assert!(
            (advance - (6.0 + 1.0) * 32.0).abs() < 2.0,
            "built must start one 64px space after Kitae (expected 224px advance, got {advance})"
        );
    }

    /// The start page's hero must reach the display list full-bleed (body
    /// padding reset) and fill the whole first window: every section of
    /// the page is 100vh tall, so the hero and its background image
    /// resolve to the viewport height (989 in this test's 1892×989
    /// viewport). Percentage heights on out-of-flow boxes aren't
    /// resolvable yet, so the image pins the height in vh explicitly. The
    /// background ships as an embedded QTSQ file riding an inline
    /// `data:image/qtsq;base64,` URL; the relayout stamping pass must
    /// decode it and stamp the payload id the renderer resolves pixels
    /// through.
    #[test]
    fn start_page_hero_reaches_the_display_list() {
        let _store = qtsq_test_lock();
        let mut b = Browser::new(
            Box::new(CountingHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (1892.0, 989.0),
        );
        b.navigate(START_URL);
        let dl = b.last_display_list().expect("start page published");

        // Full-bleed hero: with the UA body padding reset the gradient
        // header starts flush at x=0 and covers the whole first window.
        let hero = dl
            .rects
            .iter()
            .find(|r| {
                r.w > 1500.0 && (r.h - 989.0).abs() < 2.0 && r.color & 0xffffff00 == 0x18182600
            })
            .unwrap_or_else(|| panic!("no full-window hero rect (gradient first stop #181826)"));
        assert!(hero.x.abs() < 0.5, "hero flush left, got x={}", hero.x);

        // The background image rides the hero's height AND its padding-box
        // origin: `inset: 0` inside the padded hero must cover the padding
        // too (CSS 10.1 containing block = padding box), so it sits at
        // (0, 0) — not offset by the hero's 24px horizontal padding.
        let bg = dl
            .images
            .iter()
            .find(|im| im.src.starts_with("data:image/qtsq;base64,"))
            .unwrap_or_else(|| panic!("hero QTSQ background missing from the display list"));
        assert!(
            bg.w > 1500.0 && (bg.h - 989.0).abs() < 2.0,
            "hero background must fill the hero, got {}x{}",
            bg.w,
            bg.h
        );
        assert!(
            bg.x.abs() < 0.5 && bg.y.abs() < 0.5,
            "hero background must cover the hero from (0,0), got ({}, {})",
            bg.x,
            bg.y
        );
        let payload = bg
            .payload
            .unwrap_or_else(|| panic!("hero background payload not stamped"));
        let raster = qtsq_raster_for_id(payload)
            .unwrap_or_else(|| panic!("stamped payload {payload} has no raster"));
        assert_eq!(
            (raster.width, raster.height),
            (2048, 1136),
            "decoded hero raster must be the embedded image's size"
        );

        // The hero copy is vertically centered in the window (flex
        // centering), not top-anchored: its first run must start well
        // below the old 118px top padding and land in the middle band of
        // the 989px viewport.
        let title = dl
            .texts
            .iter()
            .find(|t| t.text.starts_with("Kitae,"))
            .unwrap_or_else(|| panic!("hero title missing from the display list"));
        assert!(
            title.y > 250.0 && title.y < 550.0,
            "hero title should sit in the vertical middle band, got y={}",
            title.y
        );
    }

    /// The two content windows must be organized, spaced and full-width
    /// (user report: overlapping key chips, a narrow 640px column, section
    /// content spilling past its own window, footer floating detached):
    /// ① key chips and their descriptions sit side by side without
    /// overlapping, ② the two topic cards share one row across the wide
    /// story column, ③ the how-it-works card's last line ends inside the
    /// second window, and ④ the site footer anchors to the bottom of the
    /// third (FAQ) window.
    #[test]
    fn start_page_sections_use_the_window_without_overlapping() {
        let mut b = Browser::new(
            Box::new(CountingHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (1892.0, 989.0),
        );
        b.navigate(START_URL);
        let dl = b.last_display_list().expect("start page published");
        let vh = 989.0f32;

        // ① The ⌘K chip (small dark keycap rect in window 2) and the first
        // word of its description: no overlap, the text starts at or after
        // the chip's right edge.
        let key = dl
            .rects
            .iter()
            .find(|r| {
                r.w < 120.0
                    && r.h < 60.0
                    && r.y > vh
                    && r.y < 2.0 * vh
                    && r.color & 0xffffff00 == 0x20202B00
            })
            .unwrap_or_else(|| panic!("⌘K keycap rect missing from window 2"));
        let cmd = dl
            .texts
            .iter()
            .find(|t| t.text == "Command")
            .unwrap_or_else(|| panic!("key description text missing"));
        assert!(
            cmd.x >= key.x + key.w - 0.5,
            "key description overlaps the chip (chip ends at {}, text at {})",
            key.x + key.w,
            cmd.x
        );

        // ② Both topic cards paint their background in window 2, on one row
        // side by side, each wide enough to count as full-window use.
        let cards: Vec<_> = dl
            .rects
            .iter()
            .filter(|r| {
                r.w > 450.0
                    && r.h > 250.0
                    && r.y > vh
                    && r.y < 2.0 * vh
                    && r.color & 0xffffff00 == 0x12121A00
            })
            .collect();
        assert_eq!(cards.len(), 2, "two topic card backgrounds expected");
        assert!(
            (cards[0].y - cards[1].y).abs() < 2.0,
            "cards must share a row, got y {} vs {}",
            cards[0].y,
            cards[1].y
        );
        assert!(
            (cards[0].x - cards[1].x).abs() > 300.0,
            "cards must sit side by side, got x {} vs {}",
            cards[0].x,
            cards[1].x
        );
        assert!(
            cards[0].w > 500.0 && cards[1].w > 500.0,
            "cards must use the wide column"
        );

        // ③ The how-it-works card's last line stays inside window 2 — no
        // spill into the FAQ window below.
        let last = dl
            .texts
            .iter()
            .find(|t| t.text == "terminal.")
            .unwrap_or_else(|| panic!("last how-it-works note missing"));
        assert!(
            last.y < 2.0 * vh,
            "section 2 content overflows its window (last line y={})",
            last.y
        );

        // ④ The footer anchors to the bottom of the third (FAQ) window.
        let tag = dl
            .texts
            .iter()
            .find(|t| t.text == "noise.")
            .unwrap_or_else(|| panic!("footer tagline missing"));
        assert!(
            tag.y > 3.0 * vh - 160.0 && tag.y < 3.0 * vh,
            "footer must anchor to the FAQ window bottom, got y={}",
            tag.y
        );
    }

    /// Clicking the demo button routes through the component's handler,
    /// re-renders via one SetText patch, and splices without a full-page
    /// relayout: the repaint counter stays put while the spliced display
    /// list carries the new text.
    #[test]
    fn click_routes_into_component_and_splices() {
        let mut b = Browser::new(
            Box::new(CountingHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.navigate(START_URL);
        assert_eq!(b.repaint_count(), 1, "initial mount = one full layout");
        let height_before = b.content_height();

        // The demo button's label "Count: 0" is a text run in the display
        // list; clicking inside its rect routes up to the <button> ref.
        let dl = b.last_display_list().expect("start page published a frame");
        let btn_text = dl
            .texts
            .iter()
            .find(|t| t.text.contains("Count: 0"))
            .expect("demo button label is in the display list")
            .clone();
        b.click(btn_text.x + 2.0, btn_text.y - 2.0);

        // The splice proof: repaint counter unchanged, "Count: 1" painted.
        assert_eq!(
            b.repaint_count(),
            1,
            "signal change splices: no full relayout"
        );
        let dl = b.last_display_list().expect("spliced frame published");
        assert!(
            dl.texts.iter().any(|t| t.text.contains("Count: 1")),
            "the spliced display list shows Count: 1"
        );
        // Runs are word-granular; "without the noise" is three runs.
        assert!(
            dl.texts.iter().any(|t| t.text == "without")
                && dl.texts.iter().any(|t| t.text == "noise."),
            "the rest of the start page survived the splice"
        );
        assert!(
            (b.content_height() - height_before).abs() < 2.0,
            "a text swap is height-neutral"
        );

        // Splice ≡ full layout: force a real relayout and compare the
        // produced document at the SAME width. Resizing to a different
        // width alone would shift the hero's wrap and, with it, the clip
        // boundary — the fixed 660px hero clips the sub's trailing lines
        // at narrow widths — so edge words could be clipped in one frame
        // and visible in the other purely from the width delta.
        let spliced: Vec<String> = b
            .last_display_list()
            .unwrap()
            .texts
            .iter()
            .map(|t| t.text.clone())
            .collect();
        b.resize(410.0, 600.0);
        assert_eq!(b.repaint_count(), 2, "width change dirties a full layout");
        b.resize(400.0, 600.0);
        let full: Vec<String> = b
            .last_display_list()
            .unwrap()
            .texts
            .iter()
            .map(|t| t.text.clone())
            .collect();
        assert_eq!(spliced, full, "the splice matches a full relayout");
    }

    #[test]
    fn subtree_height_probe_matches_full_layout() {
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        // about:start has no #probe; inject a page via file-less prepare path:
        // navigate to a data-less blank then replace via direct load of start?
        // Instead drive the pipeline directly: prepare a div page and check
        // subtree vs full geometry through layout_subtree.
        b.navigate("about:blank");
        let html = "<html><body><div id='probe'><p>hello world</p></div></body></html>";
        let (dom, _, styles) = prepare_in(html, "", 400.0);
        let full = kitae_layout::layout(&dom, &styles, 400.0, 600.0, &ApproxMeasurer);
        let target = dom.get_element_by_id("probe").unwrap();
        let sub =
            kitae_layout::layout_subtree(&dom, &styles, target, 368.0, 600.0, &ApproxMeasurer);
        assert!(
            (full.content_height - (sub.content_height + 32.0)).abs() < 2.0,
            "body padding 16+16 wraps the div: full={} sub={}",
            full.content_height,
            sub.content_height
        );
        // Browser-level probe returns Some once a real page with the id loads.
        // (about:blank has no element, so None here proves the miss path.)
        assert_eq!(b.subtree_height("probe"), None);
    }

    fn test_browser(html: &str) -> Browser {
        test_browser_at(html, "https://example.com/")
    }

    /// The same helper with an explicit base URL — tests whose fixtures live
    /// on local disk need a local page origin (the S-1 gate only lets local
    /// pages fetch local files).
    fn test_browser_at(html: &str, base: &str) -> Browser {
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(html, base);
        b
    }

    /// A local page origin for tests that fetch `file://` fixtures. Never
    /// fetched itself — pure origin context for the S-1 gate.
    fn local_base_url() -> String {
        "file:///kitae-test-local/page.html".to_string()
    }

    #[test]
    fn sheet_level_media_queries_see_the_viewport_height() {
        // A 400x600 viewport is portrait; a 400x300 one is landscape.
        let portrait = test_browser_at(
            "<html><head><style>\
             @media (orientation: portrait) { p { color: #112233 } }\
             @media (min-height: 500px) { b { color: #445566 } }\
             @media (height < 400px) { i { color: #778899 } }\
             </style></head><body><p>p</p><b>b</b><i>i</i></body></html>",
            &local_base_url(),
        );
        let p = find_style(&portrait, "p");
        assert_eq!(
            p.color, 0x11_22_33_ff,
            "portrait query applies in a 400x600 viewport"
        );
        let b = find_style(&portrait, "b");
        assert_eq!(b.color, 0x44_55_66_ff, "min-height query applies at 600px");
        let i = find_style(&portrait, "i");
        assert_eq!(
            i.color, 0x1A1D23FF,
            "height range query does not apply at 600px (UA ink left alone)"
        );
    }

    #[test]
    fn style_attribute_media_gating_sees_the_viewport_height() {
        let b = test_browser_at(
            "<html><head>\
             <style media=\"(orientation: landscape)\">p { color: #0a0b0c }</style>\
             <style media=\"(orientation: portrait)\">b { color: #0d0e0f }</style>\
             </style></head><body><p>p</p><b>b</b></body></html>",
            &local_base_url(),
        );
        let p = find_style(&b, "p");
        assert_eq!(
            p.color, 0x1A1D23FF,
            "landscape sheet is skipped in a portrait viewport (UA ink left alone)"
        );
        let bb = find_style(&b, "b");
        assert_eq!(bb.color, 0x0d_0e_0f_ff, "portrait sheet applies");
    }

    #[test]
    fn import_media_queries_see_the_viewport_height() {
        let dir = std::env::temp_dir().join(format!("kitae_mqimport_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("tall.css"), "p { color: #0f0e0d }").expect("tall");
        std::fs::write(dir.join("short.css"), "b { color: #0c0b0a }").expect("short");
        let base = format!("file://{}/page.html", dir.display());
        let css = format!(
            "@import url(tall.css) (min-height: 500px);\
             @import url(short.css) (max-height: 300px);"
        );
        let sheets = load_stylesheets(
            &[
                StyleSource::Inline(css),
                StyleSource::Inline("i { color: #000000 }".to_string()),
            ],
            &base,
        );
        // The height-less public entry point keeps the historical behavior:
        // height features are unknown, so only the unconditional sheet lands.
        assert_eq!(sheets.len(), 1, "height-gated imports stay out: {sheets:?}");

        // With an explicit viewport box the height features resolve.
        let (sheets, _) = load_stylesheets_in_env(
            &[
                StyleSource::Inline(format!(
                    "@import url(tall.css) (min-height: 500px);\
                     @import url(short.css) (max-height: 300px);"
                )),
                StyleSource::Inline("i { color: #000000 }".to_string()),
            ],
            &base,
            &base,
            400.0,
            600.0,
            &EnvironmentState::default(),
            MAX_EXTERNAL_STYLESHEETS,
            MAX_EXTERNAL_STYLESHEET_BYTES,
        );
        assert_eq!(
            sheets.len(),
            2,
            "only the matching import loads: {sheets:?}"
        );
        assert!(sheets[0].contains("0f0e0d"), "tall import loaded");
        assert!(
            sheets[1].contains("000000"),
            "unconditional sheet still loads"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The computed style of the first element with `tag`, by tag name.
    fn find_style(b: &Browser, tag: &str) -> ComputedStyle {
        let page = b.page.as_ref().expect("page");
        let mut found = None;
        page.dom.walk(page.dom.root().expect("root"), &mut |id| {
            if found.is_none() && page.dom.tag(id) == tag {
                found = page
                    .styles
                    .iter()
                    .find(|(n, _)| *n == id)
                    .map(|(_, s)| s.clone());
            }
        });
        found.expect("element with tag")
    }

    // ── Web raster plumbing (K3) ──

    fn web_fixture_url(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("kitae_media")
            .join("testdata")
            .join(name);
        format!("file://{}", path.display())
    }

    #[test]
    fn web_jpeg_resolves_to_payload_raster() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let src = web_fixture_url("baseline.jpg");
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(
            &format!("<html><body><p><img src=\"{src}\" alt=\"base\"></p></body></html>"),
            &local_base_url(),
        );
        let dl = b.last_dl.clone().expect("frame published");
        assert_eq!(dl.images.len(), 1);
        let payload = dl.images[0].payload.expect("web jpeg carries a payload id");
        assert_ne!(payload, 0);
        // Pull path (the frame side-table on the host side) resolves it.
        let raster = qtsq_raster_for_id(payload).expect("payload resolves");
        assert_eq!((raster.width, raster.height), (16, 16));
        assert_eq!(raster.pixels.len(), 16 * 16 * 4);
        // On-demand hook agrees (scroll-window fetching seam).
        let again = b.raster_for(&src).expect("raster_for");
        assert_eq!(again.id, payload);
        assert!(b.raster_len() >= 1);
        clear_qtsq_rasters();
    }

    // --- I3: purgeable raster caches (byte budget + pressure purge) ---

    #[test]
    fn raster_byte_budget_evicts_oldest_first() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        set_raster_cache_byte_budget(600);
        let a = qtsq_register_raster("kitae_i3_a", 10, 10, vec![0; 400]);
        let b = qtsq_register_raster("kitae_i3_b", 10, 10, vec![1; 400]);
        // 800 decoded bytes against a 600-byte budget: the older raster is
        // the one that goes.
        assert!(
            qtsq_raster_for_url("kitae_i3_a").is_none(),
            "oldest evicted"
        );
        assert!(
            qtsq_raster_for_url("kitae_i3_b").is_some(),
            "newest retained"
        );
        assert_eq!(b.id, a.id + 1);
        let (entries, bytes) = raster_cache_stats();
        assert_eq!((entries, bytes), (1, 400));
        clear_qtsq_rasters();
        set_raster_cache_byte_budget(RASTER_CACHE_MAX_BYTES);
    }

    #[test]
    fn raster_re_register_replaces_bytes_not_adds() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        set_raster_cache_byte_budget(RASTER_CACHE_MAX_BYTES);
        let first = qtsq_register_raster("kitae_i3_re", 10, 10, vec![0; 400]);
        qtsq_register_raster("kitae_i3_re", 100, 100, vec![0; 40_000]);
        let again = qtsq_raster_for_url("kitae_i3_re").expect("still cached");
        assert_eq!(again.id, first.id, "re-registration keeps the payload id");
        let (entries, bytes) = raster_cache_stats();
        assert_eq!((entries, bytes), (1, 40_000));
        clear_qtsq_rasters();
    }

    #[test]
    fn lone_oversized_raster_survives_the_budget() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        set_raster_cache_byte_budget(100);
        qtsq_register_raster("kitae_i3_big", 100, 100, vec![0; 40_000]);
        // A single raster above the budget is still the one in use — the
        // store keeps exactly it rather than evicting everything.
        assert!(qtsq_raster_for_url("kitae_i3_big").is_some());
        assert_eq!(raster_cache_stats(), (1, 40_000));
        clear_qtsq_rasters();
        set_raster_cache_byte_budget(RASTER_CACHE_MAX_BYTES);
    }

    #[test]
    fn purge_image_caches_drops_rasters_and_bytes() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        qtsq_register_raster("kitae_i3_purge", 8, 8, vec![7; 256]);
        assert!(qtsq_raster_for_url("kitae_i3_purge").is_some());
        purge_image_caches();
        assert!(qtsq_raster_for_url("kitae_i3_purge").is_none(), "purged");
        assert_eq!(raster_cache_stats(), (0, 0));
        // Re-decode on demand: registering again yields a working raster.
        let back = qtsq_register_raster("kitae_i3_purge", 8, 8, vec![7; 256]);
        assert_eq!(back.pixels.len(), 256);
        clear_qtsq_rasters();
    }

    /// Q8: writes a 2-frame animated GIF fixture (frame 1 red, frame 2 blue,
    /// looping forever) and returns its file:// URL.
    fn animated_gif_fixture_url(tag: &str) -> String {
        let mut out = Vec::new();
        let mut enc = gif::Encoder::new(&mut out, 4, 4, &[0xFF, 0, 0, 0, 0, 0xFF, 0, 0, 0xFF])
            .expect("gif encoder"); // palette: 0 = red, 1 = blue
        enc.set_repeat(gif::Repeat::Infinite).expect("set_repeat");
        for idx in [0u8, 1u8] {
            let frame = gif::Frame::from_indexed_pixels(4, 4, vec![idx; 16], None);
            enc.write_frame(&frame).expect("write frame");
        }
        drop(enc);
        let path =
            std::env::temp_dir().join(format!("kitae_q8_{}_{}.gif", std::process::id(), tag));
        std::fs::write(&path, out).expect("write gif fixture");
        format!("file://{}", path.display())
    }

    #[test]
    fn web_gif_registers_animation_and_stamps_display_image() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let src = animated_gif_fixture_url("anim");
        let b = test_browser_at(
            &format!("<html><body><p><img src=\"{src}\" alt=\"spin\"></p></body></html>"),
            &local_base_url(),
        );
        let dl = b.last_dl.clone().expect("frame published");
        assert_eq!(dl.images.len(), 1);
        let im = &dl.images[0];
        let payload = im.payload.expect("animated gif carries a payload id");
        assert_ne!(payload, 0);
        assert!(im.animated, "engine stamps multi-frame gif as animated");
        // The still raster (frame 0) is in the ordinary store; the full
        // animation is in the side-table, keyed by the same id.
        let raster = qtsq_raster_for_id(payload).expect("frame-0 still resolves");
        assert_eq!((raster.width, raster.height), (4, 4));
        let anim = qtsq_animation_for_id(payload).expect("animation registered");
        assert_eq!(anim.id, payload);
        assert_eq!(anim.frames.len(), 2, "both frames decoded");
        assert_eq!(anim.width, 4);
        assert_eq!(anim.height, 4);
        assert_eq!(anim.loop_count, Some(u16::MAX), "NETSCAPE infinite");
        assert_eq!(
            anim.frames[0].delay_ms,
            kitae_media::MIN_FRAME_DELAY_MS,
            "delay floored at 20ms"
        );
        assert!(
            anim.frames[0].pixels.iter().all(|&b| b == 0xFF || b == 0),
            "frame 0 is solid red"
        );
        assert!(anim.frames[1].pixels[0] == 0, "frame 1 red channel off");
        assert_eq!(anim.frames[1].pixels[2], 0xFF, "frame 1 blue channel on");
        clear_qtsq_rasters();
    }

    #[test]
    fn web_single_frame_gif_is_a_still_not_an_animation() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        // One frame, no NETSCAPE extension: decodes as an ordinary still.
        let mut out = Vec::new();
        let mut enc =
            gif::Encoder::new(&mut out, 4, 4, &[1, 0, 0, 0, 1, 0, 0, 0, 1]).expect("gif encoder");
        let frame = gif::Frame::from_indexed_pixels(4, 4, vec![0u8; 16], None);
        enc.write_frame(&frame).expect("write frame");
        drop(enc);
        let path = std::env::temp_dir().join(format!("kitae_q8_still_{}.gif", std::process::id()));
        std::fs::write(&path, out).expect("write gif fixture");
        let src = format!("file://{}", path.display());

        let b = test_browser_at(
            &format!("<html><body><p><img src=\"{src}\" alt=\"still\"></p></body></html>"),
            &local_base_url(),
        );
        let dl = b.last_dl.clone().expect("frame published");
        assert_eq!(dl.images.len(), 1);
        let payload = dl.images[0]
            .payload
            .expect("still gif carries a payload id");
        assert!(!dl.images[0].animated, "single-frame gif is not animated");
        assert!(qtsq_raster_for_id(payload).is_some(), "still resolves");
        assert!(
            qtsq_animation_for_id(payload).is_none(),
            "no side-table entry"
        );
        clear_qtsq_rasters();
    }

    /// Build a 2-frame APNG (frame 0 is the default image, frame 1 an
    /// `fdAT` overlay) and return its file:// URL.
    fn apng_fixture_url(tag: &str) -> String {
        use flate2::{write::ZlibEncoder, Compression};
        use std::io::Write;

        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let mut crc = flate2::Crc::new();
            crc.update(kind);
            crc.update(data);
            out.extend_from_slice(&crc.sum().to_be_bytes());
        }
        fn idat(px: &[u8]) -> Vec<u8> {
            let mut raw = Vec::new();
            for row in px.chunks_exact(8) {
                raw.push(0u8);
                raw.extend_from_slice(row);
            }
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(&raw).unwrap();
            enc.finish().unwrap()
        }
        let fctl = |seq: u32| {
            let mut d = Vec::new();
            d.extend_from_slice(&seq.to_be_bytes());
            d.extend_from_slice(&2u32.to_be_bytes()); // w
            d.extend_from_slice(&2u32.to_be_bytes()); // h
            d.extend_from_slice(&0u32.to_be_bytes()); // x
            d.extend_from_slice(&0u32.to_be_bytes()); // y
            d.extend_from_slice(&1u16.to_be_bytes()); // delay num
            d.extend_from_slice(&10u16.to_be_bytes()); // delay den → 100ms
            d.push(0); // dispose: none
            d.push(0); // blend: source
            d
        };
        let solid = |c: [u8; 4]| [c; 4].concat();
        let mut out = Vec::new();
        out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(
            &mut out,
            b"acTL",
            &[2u32.to_be_bytes(), 0u32.to_be_bytes()].concat(),
        );
        chunk(&mut out, b"fcTL", &fctl(0));
        chunk(&mut out, b"IDAT", &idat(&solid([0xFF, 0, 0, 0xFF])));
        chunk(&mut out, b"fcTL", &fctl(2));
        let mut fdat = 3u32.to_be_bytes().to_vec();
        fdat.extend_from_slice(&idat(&solid([0, 0, 0xFF, 0xFF])));
        chunk(&mut out, b"fdAT", &fdat);
        chunk(&mut out, b"IEND", &[]);
        let path =
            std::env::temp_dir().join(format!("kitae_apng_{}_{}.png", std::process::id(), tag));
        std::fs::write(&path, out).expect("write apng fixture");
        format!("file://{}", path.display())
    }

    #[test]
    fn web_apng_registers_animation_and_stamps_display_image() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let src = apng_fixture_url("anim");
        let b = test_browser_at(
            &format!("<html><body><p><img src=\"{src}\" alt=\"spin\"></p></body></html>"),
            &local_base_url(),
        );
        let dl = b.last_dl.clone().expect("frame published");
        assert_eq!(dl.images.len(), 1);
        let im = &dl.images[0];
        let payload = im.payload.expect("apng carries a payload id");
        assert!(im.animated, "multi-frame apng is stamped animated");
        let raster = qtsq_raster_for_id(payload).expect("frame-0 still resolves");
        assert_eq!((raster.width, raster.height), (2, 2));
        let anim = qtsq_animation_for_id(payload).expect("animation registered");
        assert_eq!(anim.frames.len(), 2, "both frames decoded");
        assert_eq!(anim.loop_count, Some(u16::MAX), "num_plays 0 = forever");
        assert_eq!(anim.frames[0].delay_ms, 100, "1/10s delay");
        assert_eq!(anim.frames[0].pixels[0], 0xFF, "frame 0 red");
        assert_eq!(anim.frames[1].pixels[0], 0, "frame 1 blue");
        assert_eq!(anim.frames[1].pixels[2], 0xFF);
        clear_qtsq_rasters();
    }

    /// Q9: writes a 4x4 solid-red lossless WebP and returns its file:// URL.
    fn webp_fixture_url(tag: &str) -> String {
        let mut out = Vec::new();
        let px: Vec<u8> = (0..4 * 4).flat_map(|_| [0xE0u8, 0x20, 0x20]).collect();
        image_webp::WebPEncoder::new(&mut out)
            .encode(&px, 4, 4, image_webp::ColorType::Rgb8)
            .expect("webp encode");
        let path =
            std::env::temp_dir().join(format!("kitae_q9_{}_{}.webp", std::process::id(), tag));
        std::fs::write(&path, out).expect("write webp fixture");
        format!("file://{}", path.display())
    }

    /// A 4x2 lossless WebP (non-square, so `cover` vs `contain` vs `auto`
    /// all differ observably) written to a file:// URL.
    fn wide_webp_fixture_url(tag: &str) -> String {
        let mut out = Vec::new();
        let px: Vec<u8> = (0..4 * 2).flat_map(|_| [0x30u8, 0x60, 0x90]).collect();
        image_webp::WebPEncoder::new(&mut out)
            .encode(&px, 4, 2, image_webp::ColorType::Rgb8)
            .expect("webp encode");
        let path =
            std::env::temp_dir().join(format!("kitae_bg_{}_{}.webp", std::process::id(), tag));
        std::fs::write(&path, out).expect("write webp fixture");
        format!("file://{}", path.display())
    }

    #[test]
    fn background_cover_contain_and_auto_use_the_artwork_intrinsic_size() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let src = wide_webp_fixture_url("sizes");
        let b = test_browser_at(
            &format!(
                "<html><head><style>\
                 .cover {{ width: 100px; height: 100px; background-image: url({src});\
                          background-size: cover; background-repeat: no-repeat }}\
                 .contain {{ width: 100px; height: 100px; background-image: url({src});\
                             background-size: contain; background-repeat: no-repeat }}\
                 .auto {{ width: 100px; height: 100px; background-image: url({src});\
                          background-size: auto; background-repeat: no-repeat }}\
                 </style></head><body><div class='cover'></div>\
                 <div class='contain'></div><div class='auto'></div></body></html>"
            ),
            &local_base_url(),
        );
        let dl = b.last_dl.clone().expect("frame published");
        let tiles: Vec<_> = dl
            .images
            .iter()
            .filter(|im| im.src.contains("kitae_bg_"))
            .collect();
        assert_eq!(tiles.len(), 3, "one tile per background box: {tiles:?}");
        // 4x2 artwork in a 100x100 box: cover fills (100x100), contain fits
        // (100x50), auto keeps the intrinsic size (4x2).
        let mut sizes: Vec<(f32, f32)> = tiles.iter().map(|t| (t.w, t.h)).collect();
        sizes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        // cover scales until both axes are covered, so the 4:2 artwork
        // overflows the 100x100 box to 200x100 (clipped by the box).
        assert!(
            (sizes[0].0 - 200.0).abs() < 0.5 && (sizes[0].1 - 100.0).abs() < 0.5,
            "cover fills the box, got {:?}",
            sizes[0]
        );
        assert!(
            (sizes[1].0 - 100.0).abs() < 0.5 && (sizes[1].1 - 50.0).abs() < 0.5,
            "contain fits inside, got {:?}",
            sizes[1]
        );
        assert!(
            (sizes[2].0 - 4.0).abs() < 0.5 && (sizes[2].1 - 2.0).abs() < 0.5,
            "auto keeps intrinsic, got {:?}",
            sizes[2]
        );
        assert!(
            tiles.iter().all(|t| t.payload.is_some()),
            "each tile carries the decoded payload"
        );
        clear_qtsq_rasters();
    }

    #[test]
    fn web_webp_resolves_to_payload_raster() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let src = webp_fixture_url("still");
        let b = test_browser_at(
            &format!("<html><body><p><img src=\"{src}\" alt=\"wp\"></p></body></html>"),
            &local_base_url(),
        );
        let dl = b.last_dl.clone().expect("frame published");
        assert_eq!(dl.images.len(), 1);
        assert!(!dl.images[0].animated, "webp is a still (Q9 scope)");
        let payload = dl.images[0].payload.expect("webp carries a payload id");
        let raster = qtsq_raster_for_id(payload).expect("payload resolves");
        assert_eq!((raster.width, raster.height), (4, 4));
        assert_eq!(raster.pixels.len(), 4 * 4 * 4);
        // Solid red: RGB fixture expands to opaque RGBA.
        for px in raster.pixels.chunks_exact(4) {
            assert_eq!(px, [0xE0, 0x20, 0x20, 0xFF]);
        }
        clear_qtsq_rasters();
    }

    /// Q10: a file:// SVG `<img>` resolves to a payload raster rendered at
    /// the layout box size (not the SVG's intrinsic size), with straight
    /// alpha and the fill color at the center.
    #[test]
    fn web_svg_resolves_to_payload_raster_at_layout_size() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 8 8"><rect width="8" height="8" fill="#0a1df5"/></svg>"##;
        let path = std::env::temp_dir().join(format!("kitae_q10_{}.svg", std::process::id()));
        std::fs::write(&path, svg).expect("write svg fixture");
        let src = format!("file://{}", path.display());
        let b = test_browser_at(&format!(
            "<html><body><p><img src=\"{src}\" width=\"32\" height=\"16\" alt=\"svg\"></p></body></html>"), &local_base_url());
        let dl = b.last_dl.clone().expect("frame published");
        assert_eq!(dl.images.len(), 1);
        assert!(!dl.images[0].animated, "svg is a still");
        let payload = dl.images[0].payload.expect("svg carries a payload id");
        let raster = qtsq_raster_for_id(payload).expect("payload resolves");
        // The raster matches the layout box, not the 8x8 intrinsic size.
        let (bw, bh) = (dl.images[0].w.ceil() as u32, dl.images[0].h.ceil() as u32);
        assert!(
            (bw, bh) == (32, 16),
            "layout box honors the img attrs: {bw}x{bh}"
        );
        assert_eq!(
            (raster.width, raster.height),
            (bw, bh),
            "raster rendered at box size"
        );
        assert_eq!(raster.pixels.len(), (bw * bh * 4) as usize);
        // Center pixel is the fill color, opaque straight alpha.
        let cx = ((bh / 2) * bw + bw / 2) as usize * 4;
        assert_eq!(&raster.pixels[cx..cx + 4], &[0x0A, 0x1D, 0xF5, 0xFF]);
        clear_qtsq_rasters();
    }

    #[test]
    fn web_raster_decode_failure_keeps_placeholder() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let mut b = test_browser("<html><body><p><img src=\"https://example.com/broken.jpg\" alt=\"x\"></p></body></html>");
        let dl = b.last_dl.clone().expect("frame published");
        assert_eq!(dl.images.len(), 1);
        // example.com is unreachable from here (or 404): no payload, but the
        // alt placeholder the layout painted is intact.
        assert_eq!(dl.images[0].payload, None);
        assert!(dl.texts.iter().any(|t| t.text.contains('x')));
        clear_qtsq_rasters();
    }

    #[test]
    fn raster_store_evicts_oldest_past_cap() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        for i in 0..RASTER_CACHE_MAX + 5 {
            qtsq_register_raster(&format!("https://x/{i}.png"), 1, 1, vec![0, 0, 0, 255]);
        }
        let store = qtsq_raster_store().lock().unwrap();
        assert_eq!(store.by_url.len(), RASTER_CACHE_MAX);
        assert!(
            !store.by_url.contains_key("https://x/0.png"),
            "oldest evicted"
        );
        assert!(store
            .by_url
            .contains_key(&format!("https://x/{}.png", RASTER_CACHE_MAX + 4)));
        drop(store);
        clear_qtsq_rasters();
    }

    // ── QTSQ viewer pages (J2) ──

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("kitae_qtsq")
                .join("testdata")
                .join(name),
        )
        .expect("fixture file")
    }

    fn url_ending(name: &str) -> String {
        format!("file:///Users/example/{}", name)
    }

    fn write_temp_fixture(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("kitae_j2_{}_{}", std::process::id(), name));
        std::fs::write(&path, fixture(name)).expect("write fixture");
        format!("file://{}", path.display())
    }

    #[test]
    fn qtsq_image_navigation_shows_pixels_and_raster_cache() {
        let _store = crate::qtsq_test_lock();
        let url = write_temp_fixture("fix_image.qtsq");
        let page = fetch_step(&url);
        assert!(
            page.0.contains("<img"),
            "image payload renders as img: {}",
            page.0
        );
        assert!(
            page.0.contains("width=\"8\" height=\"8\""),
            "intrinsic dims: {}",
            page.0
        );
        assert!(
            page.0.contains("fix_image.qtsq"),
            "title is the filename: {}",
            page.0
        );
        // The decoded raster is registered under the final URL for the
        // shell/renderer to resolve.
        let raster = qtsq_raster_for_url(&url).expect("raster registered");
        assert_eq!(raster.width, 8);
        assert_eq!(raster.height, 8);
        assert_eq!(raster.pixels.len(), (8 * 8 * 4) as usize);
        // First pixel: (1, 2, 3, 255) per the fixture generator.
        assert_eq!(&raster.pixels[0..4], &[1, 2, 3, 255]);
        clear_qtsq_rasters();
    }

    #[test]
    fn page_img_qtsq_resolves_to_payload_raster() {
        let _store = crate::qtsq_test_lock();
        let dir = std::env::temp_dir().join(format!("kitae_j3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("photo.qtsq"), fixture("fix_image.qtsq")).expect("write img");
        let page_url = format!("file://{}/page.html", dir.display());
        let img_url = format!("file://{}/photo.qtsq", dir.display());
        let html = format!(
            "<html><head><title>raster page</title></head>\
<body><p><img src=\"photo.qtsq\" alt=\"photo\"></p></body></html>"
        );
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.load_html(&html, &page_url);
        let dl = b.last_display_list().expect("display list");
        assert_eq!(dl.images.len(), 1, "the img laid out");
        let payload = dl.images[0]
            .payload
            .expect("QTSQ subresource resolved to a payload id");
        // The id resolves through the frame side-table with the fixture's
        // known pixels (first pixel (1, 2, 3, 255), 8×8).
        let raster = qtsq_raster_for_id(payload).expect("payload id in the side-table");
        assert_eq!((raster.width, raster.height), (8, 8));
        assert_eq!(&raster.pixels[0..4], &[1, 2, 3, 255]);
        assert!(qtsq_raster_for_url(&img_url).is_some(), "cached by URL too");

        // A second img of the same file shares the id; a non-QTSQ src stays
        // placeholder (payload None) — no fetch attempt panics or hangs.
        let html2 = format!(
            "<body><img src=\"photo.qtsq\" alt=\"a\"><img src=\"pic.png\" alt=\"b\"></body>"
        );
        b.load_html(&html2, &page_url);
        let dl = b.last_display_list().expect("second display list");
        assert_eq!(
            dl.images[0].payload,
            Some(payload),
            "same URL → same payload id"
        );
        assert_eq!(
            dl.images[1].payload, None,
            "web image stays placeholder in J3"
        );

        clear_qtsq_rasters();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn qtsq_text_navigation_decodes_content() {
        let url = write_temp_fixture("fix_text.qtsq");
        let page = fetch_step(&url);
        assert!(
            page.0.contains("Hello from QTSQ!"),
            "decoded text on the page: {}",
            page.0
        );
        assert!(page.0.contains("<pre>"), "text wraps in <pre>: {}", page.0);
        assert!(page.0.contains("fix_text.qtsq"), "title is the filename");
    }

    #[test]
    fn qtsq_audio_navigation_shows_media_viewer() {
        let _store = crate::qtsq_test_lock();
        let url = write_temp_fixture("fix_audio.qtsq");
        let page = fetch_step(&url);
        assert!(
            page.0.contains("data-kind=\"audio\""),
            "media marker: {}",
            page.0
        );
        assert!(
            page.0.contains("22050"),
            "sample rate in metadata: {}",
            page.0
        );
        assert!(page.0.contains("Fourier"), "strategy name: {}", page.0);
        assert!(page.0.contains("fix_audio.qtsq"), "title is the filename");
    }

    #[test]
    fn qtsq_video_navigation_shows_media_viewer() {
        let _store = crate::qtsq_test_lock();
        let url = write_temp_fixture("fix_video.qtsq");
        let page = fetch_step(&url);
        assert!(
            page.0.contains("data-kind=\"video\""),
            "media marker: {}",
            page.0
        );
        assert!(page.0.contains("12"), "frame rate in metadata: {}", page.0);
        assert!(page.0.contains("Temporal"), "strategy name: {}", page.0);
    }

    // ── Media handoff (J4) ──

    /// Parse a CAF header the way AVAudioReader would: magic, desc chunk
    /// (sample rate, lpcm LE int16), data chunk with the right length.
    /// Container fields are big-endian per the CAF spec.
    fn assert_caf_parses(
        path: &str,
        expected_rate: u32,
        expected_channels: u8,
        expected_frames: usize,
    ) {
        let raw = std::fs::read(path).expect("CAF file exists");
        assert_eq!(&raw[0..4], b"caff", "CAF magic");
        let mut p = 8; // skip version + flags
        assert_eq!(&raw[p..p + 4], b"desc", "desc chunk first");
        p += 4;
        let desc_len = u64::from_be_bytes(raw[p..p + 8].try_into().unwrap()) as usize;
        p += 8;
        assert_eq!(desc_len, 32, "audio description is 32 bytes");
        let rate = f64::from_be_bytes(raw[p..p + 8].try_into().unwrap());
        assert_eq!(rate as u32, expected_rate, "sample rate in desc");
        assert_eq!(&raw[p + 8..p + 12], b"lpcm", "lpcm format id");
        let flags = u32::from_be_bytes(raw[p + 12..p + 16].try_into().unwrap());
        // kCAFLinearPCMFormatFlagIsLittleEndian — samples are LE int16.
        assert_eq!(flags, 2, "little-endian int16 lpcm flags");
        let bytes_per_packet = u32::from_be_bytes(raw[p + 16..p + 20].try_into().unwrap()) as usize;
        assert_eq!(
            bytes_per_packet,
            2 * expected_channels as usize,
            "packed frames"
        );
        p += desc_len;
        assert_eq!(&raw[p..p + 4], b"data", "data chunk after desc");
        p += 4;
        let data_len = u64::from_be_bytes(raw[p..p + 8].try_into().unwrap()) as usize;
        p += 8;
        assert_eq!(
            data_len,
            expected_frames * 2 * expected_channels as usize,
            "data chunk carries every sample"
        );
        assert_eq!(
            raw.len() - p - 4,
            data_len,
            "edit count + payload fill the file"
        );
        // First sample decodes to a plausible int16 from the 440 Hz sine.
        let first = i16::from_le_bytes(raw[p + 4..p + 6].try_into().unwrap());
        assert!(first.abs() < 32767, "in-range sample, got {first}");
    }

    #[test]
    fn qtsq_audio_navigation_publishes_media_info_and_caf() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        clear_qtsq_media();
        let url = write_temp_fixture("fix_audio.qtsq");
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.navigate(&url);
        let media = b.state().media.expect("audio page publishes MediaInfo");
        assert_eq!(media.url, url, "media keyed by the page URL");
        assert_eq!(media.kind, MediaKind::Audio);
        assert_eq!(media.rate, 22050, "fixture sample rate");
        assert_eq!(media.channels, 1, "mono fixture");
        assert!(
            (media.duration - 1.0).abs() < 0.01,
            "1.0s fixture, got {}",
            media.duration
        );
        assert!(!media.playing, "transport starts paused");
        assert_eq!(media.position, 0.0);
        assert!(media.caf_path.ends_with(".caf"), "CAF handoff path");
        // The handed-off CAF is a real file AVAudioPlayer can open.
        assert_caf_parses(&media.caf_path, 22050, 1, 22050);
        // Loading an HTML page clears the media and removes its handoff file.
        let caf_path = media.caf_path.clone();
        b.load_html("<h1>plain</h1>", "about:blank");
        assert!(b.state().media.is_none(), "plain page has no media");
        assert!(
            !std::path::Path::new(&caf_path).exists(),
            "navigating away removes the CAF handoff"
        );
        clear_qtsq_media();
    }

    #[test]
    fn media_play_transitions_mirror_through_state() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        clear_qtsq_media();
        let url = write_temp_fixture("fix_audio.qtsq");
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.navigate(&url);
        assert!(b.state().media.is_some());

        b.media_open();
        let m = b.state().media.expect("media after open");
        assert!(!m.playing, "open does not start playback");
        assert_eq!(m.position, 0.0);

        b.media_play(true, 0.0);
        let m = b.state().media.expect("media after play");
        assert!(m.playing, "play mirrors through");
        assert_eq!(m.position, 0.0);

        b.media_play(false, 0.5);
        let m = b.state().media.expect("media after pause");
        assert!(!m.playing);
        assert!(
            (m.position - 0.5).abs() < 1e-4,
            "seek/pause position mirrors"
        );

        b.media_play(false, 99.0);
        assert_eq!(b.state().media.expect("media after clamp").position, 1.0);
        b.media_play(false, f32::NAN);
        assert_eq!(b.state().media.expect("media after invalid").position, 0.0);

        // Reloading the page resets the transport to a fresh session.
        let old_caf = b.state().media.expect("media before reload").caf_path;
        b.reload();
        let m = b.state().media.expect("media after reload");
        assert!(!m.playing, "reload resets transport");
        assert_eq!(m.position, 0.0);
        assert!(m.caf_path.ends_with(".caf"), "CAF re-decoded on reload");
        assert_ne!(m.caf_path, old_caf, "reload uses a fresh CAF path");
        assert!(
            !std::path::Path::new(&old_caf).exists(),
            "reload removes the previous CAF handoff"
        );
        clear_qtsq_media();
    }

    #[test]
    fn qtsq_video_navigation_publishes_media_info() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        clear_qtsq_media();
        let url = write_temp_fixture("fix_video.qtsq");
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.navigate(&url);
        let media = b.state().media.expect("video page publishes MediaInfo");
        assert_eq!(media.kind, MediaKind::Video);
        assert!(
            (media.duration - 0.5).abs() < 0.01,
            "6 frames at 12 fps = 0.5s, got {}",
            media.duration
        );
        assert_eq!(media.rate, 0, "no audio rate for video");
        assert_eq!(media.caf_path, "", "no CAF handoff for video");
        clear_qtsq_media();
        clear_qtsq_videos();
    }

    #[test]
    fn qtsq_video_cache_is_bounded() {
        let _store = crate::qtsq_test_lock();
        let url = write_temp_fixture("fix_video.qtsq");
        let path = url.strip_prefix("file://").unwrap();
        let body = std::fs::read(path).expect("video fixture");
        clear_qtsq_videos();
        for i in 0..QTSQ_VIDEO_CACHE_MAX_ENTRIES {
            let decoder = kitae_qtsq::VideoDecoder::open(&body, None).expect("video decoder");
            qtsq_register_video(&format!("https://cache.example/{i}.qtsq"), &body, decoder);
        }
        let store = qtsq_video_store().lock().unwrap();
        assert_eq!(store.by_url.len(), QTSQ_VIDEO_CACHE_MAX_ENTRIES);
        assert!(store.bytes <= QTSQ_VIDEO_CACHE_MAX_BYTES || store.by_url.len() == 1);
        drop(store);

        let first = "https://cache.example/0.qtsq";
        {
            let mut store = qtsq_video_store().lock().unwrap();
            qtsq_touch_video(&mut store, first);
        }
        let decoder = kitae_qtsq::VideoDecoder::open(&body, None).expect("video decoder");
        qtsq_register_video("https://cache.example/recent.qtsq", &body, decoder);
        let store = qtsq_video_store().lock().unwrap();
        assert!(
            store.by_url.contains_key(first),
            "touched decoder survives eviction"
        );
        assert!(
            !store.by_url.contains_key("https://cache.example/1.qtsq"),
            "least-recent decoder is evicted before the touched one"
        );
        drop(store);
        clear_qtsq_videos();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn qtsq_video_navigation_emits_video_element() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        clear_qtsq_media();
        clear_qtsq_videos();
        let url = write_temp_fixture("fix_video.qtsq");
        let page = fetch_step(&url);
        assert!(page.0.contains("<video src="), "video element: {}", page.0);
        assert!(
            page.0.contains("width=\"16\" height=\"16\""),
            "decoder dims: {}",
            page.0
        );
        assert!(
            page.0.contains("Frame rate: 12 fps"),
            "frame rate metadata: {}",
            page.0
        );
        clear_qtsq_rasters();
        clear_qtsq_media();
        clear_qtsq_videos();
    }

    #[test]
    fn qtsq_video_frames_step_with_transport() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        clear_qtsq_media();
        clear_qtsq_videos();
        let url = write_temp_fixture("fix_video.qtsq");
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.navigate(&url);
        let dl = b.last_display_list().expect("display list").clone();
        assert_eq!(dl.media.len(), 1, "the <video> laid out as a media item");
        assert_eq!(dl.images.len(), 1, "the frame image rides along");
        assert!(dl.images[0].src.starts_with("\u{0}media:"), "sentinel src");
        assert_eq!(dl.media[0].state, (false, 0.0));
        b.media_play(true, 0.1);
        let live = b.last_display_list().expect("playing display list");
        assert_eq!(
            live.media[0].state,
            (true, 0.1),
            "play state reaches the media item"
        );
        b.media_play(false, 0.0);

        // Position 0 → frame 0: blue-dominant (fixture f0 starts blue).
        let f0 = dl.images[0].payload.expect("frame raster stamped");
        let r0 = qtsq_raster_for_id(f0).expect("raster in side-table");
        assert_eq!((r0.width, r0.height), (16, 16), "decoder frame dims");
        assert!(
            r0.pixels[2] > r0.pixels[0],
            "f0 blue-dominant (lossy fixture)"
        );

        // Seek to the end → frame 5: red-dominant. The republish path swaps
        // pixels under the same raster id — no relayout.
        b.media_play(false, 0.5);
        let m = b.state().media.expect("media after seek");
        assert!((m.position - 0.5).abs() < 1e-4, "transport mirrors");
        let dl2 = b.last_display_list().expect("republished display list");
        assert_eq!(dl2.images[0].payload, Some(f0), "stable raster id");
        let r5 = qtsq_raster_for_id(f0).expect("re-stepped raster");
        assert!(r5.pixels[0] > r5.pixels[2], "f5 red-dominant after seek");

        // A position past the end clamps to the last frame.
        b.media_play(false, 99.0);
        let r99 = qtsq_raster_for_id(f0).expect("clamped raster");
        assert!(r99.pixels[0] > r99.pixels[2], "clamp keeps frame 5");
        b.load_html("<h1>plain</h1>", "about:blank");
        assert!(
            qtsq_raster_for_id(f0).is_none(),
            "page release drops its frame raster"
        );
        clear_qtsq_rasters();
        clear_qtsq_media();
        clear_qtsq_videos();
    }

    // ---- web `<video>` playback attributes (poster/autoplay/loop/preload) ----

    /// A real media fixture from kitae_media's test data (containers the
    /// demuxer and the audio decoder accept, plus a PNG for posters).
    fn media_fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("kitae_media")
                .join("tests")
                .join("fixtures")
                .join(name),
        )
        .unwrap_or_else(|e| panic!("media fixture {name}: {e}"))
    }

    /// A temp directory holding `files` (name → bytes) plus the `file://` URL
    /// of a `page.html` in it, so a page can name its subresources relatively.
    fn write_media_page(name: &str, files: &[(&str, &[u8])]) -> String {
        let dir = std::env::temp_dir().join(format!("kitae_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        for (file, body) in files {
            std::fs::write(dir.join(file), body).expect("write fixture");
        }
        format!("file://{}/page.html", dir.display())
    }

    fn media_browser() -> Browser {
        Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        )
    }

    /// The URL of a subresource sitting next to the page (the same absolute
    /// form the engine resolves a relative `src`/`poster` to).
    fn sibling_url(page_url: &str, name: &str) -> String {
        let dir = page_url
            .rsplit_once('/')
            .map(|(d, _)| d)
            .unwrap_or(page_url);
        format!("{dir}/{name}")
    }

    #[test]
    fn video_poster_paints_until_a_frame_arrives() {
        let _store = crate::qtsq_test_lock();
        clear_qtsq_rasters();
        let png = media_fixture("anim.png");
        let page_url = write_media_page(
            "poster",
            &[
                ("clip.webm", &media_fixture("clip.webm")),
                ("still.png", &png),
            ],
        );
        let mut b = media_browser();
        b.load_html(
            r#"<body><video src="clip.webm" poster="still.png" controls width="320" height="180"></video></body>"#,
            &page_url,
        );
        let video_url = sibling_url(&page_url, "clip.webm");
        // The source loaded (the engine demuxed it) but no embedder has fed a
        // frame yet — so the poster is what the element shows.
        assert_eq!(web_video_codec(&video_url), 4, "VP9 track demuxed");
        assert!(
            qtsq_raster_for_url(&video_url).is_none(),
            "no video frame yet"
        );
        let dl = b.last_display_list().expect("display list").clone();
        let payload = dl.images[0]
            .payload
            .expect("poster stamped into the frame image");
        let raster = qtsq_raster_for_id(payload).expect("poster raster resolves");
        let want = decode_image(&png, Some("image/png")).expect("poster decodes");
        assert_eq!(
            (raster.width, raster.height),
            (want.width, want.height),
            "the poster's own size, not the video's 320x240 frame"
        );
        assert_eq!(&raster.pixels[..], &want.pixels[..], "poster pixels");
        assert_eq!(
            qtsq_raster_for_url(&sibling_url(&page_url, "still.png")).map(|r| r.id),
            Some(payload),
            "cached under the poster URL like any other image"
        );

        // Once the embedder feeds a frame it wins over the poster (the
        // poster's job ends when video data exists).
        web_video_feed_frame(&video_url, 4, 4, vec![7; 4 * 4 * 4]);
        b.media_play(false, 0.0);
        let dl2 = b.last_display_list().expect("republished display list");
        let frame = dl2.images[0].payload.expect("frame stamped");
        let frame_raster = qtsq_raster_for_id(frame).expect("frame raster");
        assert_eq!((frame_raster.width, frame_raster.height), (4, 4));
        assert_ne!(frame, payload, "the video frame replaced the poster");
        // The poster stays cached (a rewind before the next frame reuses it).
        assert!(qtsq_raster_for_id(payload).is_some());

        // No poster attribute: nothing but the placeholder box until a frame.
        clear_qtsq_rasters();
        let mut b = media_browser();
        b.load_html(
            r#"<body><video src="clip.webm" controls width="320" height="180"></video></body>"#,
            &page_url,
        );
        assert_eq!(
            b.last_display_list().expect("display list").images[0].payload,
            None,
            "no poster, no frame → placeholder"
        );
        clear_qtsq_rasters();
    }

    #[test]
    fn video_autoplay_starts_the_transport() {
        let _store = crate::qtsq_test_lock();
        let page_url = write_media_page(
            "autoplay",
            &[
                ("clip.webm", &media_fixture("clip.webm")),
                ("tone.mp3", &media_fixture("tone.mp3")),
            ],
        );
        let mut b = media_browser();
        // Silent video: `autoplay` alone starts the transport (the engine
        // never plays a video's audio track, so nothing audible escapes).
        b.load_html(
            r#"<body><video src="clip.webm" controls autoplay width="320" height="180"></video></body>"#,
            &page_url,
        );
        let m = b
            .state()
            .media
            .expect("autoplaying video page publishes media");
        assert!(m.playing, "autoplay starts playback");
        assert_eq!(m.position, 0.0, "autoplay starts at the beginning");
        let dl = b.last_display_list().expect("display list");
        assert_eq!(dl.media[0].state, (true, 0.0), "state reaches the item");

        // Without `autoplay` the transport stays parked.
        b.load_html(
            r#"<body><video src="clip.webm" controls width="320" height="180"></video></body>"#,
            &page_url,
        );
        let m = b.state().media.expect("video page publishes media");
        assert!(!m.playing, "no autoplay → paused");
        assert_eq!(m.position, 0.0);

        // Audible `<audio autoplay>` waits for a gesture; `muted` unblocks it.
        // (The UA sheet hides `<audio>`, so the author's display override is
        // what puts the media item on the page.)
        b.load_html(
            r#"<body><audio src="tone.mp3" controls autoplay style="display:inline-block"></audio></body>"#,
            &page_url,
        );
        let m = b.state().media.expect("audio page publishes media");
        assert!(!m.playing, "audible autoplay is blocked");
        b.load_html(
            r#"<body><audio src="tone.mp3" controls autoplay muted style="display:inline-block"></audio></body>"#,
            &page_url,
        );
        let m = b.state().media.expect("muted audio page publishes media");
        assert!(m.playing, "muted autoplay starts playback");
        // Autoplay is a load-time hint, not a watchdog: once the user has the
        // transport (paused mid-clip), a relayout leaves it alone.
        let duration = b.state().media.expect("media").duration;
        let seek = duration / 2.0;
        b.media_play(false, seek);
        b.resize(500.0, 600.0);
        let m = b.state().media.expect("media after resize");
        assert!(!m.playing, "a user pause outranks autoplay");
        assert!(
            (m.position - seek).abs() < 1e-4,
            "seek survives the relayout: {} vs {seek}",
            m.position
        );
    }

    #[test]
    fn video_loop_rewinds_the_transport() {
        let _store = crate::qtsq_test_lock();
        let page_url = write_media_page("loop", &[("clip.webm", &media_fixture("clip.webm"))]);
        let mut b = media_browser();
        b.load_html(
            r#"<body><video src="clip.webm" controls loop width="320" height="180"></video></body>"#,
            &page_url,
        );
        let duration = b.state().media.expect("media").duration;
        assert!(duration > 0.0, "demuxed duration");
        // A seek inside the resource is a plain seek.
        b.media_play(true, duration / 2.0);
        let m = b.state().media.expect("media after seek");
        assert!((m.position - duration / 2.0).abs() < 1e-3, "mid-seek holds");
        // Reaching the end rewinds to the start and keeps playing.
        b.media_play(false, duration);
        let m = b.state().media.expect("media after end");
        assert!(m.playing, "loop keeps playing across the end");
        assert_eq!(m.position, 0.0, "loop rewinds to zero");
        let dl = b.last_display_list().expect("display list");
        assert_eq!(
            dl.media[0].state,
            (true, 0.0),
            "the item mirrors the rewind"
        );

        // Without `loop` the same report parks at the end.
        b.load_html(
            r#"<body><video src="clip.webm" controls width="320" height="180"></video></body>"#,
            &page_url,
        );
        let duration = b.state().media.expect("media").duration;
        b.media_play(false, duration);
        let m = b.state().media.expect("media after end");
        assert!(!m.playing, "end of stream stops playback");
        assert!(
            (m.position - duration).abs() < 1e-3,
            "no rewind without loop"
        );
    }

    #[test]
    fn loop_scrub_to_the_end_is_a_seek_not_a_restart() {
        let _store = crate::qtsq_test_lock();
        let page_url = write_media_page("scrub", &[("clip.webm", &media_fixture("clip.webm"))]);
        let mut b = media_browser();
        b.load_html(
            r#"<body><video src="clip.webm" controls loop width="320" height="180"></video></body>"#,
            &page_url,
        );
        let duration = b.state().media.expect("media").duration;
        let m = b.last_display_list().expect("display list").media[0].clone();
        let (bx, by, bw, bh) = media_chrome_band(&m);
        // Drag the scrubber to the far right: the transport seeks to the end
        // and stays paused (the rewind belongs to playback reaching the end,
        // which is the host's report, not a seek).
        b.click(bx + 44.0 + (bw - 116.0), by + bh / 2.0);
        let m = b.state().media.expect("media after scrub");
        assert!(!m.playing, "the scrubber does not start playback");
        assert!(
            (m.position - duration).abs() < 1e-3,
            "scrubbed to the end: {} vs {duration}",
            m.position
        );
        // The play toggle then restarts it from the beginning, as `loop` says.
        b.click(bx + 10.0, by + bh / 2.0);
        let m = b.state().media.expect("media after play click");
        assert!(m.playing, "play from the end of a looped resource");
        assert_eq!(m.position, 0.0, "restarted from the beginning");
    }

    #[test]
    fn preload_none_defers_the_source_until_play() {
        let _store = crate::qtsq_test_lock();
        let page_url = write_media_page("preload", &[("clip.webm", &media_fixture("clip.webm"))]);
        let video_url = sibling_url(&page_url, "clip.webm");
        let mut b = media_browser();
        b.load_html(
            r#"<body><video src="clip.webm" controls preload="none" width="320" height="180"></video></body>"#,
            &page_url,
        );
        assert!(b.state().media.is_none(), "preload=none defers the source");
        assert_eq!(
            web_video_codec(&video_url),
            0,
            "the deferred source was never demuxed"
        );
        let dl = b.last_display_list().expect("display list");
        assert_eq!(dl.images[0].payload, None, "placeholder while deferred");

        // Asking to play is the request that loads it.
        b.media_play(true, 0.0);
        let m = b.state().media.expect("play loads the deferred source");
        assert!(m.playing, "playback runs");
        assert!(m.duration > 0.0, "metadata arrived with the load");
        assert_eq!(web_video_codec(&video_url), 4, "demuxed on demand");

        // `preload="metadata"` is the same single GET the engine always makes
        // (it has no byte-range fetcher), so the metadata is there at load.
        b.load_html(
            r#"<body><video src="clip.webm" controls preload="metadata" width="320" height="180"></video></body>"#,
            &page_url,
        );
        let m = b.state().media.expect("metadata preload publishes media");
        assert!(m.duration > 0.0, "duration known at load");
        assert!(!m.playing, "preload is a hint, not a play");
        assert_eq!(web_video_codec(&video_url), 4, "demuxed at load");

        // `autoplay` overrides `preload="none"`: playing needs the data.
        b.load_html(
            r#"<body><video src="clip.webm" controls preload="none" autoplay width="320" height="180"></video></body>"#,
            &page_url,
        );
        let m = b
            .state()
            .media
            .expect("autoplay overrides the preload hint");
        assert!(m.playing && m.duration > 0.0);
    }

    /// A page whose `<video>`/`<audio>` media resources live next to it: the
    /// real decodable fixtures, `clip.webm` under two names, plus a
    /// `broken.*` file that no demuxer or audio decoder accepts, so the
    /// selection has something to reject.
    fn write_source_page(name: &str) -> String {
        let clip = media_fixture("clip.webm");
        write_media_page(
            name,
            &[
                ("clip.webm", &clip),
                ("other.webm", &clip),
                ("tone.mp3", &media_fixture("tone.mp3")),
                ("broken.webm", b"not a webm container at all"),
                ("broken.mp3", b"not an mp3 frame at all"),
            ],
        )
    }

    /// The media URL the page's display list publishes for item 0 — what the
    /// frame's v20 media section carries, and what the host decodes.
    fn published_media_url(b: &Browser) -> String {
        b.last_display_list()
            .expect("display list")
            .media
            .first()
            .expect("media item")
            .src
            .clone()
    }

    #[test]
    fn source_child_picks_the_first_candidate_that_decodes() {
        let _store = crate::qtsq_test_lock();
        let page_url = write_source_page("source_pick");
        let clip = sibling_url(&page_url, "clip.webm");
        let other = sibling_url(&page_url, "other.webm");
        let broken = sibling_url(&page_url, "broken.webm");
        let mut b = media_browser();
        // A broken first candidate falls through to the next `<source>`...
        b.load_html(
            r#"<body><video controls width="320" height="180">
                 <source src="broken.webm"><source src="clip.webm">
               </video></body>"#,
            &page_url,
        );
        assert_eq!(
            b.state().media.expect("a source plays").url,
            clip,
            "first candidate that decodes"
        );
        assert_eq!(
            published_media_url(&b),
            clip,
            "the frame's media URL is the resource that decoded"
        );
        assert_eq!(web_video_codec(&broken), 0, "the broken one never decoded");

        // ...and the element's own `src` is the fallback, not the first
        // choice: a good `<source>` beats a broken `src`, and a broken
        // `<source>` falls back to a good `src`.
        b.load_html(
            r#"<body><video src="broken.webm" controls width="320" height="180">
                 <source src="clip.webm">
               </video></body>"#,
            &page_url,
        );
        assert_eq!(b.state().media.expect("source beats src").url, clip);
        b.load_html(
            r#"<body><video src="clip.webm" controls width="320" height="180">
                 <source src="broken.webm">
               </video></body>"#,
            &page_url,
        );
        assert_eq!(
            b.state().media.expect("src is the fallback").url,
            clip,
            "element src after no source decoded"
        );

        // Two decodable candidates: document order decides, first one wins.
        b.load_html(
            r#"<body><video controls width="320" height="180">
                 <source src="other.webm"><source src="clip.webm">
               </video></body>"#,
            &page_url,
        );
        assert_eq!(
            b.state().media.expect("first decodable source").url,
            other,
            "no unnecessary fetch of the second candidate"
        );
        assert_eq!(
            web_video_codec(&clip),
            0,
            "the later source stayed untouched"
        );

        // A relayout re-runs the selection but re-fetches nothing: the
        // winning URL stays the transport.
        b.resize(500.0, 600.0);
        assert_eq!(
            b.state().media.expect("still loaded after resize").url,
            other
        );
        assert_eq!(published_media_url(&b), other);

        // No candidate decodes → no media at all, and no URL published.
        b.load_html(
            r#"<body><video src="broken.webm" controls width="320" height="180">
                 <source src="broken.webm">
               </video></body>"#,
            &page_url,
        );
        assert!(b.state().media.is_none(), "nothing decodes → no media");
        assert!(
            published_media_url(&b).is_empty(),
            "and no URL for the host to chase"
        );
    }

    #[test]
    fn source_child_works_for_audio_too() {
        let _store = crate::qtsq_test_lock();
        let page_url = write_source_page("source_audio");
        let tone = sibling_url(&page_url, "tone.mp3");
        let mut b = media_browser();
        b.load_html(
            r#"<body><audio controls style="display:inline-block">
                 <source src="broken.mp3"><source src="tone.mp3">
               </audio></body>"#,
            &page_url,
        );
        let m = b.state().media.expect("audio source plays");
        assert_eq!(m.url, tone);
        assert_eq!(m.kind, MediaKind::Audio, "still an audio handoff");
        assert_eq!(published_media_url(&b), tone);
    }

    #[test]
    fn source_media_condition_gates_which_candidate_is_tried() {
        let _store = crate::qtsq_test_lock();
        let page_url = write_source_page("source_media");
        let clip = sibling_url(&page_url, "clip.webm");
        let other = sibling_url(&page_url, "other.webm");
        let mut b = media_browser();
        // The 400px test viewport is narrower than the gated candidate's
        // condition, so the next source is the one that plays.
        b.load_html(
            r#"<body><video controls width="320" height="180">
                 <source src="other.webm" media="(min-width: 900px)">
                 <source src="clip.webm">
               </video></body>"#,
            &page_url,
        );
        assert_eq!(b.state().media.expect("gated-out source skipped").url, clip);
        assert_eq!(
            web_video_codec(&other),
            0,
            "a source whose media condition fails is never fetched"
        );

        // Widening the viewport makes the first candidate eligible again.
        b.resize(1200.0, 600.0);
        assert_eq!(
            b.state().media.expect("condition holds now").url,
            other,
            "the source list is re-evaluated on resize"
        );

        // A condition no candidate satisfies: nothing is fetched, and the
        // element's own (absent) `src` is not a rescue.
        b.resize(400.0, 600.0);
        b.load_html(
            r#"<body><video controls width="320" height="180">
                 <source src="clip.webm" media="(min-width: 900px)">
               </video></body>"#,
            &page_url,
        );
        assert!(b.state().media.is_none(), "no eligible source → no media");
        assert_eq!(
            web_video_codec(&clip),
            0,
            "the gated source stayed unfetched"
        );
    }

    #[test]
    fn deferred_source_list_selects_on_playback_and_republishes() {
        let _store = crate::qtsq_test_lock();
        let page_url = write_source_page("source_preload");
        let clip = sibling_url(&page_url, "clip.webm");
        let other = sibling_url(&page_url, "other.webm");
        let broken = sibling_url(&page_url, "broken.webm");
        let mut b = media_browser();
        b.load_html(
            r#"<body><video controls preload="none" width="320" height="180">
                 <source src="broken.webm"><source src="clip.webm">
               </video></body>"#,
            &page_url,
        );
        // `preload="none"` defers the whole resource selection, sources and all.
        assert!(b.state().media.is_none(), "the source list is deferred");
        for url in [&clip, &other, &broken] {
            assert_eq!(web_video_codec(url), 0, "{url} was never demuxed");
        }

        // Asking to play is the request: the first candidate that decodes
        // wins, and the frame's media URL follows it.
        b.media_play(true, 0.0);
        let m = b.state().media.expect("play runs the selection");
        assert_eq!(m.url, clip);
        assert!(
            m.playing && m.duration > 0.0,
            "metadata arrived with the load"
        );
        assert_eq!(published_media_url(&b), clip, "the winner is published");
        assert_eq!(
            b.last_display_list().expect("dl").media[0].state,
            (true, 0.0),
            "transport state reaches the item"
        );
        // Re-running the selection on a later play is a no-op: the same
        // candidate wins and nothing is fetched twice.
        b.media_play(false, 0.0);
        b.media_play(true, 0.0);
        assert_eq!(b.state().media.expect("still the same source").url, clip);
        assert_eq!(published_media_url(&b), clip);
    }

    #[test]
    fn crossorigin_anonymous_fetches_and_credentials_fail_closed() {
        let _store = crate::qtsq_test_lock();
        let page_url = write_source_page("crossorigin");
        let clip = sibling_url(&page_url, "clip.webm");
        let mut b = media_browser();
        // `crossorigin` with no value is the empty string, which for this
        // attribute means anonymous; both forms load like a plain fetch.
        for attr in ["crossorigin", "crossorigin=\"anonymous\"", ""] {
            b.load_html(
                &format!(
                    r#"<body><video src="clip.webm" controls {attr} width="320" height="180"></video></body>"#
                ),
                &page_url,
            );
            assert_eq!(
                b.state().media.expect("anonymous fetches").url,
                clip,
                "{attr}"
            );
        }

        // `use-credentials` asks for a credentialed CORS fetch the engine
        // cannot make: it fails closed rather than sending it anyway, and
        // publishes no URL for the host to chase.
        b.load_html(
            r#"<body><video src="clip.webm" controls crossorigin="use-credentials" width="320" height="180"></video></body>"#,
            &page_url,
        );
        assert!(
            b.state().media.is_none(),
            "use-credentials never becomes media state"
        );
        assert_eq!(
            web_video_codec(&clip),
            0,
            "the credentialed resource was never fetched"
        );
        assert!(
            published_media_url(&b).is_empty(),
            "and no URL reaches the frame"
        );
        // The element is otherwise a normal video: the poster still paints
        // (posters are images, not the media resource) and playback is not
        // otherwise disabled.
        let poster_page = write_media_page(
            "crossorigin_poster",
            &[
                ("clip.webm", &media_fixture("clip.webm")),
                ("still.png", &media_fixture("anim.png")),
            ],
        );
        b.load_html(
            r#"<body><video src="clip.webm" poster="still.png" controls crossorigin="use-credentials" width="320" height="180"></video></body>"#,
            &poster_page,
        );
        assert!(b.state().media.is_none(), "still no media");
        assert!(
            b.last_display_list().expect("dl").images[0]
                .payload
                .is_some(),
            "the poster image still paints"
        );

        // `playsinline` is a no-op flag, and specifically never a reason to
        // refuse playback.
        b.load_html(
            r#"<body><video src="clip.webm" controls autoplay playsinline width="320" height="180"></video></body>"#,
            &page_url,
        );
        let m = b
            .state()
            .media
            .expect("playsinline does not disable playback");
        assert!(m.playing, "playsinline + autoplay still autoplays");
    }

    #[test]
    fn qtsq_tensor_navigation_shows_viewer_page() {
        let _g = qtsq_test_lock();
        let url = write_temp_fixture("fix_tensor.qtsq");
        let page = fetch_step(&url);
        assert!(page.0.contains("Tensor"), "strategy name: {}", page.0);
        assert!(
            page.0.contains("Original size"),
            "original size line: {}",
            page.0
        );
        assert!(
            page.0.contains("Encryption: no"),
            "encryption line: {}",
            page.0
        );
        // Real viewer now: heat-map strip image + values table + shape line.
        assert!(
            page.0.contains("<img"),
            "heat-map strip present: {}",
            page.0
        );
        assert!(
            page.0.contains("<table"),
            "values table present: {}",
            page.0
        );
        assert!(page.0.contains("shape"), "shape summary: {}", page.0);
        assert!(page.0.contains("neural tensor"), "kind summary: {}", page.0);
        // The strip raster is registered under the heat-map fragment URL and
        // the <img src> points at it (relayout stamps the payload id).
        let src_start = page
            .0
            .find("<img src=\"")
            .map(|i| i + "<img src=\"".len())
            .expect("img src");
        let src = &page.0[src_start..page.0[src_start..].find('"').unwrap() + src_start];
        let raster = qtsq_raster_for_url(src).expect("strip raster registered");
        assert_eq!(
            (raster.width, raster.height),
            (16 * 8, 2 * 8),
            "24 values → 16×2 cells of 8px"
        );
        // Ramp endpoints appear in the strip: min value paints indigo
        // (68,1,84), max paints yellow (253,231,37) — neither matches the
        // dark padding filler (16,16,20).
        assert!(raster
            .pixels
            .chunks(4)
            .any(|p| p[0] < 90 && p[1] < 60 && p[2] > 60));
        assert!(raster.pixels.chunks(4).any(|p| p[0] > 200 && p[1] > 180));
        clear_qtsq_rasters();
        std::fs::remove_file(std::path::Path::new(
            url.strip_prefix("file://").unwrap_or(&url),
        ))
        .ok();
    }

    #[test]
    fn qtsq_history_back_forward_works() {
        let url = write_temp_fixture("fix_text.qtsq");
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.navigate("about:blank");
        b.navigate(&url);
        assert!(b.state().url.ends_with("fix_text.qtsq"), "on the qtsq page");
        b.go(-1);
        assert_eq!(b.state().url, "about:blank");
        b.go(1);
        assert!(
            b.state().url.ends_with("fix_text.qtsq"),
            "forward returns to qtsq"
        );
    }

    #[test]
    fn history_caps_at_max_evicting_oldest() {
        // 1 entry from test_browser + 505 navigations = 506 pushes; the cap
        // must evict the six oldest (M-4, SECURITY_AUDIT.md).
        let mut b = test_browser("<h1>x</h1>");
        for i in 0..505 {
            let base = format!("https://example.com/page/{i}");
            b.load_html("<h1>x</h1>", base.as_str());
        }
        assert_eq!(
            b.history.len(),
            HISTORY_MAX,
            "history never exceeds the cap"
        );
        assert_eq!(
            b.history[0], "https://example.com/page/5",
            "oldest survivor"
        );
        assert_eq!(b.history[HISTORY_MAX - 1], "https://example.com/page/504");
        // The position still lands on the newest entry: back/forward work
        // exactly as before the cap.
        assert_eq!(b.state().url, "https://example.com/page/504");
        assert!(b.state().can_go_back);
        assert!(!b.state().can_go_forward);
        b.go(-1);
        assert_eq!(b.state().url, "https://example.com/page/503");
        b.go(1);
        assert_eq!(b.state().url, "https://example.com/page/504");
    }

    #[test]
    fn qtsq_corrupt_header_still_renders_info_page() {
        // Truncated payload: fetch succeeds (it is a file), probe fails —
        // the pipeline must produce a page, not a panic.
        let path = std::env::temp_dir().join(format!("kitae_j2_trunc_{}", std::process::id()));
        std::fs::write(&path, b"QTS\x01\x03").expect("write truncated");
        let url = format!("file://{}", path.display());
        let page = fetch_step(&url);
        assert!(
            page.0.contains("header unreadable"),
            "info page: {}",
            page.0
        );
        std::fs::remove_file(&path).ok();
        for name in [
            "fix_image.qtsq",
            "fix_text.qtsq",
            "fix_audio.qtsq",
            "fix_video.qtsq",
            "fix_tensor.qtsq",
        ] {
            let p = std::env::temp_dir().join(format!("kitae_j2_{}_{}", std::process::id(), name));
            std::fs::remove_file(&p).ok();
        }
    }

    #[test]
    fn qtsq_archive_envelope_round_trips() {
        let payload = qtsq_archive_payload(
            "https://example.com/dir/page.html",
            "Doc\ntitle",
            "<p>body</p>",
        );
        // Newlines inside the title are flattened at save time so the
        // line-oriented envelope survives the strict sanitizer.
        assert!(!payload.contains("Doc\ntitle"));
        let (base, title, doc) = qtsq_archive_parts(&payload).expect("envelope parses back");
        assert_eq!(base, "https://example.com/dir/page.html");
        assert_eq!(title, "Doc title");
        assert_eq!(doc, "<p>body</p>");
        // Non-archive text (any .qtsq text payload saved before slice ②)
        // must not be mistaken for an archive.
        assert!(qtsq_archive_parts("plain notes\nsecond line").is_none());
        assert!(
            qtsq_archive_parts("<!--kitae-archive v1\nbase=\n-->").is_none(),
            "empty base is rejected"
        );
        assert!(
            qtsq_archive_parts("<!--kitae-archive v1\nbase=https://x\nno terminator").is_none(),
            "missing terminator is rejected"
        );
    }

    /// NullHost plus a save_finished recorder for the archive tests. The
    /// recorder is shared so the test can inspect it after the Browser
    /// (which owns the host) is dropped.
    struct SaveHost {
        calls: std::rc::Rc<std::cell::RefCell<Vec<(String, Option<String>)>>>,
    }
    impl Host for SaveHost {
        fn publish(&mut self, _state: &PublicState, _dl: &DisplayList) {}
        fn scroll_updated(&mut self, _state: &PublicState) {}
        fn set_loading(&mut self, _loading: bool) {}
        fn open_external(&mut self, _url: &str) {}
        fn save_finished(&mut self, path: &str, error: Option<&str>) {
            self.calls
                .borrow_mut()
                .push((path.to_string(), error.map(|e| e.to_string())));
        }
    }

    #[test]
    fn qtsq_save_page_and_reopen_as_document() {
        let _g = crate::qtsq_test_lock();
        // save_page_qtsq → device_keys() generates key pairs; keep them out
        // of the user's real key store.
        let old_dir = std::env::var("KITAE_QTSQ_KEY_DIR").ok();
        let key_dir = std::env::temp_dir().join(format!(
            "kitae_j6_keys_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&key_dir).expect("create key dir");
        std::env::set_var("KITAE_QTSQ_KEY_DIR", &key_dir);

        let out_path =
            std::env::temp_dir().join(format!("kitae_j6_save_{}.qtsq", std::process::id()));
        let out = out_path.to_string_lossy().into_owned();
        let calls = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let mut b = Browser::new(
                Box::new(SaveHost {
                    calls: std::rc::Rc::clone(&calls),
                }),
                Box::new(ApproxMeasurer),
                (400.0, 600.0),
            );
            b.load_html(
                "<html><head><title>T</title></head><body><p>hello archive</p>\
                 <a href='n.html'>n</a></body></html>",
                "https://example.com/dir/page.html",
            );
            let n = b.save_page_qtsq(&out).expect("save succeeds");
            assert!(n > 0, "archive bytes written");
        }
        // The host was told it succeeded (fire-and-forget command).
        assert_eq!(
            calls.borrow().as_slice(),
            [(out.clone(), None)] as [(String, Option<String>); 1],
            "save_finished reported success with the path"
        );

        // Archive is device-encrypted and device-signed; reopen is
        // passwordless through the device key path.
        let bytes = std::fs::read(&out_path).expect("archive file");
        let info = kitae_qtsq::probe(&bytes).expect("probe");
        assert!(info.encrypted, "device-encrypted");
        assert!(info.signed, "device-signed");
        let text = kitae_qtsq::decode_text(&bytes, None).expect("passwordless decode");
        assert!(text.starts_with("<!--kitae-archive v1\nbase=https://example.com/dir/page.html\n"));

        // Reopen through the real pipeline: session URL stays the .qtsq file
        // URL, the document renders (parser absorbs the nested html/head/body
        // from the archive), and the signature verifies.
        let mut b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        b.navigate("about:blank");
        let file_url = format!("file://{}", out_path.display());
        b.navigate(&file_url);
        let st = b.state();
        assert!(
            st.url.ends_with(".qtsq"),
            "session URL keeps the file: {}",
            st.url
        );
        assert_eq!(st.title, "T", "archived title becomes the page title");
        assert_eq!(
            st.qtsq_sig,
            QtsqSigState::Valid,
            "device signature verifies"
        );
        let dl = b.last_display_list().expect("display list after reopen");
        // Runs are word-granular.
        assert!(
            dl.texts.iter().any(|t| t.text == "hello")
                && dl.texts.iter().any(|t| t.text == "archive"),
            "archived document renders: {:?}",
            dl.texts
                .iter()
                .map(|t| &t.text)
                .take(10)
                .collect::<Vec<_>>()
        );
        assert!(
            dl.texts.iter().any(|t| t.text.contains("Saved")),
            "provenance note present"
        );
        // Relative links resolve against the archived base, not the .qtsq
        // path — clicking must navigate to the sibling page.
        let links: Vec<(f32, f32, f32, f32)> = b
            .links
            .iter()
            .filter(|l| l.href == "n.html")
            .map(|l| (l.x, l.y, l.w, l.h))
            .collect();
        assert_eq!(links.len(), 1, "one 'n' link: {:?}", links);
        b.click(links[0].0 + links[0].2 / 2.0, links[0].1 + links[0].3 / 2.0);
        assert_eq!(
            b.state().url,
            "https://example.com/dir/n.html",
            "click resolves against the archived base"
        );

        std::fs::remove_file(&out_path).ok();
        std::fs::remove_dir_all(&key_dir).ok();
        match old_dir {
            Some(d) => std::env::set_var("KITAE_QTSQ_KEY_DIR", d),
            None => std::env::remove_var("KITAE_QTSQ_KEY_DIR"),
        }
    }

    #[test]
    fn focus_cycles_through_controls_in_order() {
        let mut b = test_browser(
            "<html><body><a href='/a' id='l1'>one</a><input id='q' name='q'><button id='go'>Go</button></body></html>",
        );
        assert_eq!(b.focusables().len(), 3);
        assert_eq!(b.focused(), None);
        let f1 = b.focus_next().unwrap();
        assert_eq!(b.focused(), Some(f1));
        assert_eq!(b.focused_label().as_deref(), Some("l1"));
        b.focus_next();
        assert_eq!(b.focused_label().as_deref(), Some("q"));
        b.focus_next();
        assert_eq!(b.focused_label().as_deref(), Some("go"));
        // Wraps.
        b.focus_next();
        assert_eq!(b.focused_label().as_deref(), Some("l1"));
        b.focus_prev();
        assert_eq!(b.focused_label().as_deref(), Some("go"));
        assert!(b.focus_by_id("q"));
        assert_eq!(b.focused_label().as_deref(), Some("q"));
        assert!(!b.focus_by_id("missing"));
    }

    #[test]
    fn dialog_modal_autofocuses_and_traps_tab() {
        let mut b = test_browser(
            "<html><body><a href='/out' id='out'>out</a>\
             <dialog open><input id='q' name='q'><button id='go'>Go</button></dialog></body></html>",
        );
        // Load auto-focuses the first focusable inside the modal.
        assert_eq!(
            b.focused_label().as_deref(),
            Some("q"),
            "open dialog claims focus on load"
        );
        // Tab (and shift-Tab) cycle stay inside the dialog — the page link
        // behind the modal never enters the cycle.
        b.focus_next();
        assert_eq!(b.focused_label().as_deref(), Some("go"));
        b.focus_next();
        assert_eq!(
            b.focused_label().as_deref(),
            Some("q"),
            "tab wraps inside the dialog"
        );
        b.focus_prev();
        assert_eq!(b.focused_label().as_deref(), Some("go"));
    }

    #[test]
    fn escape_closes_topmost_dialog_then_next() {
        let mut b = test_browser(
            "<html><body><dialog open><p>first modal</p></dialog>\
             <dialog open><p>second modal</p></dialog></body></html>",
        );
        b.escape_key();
        let dl = b.last_display_list().expect("display list after first Esc");
        assert!(
            !dl.texts.iter().any(|t| t.text.contains("second")),
            "Esc closes the topmost dialog"
        );
        assert!(
            dl.texts.iter().any(|t| t.text.contains("first")),
            "the dialog beneath stays open"
        );
        b.escape_key();
        let dl = b
            .last_display_list()
            .expect("display list after second Esc");
        assert!(
            !dl.texts.iter().any(|t| t.text.contains("first")),
            "second Esc closes the remaining dialog"
        );
        // Esc without an open dialog is a no-op.
        b.escape_key();
    }

    #[test]
    fn modal_backdrop_gates_click_until_escape() {
        let mut b = test_browser(
            "<html><body><a href='/out' id='out'>Page out link</a>\
             <dialog open style='width:200px'><p>Modal body</p></dialog></body></html>",
        );
        let out = b
            .links()
            .iter()
            .find(|l| l.href == "/out")
            .map(|l| (l.x, l.y, l.w, l.h))
            .expect("background link area");
        // The card sits at left:50% (200) shifted -100px by translateX(-50%)
        // → x 100..300; the background link (x≈16) is behind the backdrop.
        b.click(out.0 + out.2 / 2.0, out.1 + out.3 / 2.0);
        assert_eq!(
            b.state().url,
            "https://example.com/",
            "backdrop click is swallowed while a modal is open"
        );
        // Esc closes the dialog; the same click now lands on the page.
        b.escape_key();
        let dl = b.last_display_list().expect("display list after Esc");
        assert!(
            !dl.texts.iter().any(|t| t.text.contains("Modal")),
            "dialog closed by Esc"
        );
        b.click(out.0 + out.2 / 2.0, out.1 + out.3 / 2.0);
        assert_eq!(
            b.state().url,
            "https://example.com/out",
            "after Esc the same click reaches the page"
        );
    }

    #[test]
    fn input_values_round_trip_and_typing_appends() {
        let mut b = test_browser(
            "<html><body><form id='f' action='/s'><input id='q' name='q' value='hi'><textarea id='t' name='t'>Hey</textarea><select id='s' name='c'><option>One</option><option selected>Two</option></select></form></body></html>",
        );
        assert_eq!(b.input_value("q").as_deref(), Some("hi"));
        assert_eq!(b.input_value("t").as_deref(), Some("Hey"));
        assert_eq!(b.input_value("s").as_deref(), Some("Two"));
        assert!(b.set_input_value("q", "hello"));
        assert_eq!(b.input_value("q").as_deref(), Some("hello"));
        assert!(b.focus_by_id("q"));
        assert!(b.type_into_focused("!"));
        assert_eq!(b.input_value("q").as_deref(), Some("hello!"));
        assert!(b.set_input_value("t", "New"));
        assert!(b.set_input_value("s", "One"));
        assert_eq!(b.input_value("s").as_deref(), Some("One"));
        assert!(!b.set_input_value("missing", "x"));
        assert_eq!(b.input_value("missing"), None);
    }

    /// A11a: focusing an editable control parks the caret at the end; the
    /// caret then drives insert-at-position, arrow movement (clamped), and
    /// backspace / delete-forward.
    #[test]
    fn caret_inserts_at_position_and_moves() {
        let mut b = test_browser("<html><body><input id='q' value='hello'></body></html>");
        assert!(b.focus_by_id("q"));
        assert_eq!(
            b.caret_position(),
            Some(5),
            "focus parks the caret at the end"
        );
        // Place mid-string and insert there (not append).
        assert_eq!(b.set_caret(2), Some(2));
        assert!(b.type_into_focused("XY"));
        assert_eq!(b.input_value("q").as_deref(), Some("heXYllo"));
        assert_eq!(
            b.caret_position(),
            Some(4),
            "caret advances past the insert"
        );
        // Arrow movement clamps to the value bounds.
        assert_eq!(b.move_caret(-2), Some(2));
        assert_eq!(b.move_caret(100), Some(7), "clamps to the end");
        assert_eq!(b.move_caret(-100), Some(0), "clamps to the start");
        // Backspace deletes before the caret; delete-forward after it.
        assert_eq!(b.set_caret(2), Some(2)); // h [e] X Y l l o
        assert!(b.backspace_focused());
        assert_eq!(b.input_value("q").as_deref(), Some("hXYllo"));
        assert_eq!(b.caret_position(), Some(1));
        assert!(b.delete_forward()); // remove 'X'
        assert_eq!(b.input_value("q").as_deref(), Some("hYllo"));
        assert_eq!(b.caret_position(), Some(1));
        // Home / End.
        assert_eq!(b.caret_home(), Some(0));
        assert_eq!(b.caret_end(), Some(5));
    }

    /// A11a: backspace at the start and delete-forward at the end report
    /// false and leave the value untouched.
    #[test]
    fn caret_edge_deletes_are_noops() {
        let mut b = test_browser("<html><body><input id='q' value='ab'></body></html>");
        assert!(b.focus_by_id("q"));
        assert_eq!(b.caret_position(), Some(2));
        assert!(b.backspace_focused()); // "a"
        assert_eq!(b.input_value("q").as_deref(), Some("a"));
        assert!(b.backspace_focused()); // ""
        assert_eq!(b.input_value("q").as_deref(), Some(""));
        assert_eq!(b.caret_position(), Some(0));
        assert!(!b.backspace_focused(), "backspace at 0 is a no-op");
        assert!(!b.delete_forward(), "delete at the end is a no-op");
        assert_eq!(b.input_value("q").as_deref(), Some(""));
    }

    /// A11a: clicking inside an editable control places the caret at the
    /// nearest char boundary, using the same width walk the paint uses.
    #[test]
    fn click_places_caret_in_text() {
        let mut b = test_browser("<html><body><input id='q' value='hello'></body></html>");
        assert!(b.focus_by_id("q"));
        let node = b.focused().expect("focused node");
        let dl = b.last_display_list().expect("display list").clone();
        let run = dl
            .texts
            .iter()
            .find(|t| t.node == node.0)
            .expect("control label run");
        let cw = run.size * 0.5; // ApproxMeasurer: chars * size * 0.5
        b.click(run.x, run.y);
        assert_eq!(b.caret_position(), Some(0), "click at the text start");
        b.click(run.x + 2.0 * cw + cw * 0.4, run.y);
        assert_eq!(b.caret_position(), Some(2), "click just after 'he'");
        b.click(run.x + 10.0 * cw, run.y);
        assert_eq!(
            b.caret_position(),
            Some(5),
            "click past the text clamps to the end"
        );
    }

    #[test]
    fn point_selection_and_caret_extension_map_to_offsets() {
        let mut b = test_browser("<html><body><input id='q' value='hello'></body></html>");
        assert!(b.focus_by_id("q"));
        let node = b.focused().expect("focused node");
        let run = b
            .last_display_list()
            .expect("display list")
            .texts
            .iter()
            .find(|t| t.node == node.0)
            .expect("value run")
            .clone();
        let cw = run.size * 0.5;
        assert!(b.begin_selection_at_point(run.x + 1.4 * cw, run.y));
        assert!(b.extend_selection_at_point(run.x + 2.6 * cw, run.y));
        assert_eq!(b.selection_range(), Some((1, 3)));
        assert_eq!(b.selection.as_deref(), Some("el"));
        assert_eq!(b.extend_caret(1), Some(4));
        assert_eq!(b.selection_range(), Some((1, 4)));
        assert!(b.type_into_focused("X"));
        assert_eq!(b.input_value("q").as_deref(), Some("hXo"));
    }

    /// A11b: a contenteditable host is focusable and editable as plain
    /// text — the value reads as its concatenated descendant text, focus
    /// parks the caret at the end, and typing / backspace work through the
    /// same editing ops the form controls use (edits flatten markup to one
    /// text child, the documented v1 contract).
    #[test]
    fn contenteditable_focus_and_typing() {
        let mut b = test_browser(
            "<html><body><div id='ed' contenteditable>Hello <b>world</b></div></body></html>",
        );
        // The host joins the tab order (focus_next from nothing = first
        // focusable); the nested <b> is not a separate focusable (the
        // attribute must sit on the element itself).
        let host = b.focus_next().expect("contenteditable is focusable");
        assert_eq!(
            b.page.as_ref().unwrap().dom.get_element_by_id("ed"),
            Some(host),
            "the host is the first focusable"
        );
        assert_eq!(
            b.editable_value("ed").as_deref(),
            Some("Hello world"),
            "value = concatenated descendant text"
        );
        assert_eq!(
            b.caret_position(),
            Some(11),
            "focus parks the caret at the end"
        );
        assert!(b.type_into_focused("!"));
        assert_eq!(b.editable_value("ed").as_deref(), Some("Hello world!"));
        assert_eq!(b.caret_position(), Some(12));
        // The edit flattened the host to one text child — the <b> is gone.
        let page = b.page.as_ref().unwrap();
        let kids = page.dom.children(host).len();
        assert_eq!(kids, 1, "edit collapses the host to a single text child");
        assert!(b.backspace_focused());
        assert_eq!(b.editable_value("ed").as_deref(), Some("Hello world"));
        assert_eq!(b.caret_position(), Some(11));
    }

    /// A11b: clicking inside a contenteditable host focuses it and places
    /// the caret at the clicked word — the host paints its value as normal
    /// text-flow runs (one per word, stamped with the text nodes' ids), so
    /// the caret maps the offset across those runs, and the decorated caret
    /// rect anchors to the run's geometry rather than a control box.
    #[test]
    fn contenteditable_click_places_caret_and_paints() {
        let mut b =
            test_browser("<html><body><div id='ed' contenteditable>alpha beta</div></body></html>");
        assert!(b.focus_by_id("ed"));
        let host = b.focused().expect("host");
        let dl = b.last_display_list().expect("display list").clone();
        let runs: Vec<_> = dl
            .texts
            .iter()
            .filter(|t| t.text == "alpha" || t.text == "beta")
            .collect();
        assert_eq!(runs.len(), 2, "one painted run per word");
        assert!(
            runs.iter().all(|t| t.node == host.0),
            "direct text paints host-stamped (nested markup would stamp descendants — \
             both resolve through the subtree filter in editable_runs)"
        );
        let first = runs[0];
        let cw = first.size * 0.5; // ApproxMeasurer: chars * size * 0.5
                                   // Click mid-'alpha' → caret inside the first word.
        b.click(first.x + 3.0 * cw + cw * 0.4, first.y);
        assert_eq!(
            b.caret_position(),
            Some(3),
            "click mid-'alpha' places the caret there"
        );
        // Click just left of 'beta''s third-char midpoint → caret maps across
        // the first word + its space and rounds to the glyph edge (offset 8).
        let last = runs[1];
        b.click(last.x + 2.4 * last.size * 0.5, last.y);
        assert_eq!(
            b.caret_position(),
            Some(8),
            "5 ('alpha') + 1 (space) + 2 into 'beta'"
        );
        // The caret rect: appended by decorate_caret, stamped with the host,
        // 1px wide, sitting on the clicked line's baseline geometry.
        let clean = b.last_display_list().expect("display list").clone();
        let caret = b
            .decorate_caret(&clean)
            .rects
            .last()
            .expect("caret rect")
            .clone();
        assert!(caret.node == host.0 && (caret.w - 1.0).abs() < 0.01);
        assert!(
            caret.x > last.x - 1.0 && caret.y > 0.0,
            "anchored in the text, not the box edge"
        );
        assert!(
            !clean
                .rects
                .iter()
                .any(|r| r.node == host.0 && (r.w - 1.0).abs() < 0.01),
            "the clean stored list carries no caret rect"
        );
    }

    /// A11b: `contenteditable="false"` is neither focusable nor editable,
    /// and a runtime flip to "false" on a focused host ends editing.
    #[test]
    fn contenteditable_false_is_inert() {
        let mut b = test_browser(
            "<html><body><div id='ed' contenteditable='false'>static</div>\
             <div id='on' contenteditable='true'>live</div></body></html>",
        );
        assert!(
            !b.focus_by_id("ed"),
            "contenteditable=false is not focusable"
        );
        assert!(b.focus_by_id("on"));
        assert!(b.type_into_focused("X"));
        assert_eq!(b.editable_value("on").as_deref(), Some("liveX"));
        // Flip the attribute at runtime; focus and editing both stop.
        let node = b.focused().unwrap();
        b.page
            .as_mut()
            .unwrap()
            .dom
            .set_attr(node, "contenteditable", "false");
        b.clear_focus();
        assert!(
            !b.focus_by_id("on"),
            "runtime flip to false ends focusability"
        );
    }

    #[test]
    fn range_selection_replaces_typing_and_deletes() {
        let mut b = test_browser("<html><body><input id='q' value='abcdef'></body></html>");
        assert!(b.focus_by_id("q"));
        assert!(b.begin_selection(1));
        assert!(b.extend_selection(4));
        assert!(b.end_selection());
        assert_eq!(b.selection_range(), Some((1, 4)));
        assert_eq!(b.selection_text().as_deref(), Some("bcd"));
        assert!(b.type_into_focused("X"));
        assert_eq!(b.input_value("q").as_deref(), Some("aXef"));
        assert_eq!(b.caret_position(), Some(2));
        assert_eq!(b.selection_range(), None);

        assert!(b.set_input_value("q", "abcdef"));
        assert_eq!(b.set_caret(1), Some(1));
        assert!(b.set_selection_range(1, 4));
        assert!(b.backspace_focused());
        assert_eq!(b.input_value("q").as_deref(), Some("aef"));
        assert_eq!(b.caret_position(), Some(1));

        assert!(b.set_input_value("q", "abcdef"));
        assert_eq!(b.set_caret(1), Some(1));
        assert!(b.set_selection_range(1, 4));
        assert!(b.delete_forward());
        assert_eq!(b.input_value("q").as_deref(), Some("aef"));
        assert_eq!(b.caret_position(), Some(1));
    }

    #[test]
    fn contenteditable_range_replaces_plain_text() {
        let mut b = test_browser(
            "<html><body><div id='ed' contenteditable>alpha beta gamma</div></body></html>",
        );
        assert!(b.focus_by_id("ed"));
        assert!(b.set_selection_range(6, 10));
        assert_eq!(b.selection_text().as_deref(), Some("beta"));
        assert!(b.type_into_focused("X"));
        assert_eq!(b.editable_value("ed").as_deref(), Some("alpha X gamma"));
        assert_eq!(b.selection_range(), None);
    }

    #[test]
    fn range_highlights_have_geometry_and_do_not_accumulate() {
        let mut b = test_browser("<html><body><input id='q' value='abcdef'></body></html>");
        assert!(b.focus_by_id("q"));
        assert!(b.set_selection_range(1, 4));
        let clean = b.last_display_list().expect("display list").clone();
        let decorated = b.decorate_caret(&clean);
        let highlights: Vec<_> = decorated
            .rects
            .iter()
            .filter(|r| r.color == SELECTION_HIGHLIGHT_COLOR)
            .collect();
        assert_eq!(highlights.len(), 1);
        let run = clean
            .texts
            .iter()
            .find(|t| t.node == b.focused().unwrap().0)
            .expect("value run");
        let expected_x = run.x + run.size * 0.5;
        let expected_w = run.size * 1.5;
        assert!((highlights[0].x - expected_x).abs() < 0.1);
        assert!((highlights[0].w - expected_w).abs() < 0.1);
        assert!(highlights[0].h > 0.0);
        assert!(!clean
            .rects
            .iter()
            .any(|r| r.color == SELECTION_HIGHLIGHT_COLOR));
        let decorated_again = b.decorate_caret(&clean);
        assert_eq!(
            decorated_again
                .rects
                .iter()
                .filter(|r| r.color == SELECTION_HIGHLIGHT_COLOR)
                .count(),
            1
        );
        let nested = b.decorate_caret(&decorated);
        assert_eq!(
            nested
                .rects
                .iter()
                .filter(|r| r.color == SELECTION_HIGHLIGHT_COLOR)
                .count(),
            1
        );
        b.publish_current();
        assert!(!b
            .last_display_list()
            .unwrap()
            .rects
            .iter()
            .any(|r| r.color == SELECTION_HIGHLIGHT_COLOR));
    }

    #[test]
    fn composition_commit_and_cancel_replace_preview() {
        let mut b = test_browser("<html><body><input id='q' value='abc'></body></html>");
        assert!(b.focus_by_id("q"));
        assert_eq!(b.set_caret(1), Some(1));
        assert!(b.update_marked_text("XY"));
        assert!(b.composition_active());
        assert_eq!(b.input_value("q").as_deref(), Some("aXYbc"));
        assert!(b
            .last_display_list()
            .unwrap()
            .texts
            .iter()
            .any(|t| t.text == "aXYbc"));
        assert!(b.commit_composition());
        assert!(!b.composition_active());
        assert_eq!(b.input_value("q").as_deref(), Some("aXYbc"));
        assert_eq!(b.caret_position(), Some(3));

        assert_eq!(b.set_caret(1), Some(1));
        assert!(b.update_marked_text("Q"));
        assert!(b.cancel_composition());
        assert!(!b.composition_active());
        assert_eq!(b.input_value("q").as_deref(), Some("aXYbc"));
        assert_eq!(b.caret_position(), Some(1));

        assert!(b.set_input_value("q", "abc"));
        assert!(b.set_selection_range(1, 3));
        assert!(b.update_marked_text("X"));
        assert_eq!(b.input_value("q").as_deref(), Some("aX"));
        assert!(b.cancel_composition());
        assert_eq!(b.input_value("q").as_deref(), Some("a"));
    }

    /// A11a: the caret rides the frame as a thin rect appended at publish
    /// time — present in the decorated list, absent from the clean stored one
    /// (so republishes never stack carets), and a no-op with nothing focused.
    #[test]
    fn caret_rect_painted_on_publish_not_in_clean_list() {
        let mut b = test_browser("<html><body><input id='q' value='hello'></body></html>");
        assert!(b.focus_by_id("q"));
        let node = b.focused().expect("focused node");
        let clean = b.last_display_list().expect("display list").clone();
        let run = clean.texts.iter().find(|t| t.node == node.0).expect("run");
        // Caret parked at the end (5 chars): x = run.x + width("hello").
        let expected_x = run.x + 5.0 * run.size * 0.5;
        let is_caret = |r: &kitae_layout::Rect| {
            r.node == node.0 && (r.w - 1.0).abs() < 0.01 && (r.x - expected_x).abs() < 0.5
        };
        // The clean stored list carries no caret rect (border sides are at the
        // box edges, not at the caret's x).
        assert!(
            !clean.rects.iter().any(is_caret),
            "the clean list carries no caret rect"
        );
        let decorated = b.decorate_caret(&clean);
        assert_eq!(
            decorated.rects.len(),
            clean.rects.len() + 1,
            "exactly one caret added"
        );
        // decorate_caret appends, so the caret is the last rect.
        let caret = decorated.rects.last().expect("caret rect");
        assert!(is_caret(caret), "caret rect at the expected x");
        // Each decorate call adds exactly one caret — which is precisely why
        // the engine stores the CLEAN list in last_dl: publishing the
        // decorated clone keeps every republish at a single caret.
        assert_eq!(
            b.decorate_caret(&b.decorate_caret(&clean)).rects.len(),
            decorated.rects.len() + 1,
            "decorating always appends exactly one caret"
        );
        // With nothing focused, decorating is a no-op.
        let b2 = test_browser("<html><body><input id='q' value='hello'></body></html>");
        let clean2 = b2.last_display_list().expect("dl").clone();
        assert_eq!(b2.decorate_caret(&clean2).rects.len(), clean2.rects.len());
    }

    /// A7: clicking a range slider sets its value from the click position on
    /// the track (the same inset-2px track geometry the widget paints),
    /// quantized to `step` and clamped to [min,max].
    #[test]
    fn click_sets_range_value_from_track_position() {
        let mut b = test_browser(
            "<html><body><input id='r' type='range' min='0' max='100' value='50'></body></html>",
        );
        let dl = b.last_display_list().expect("display list");
        // The control box: the only rect 100..200 wide on a bare page.
        let c = dl
            .rects
            .iter()
            .find(|r| r.w > 100.0 && r.w < 200.0)
            .cloned()
            .expect("range control box");
        let at = |frac: f32| c.x + 2.0 + (c.w - 4.0) * frac;
        b.click(at(0.7), c.y + c.h / 2.0);
        assert_eq!(
            b.input_value("r").as_deref(),
            Some("70"),
            "70% down the track"
        );
        b.click(at(0.0), c.y + c.h / 2.0);
        assert_eq!(
            b.input_value("r").as_deref(),
            Some("0"),
            "track start clamps to min"
        );
        b.click(at(1.0), c.y + c.h / 2.0);
        assert_eq!(
            b.input_value("r").as_deref(),
            Some("100"),
            "track end clamps to max"
        );

        // step=25: a click at 55% rounds to the 50 tick.
        let mut b = test_browser(
            "<html><body><input id='r' type='range' min='0' max='100' step='25' value='0'></body></html>",
        );
        let dl = b.last_display_list().expect("display list");
        let c = dl
            .rects
            .iter()
            .find(|r| r.w > 100.0 && r.w < 200.0)
            .cloned()
            .expect("range control box");
        b.click(c.x + 2.0 + (c.w - 4.0) * 0.55, c.y + c.h / 2.0);
        assert_eq!(
            b.input_value("r").as_deref(),
            Some("50"),
            "click snaps to the 25-step tick"
        );
        // The knob moved: a relayout repaint centers it on the new fraction.
        let dl = b.last_display_list().expect("display list after click");
        let knob = dl
            .rects
            .iter()
            .find(|r| (r.w - 10.0).abs() < 0.5 && (r.h - 10.0).abs() < 0.5)
            .expect("knob rect");
        assert!(
            (knob.x + 5.0 - (c.x + 2.0 + (c.w - 4.0) * 0.5)).abs() < 1.0,
            "knob repainted at 50%"
        );
    }

    /// A7: clicking a control focuses it; checkbox and radio clicks toggle;
    /// a radio click unchecks its group peers (mirrors activate_focused).
    #[test]
    fn click_focuses_controls_and_toggles_checks() {
        let mut b = test_browser(
            "<html><body><form id='f'><input id='t' name='t'><input type='checkbox' id='c' name='c'>\
             <input type='radio' id='r1' name='g'><input type='radio' id='r2' name='g'></form></body></html>",
        );
        let dl = b.last_display_list().expect("display list");
        // Click the text input (174px default: 160 + UA padding/border chrome).
        let t = dl
            .rects
            .iter()
            .find(|r| (r.w - 174.0).abs() < 1.0)
            .cloned()
            .expect("text input box");
        // Checkbox and radios paint ~64px-wide boxes ("[ ] on" ≈ 48px label
        // + 16 padding); the w>20 floor skips the 1px side borders.
        let is_check_box = |r: &kitae_layout::Rect| (20.0..80.0).contains(&r.w) && r.h > 18.0;
        let c = dl
            .rects
            .iter()
            .find(|r| is_check_box(r))
            .cloned()
            .expect("checkbox box");
        let r = dl
            .rects
            .iter()
            .filter(|r| is_check_box(r))
            .nth(1)
            .cloned()
            .expect("first radio box");
        let r2 = dl
            .rects
            .iter()
            .filter(|r| is_check_box(r))
            .nth(2)
            .cloned()
            .expect("second radio box");
        b.click(t.x + t.w / 2.0, t.y + t.h / 2.0);
        assert_eq!(
            b.focused_label().as_deref(),
            Some("t"),
            "click focuses the input"
        );
        // Checkbox toggles on click, and again off.
        b.click(c.x + c.w / 2.0, c.y + c.h / 2.0);
        assert_eq!(
            b.input_value("c").as_deref(),
            Some("checked"),
            "click checks it"
        );
        b.click(c.x + c.w / 2.0, c.y + c.h / 2.0);
        assert_eq!(
            b.input_value("c").as_deref(),
            Some(""),
            "second click unchecks"
        );
        // Radios: clicking one clears the other.
        b.click(r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert_eq!(b.input_value("r1").as_deref(), Some("checked"));
        b.click(r2.x + r2.w / 2.0, r2.y + r2.h / 2.0);
        assert_eq!(b.input_value("r2").as_deref(), Some("checked"));
        assert_eq!(
            b.input_value("r1").as_deref(),
            Some(""),
            "group peer unchecked"
        );
        // Clicking a text-like input must NOT submit the form.
        b.click(t.x + t.w / 2.0, t.y + t.h / 2.0);
        assert_eq!(
            b.state().url,
            "https://example.com/",
            "text input click stays put"
        );
    }

    /// A7: clicking a submit button submits the form — and the A10 gate
    /// still blocks (focusing the first invalid control) when validation
    /// fails.
    #[test]
    fn click_submit_button_runs_the_form_flow() {
        let mut b = test_browser(
            "<html><body><form id='f' action='/s'><input id='q' name='q' required>\
             <button id='go'>Go</button></form></body></html>",
        );
        let dl = b.last_display_list().expect("display list");
        // The submit button paints the UA gray 0xF4F5F7FF.
        let go = dl
            .rects
            .iter()
            .find(|r| r.color == 0xF4F5F7FF)
            .cloned()
            .expect("submit button box");
        // required+empty → the A10 gate blocks the click and focuses the field.
        b.click(go.x + go.w / 2.0, go.y + go.h / 2.0);
        assert_eq!(
            b.state().url,
            "https://example.com/",
            "invalid form blocks submit"
        );
        assert_eq!(
            b.focused_label().as_deref(),
            Some("q"),
            "first failure takes focus"
        );
        // Fill the field; the same click now submits.
        assert!(b.set_input_value("q", "hi"));
        let dl = b.last_display_list().expect("display list after fill");
        let go = dl
            .rects
            .iter()
            .find(|r| r.color == 0xF4F5F7FF)
            .cloned()
            .expect("submit button box");
        b.click(go.x + go.w / 2.0, go.y + go.h / 2.0);
        assert_eq!(
            b.state().url,
            "https://example.com/s?q=hi",
            "button click submits"
        );
    }

    /// A7: step_focused — range/number move by `step` clamped to [min,max],
    /// select moves the selected marker one option.
    #[test]
    fn step_focused_adjusts_range_number_select() {
        let mut b = test_browser(
            "<html><body><input id='r' type='range' min='0' max='100' value='50'>\
             <input id='n' type='number' min='0' max='10' step='2'>\
             <select id='s'><option selected>A</option><option>B</option><option>C</option></select></body></html>",
        );
        assert!(b.focus_by_id("r"));
        assert!(b.step_focused(1));
        assert_eq!(b.input_value("r").as_deref(), Some("51"));
        assert!(b.step_focused(-1));
        assert_eq!(b.input_value("r").as_deref(), Some("50"));
        // Clamp at max.
        assert!(b.set_input_value("r", "100"));
        assert!(b.step_focused(1));
        assert_eq!(b.input_value("r").as_deref(), Some("100"), "max clamps");

        assert!(b.focus_by_id("n"));
        assert!(b.step_focused(1), "empty number steps from 0");
        assert_eq!(b.input_value("n").as_deref(), Some("2"), "step=2 quantizes");
        assert!(b.set_input_value("n", "9"));
        assert!(b.step_focused(1));
        assert_eq!(b.input_value("n").as_deref(), Some("10"), "clamps to max");

        assert!(b.focus_by_id("s"));
        assert!(b.step_focused(1));
        assert_eq!(b.input_value("s").as_deref(), Some("B"));
        assert!(b.step_focused(1));
        assert_eq!(b.input_value("s").as_deref(), Some("C"));
        assert!(!b.step_focused(1), "already at the last option");
        assert!(b.step_focused(-1));
        assert_eq!(b.input_value("s").as_deref(), Some("B"));
    }

    /// A10 finish: the `pattern` attribute blocks submit, and the blocked
    /// state paints the validation bubble (dark card + reason text) under
    /// the focused control. Matching the pattern clears both.
    #[test]
    fn pattern_blocks_submit_and_bubble_reports() {
        let mut b = test_browser(
            "<html><body><form id='f' action='/ok'>\
             <input id='z' name='z' pattern='[A-Z]{3}\\d{2}' value=''>\
             <button>go</button></form></body></html>",
        );
        // Mismatch: submit blocked, control focused, bubble painted.
        assert!(b.set_input_value("z", "abc12"));
        assert_eq!(b.submit_form("f"), None, "pattern mismatch blocks submit");
        assert_eq!(
            b.focused_label().as_deref(),
            Some("z"),
            "failure takes focus"
        );
        let dl = b
            .last_display_list()
            .expect("display list after blocked submit");
        assert!(
            dl.texts.iter().any(|t| t.text.contains("format")),
            "bubble reason paints: {:?}",
            dl.texts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>()
        );
        assert!(
            dl.rects.iter().any(|r| r.color == 0x202124FF),
            "bubble card paints"
        );
        // Match: no bubble, and the submit goes through with the value.
        assert!(b.set_input_value("z", "ABC12"));
        let dl = b.last_display_list().expect("display list after fix");
        assert!(
            !dl.rects.iter().any(|r| r.color == 0x202124FF),
            "bubble clears once the value matches"
        );
        let url = b.submit_form("f").unwrap();
        assert_eq!(url, "https://example.com/ok?z=ABC12", "{url}");
    }

    #[test]
    fn checkbox_toggles_and_form_data_collects() {
        let mut b = test_browser(
            "<html><body><form id='f' action='/s'><input id='a' name='x' type='checkbox' value='1'><input id='b' name='y' value='keep'></form></body></html>",
        );
        assert_eq!(b.input_value("a").as_deref(), Some(""));
        assert!(b.focus_by_id("a"));
        assert!(b.activate_focused(), "checkbox toggles on");
        assert_eq!(b.input_value("a").as_deref(), Some("checked"));
        let data = b.form_data("f").unwrap();
        assert!(
            data.contains(&("x".to_string(), "1".to_string())),
            "{data:?}"
        );
        assert!(data.contains(&("y".to_string(), "keep".to_string())));
        assert!(b.activate_focused(), "checkbox toggles off");
        assert_eq!(b.input_value("a").as_deref(), Some(""));
    }

    #[test]
    fn form_submit_builds_get_url() {
        let mut b = test_browser(
            "<html><body><form id='f' action='/search'><input name='q' value='hi there'><input name='n' value='a&b'></form></body></html>",
        );
        let url = b.submit_form("f").unwrap();
        assert!(url.starts_with("https://example.com/search?"), "{url}");
        assert!(url.contains("q=hi%20there"), "{url}");
        assert!(url.contains("n=a%26b"), "{url}");
        assert_eq!(b.form_data("missing"), None);
        assert_eq!(b.submit_form("missing"), None);
    }

    /// A10: constraint validation gates submit_form. A required-empty
    /// control blocks the submit (None), takes focus, and repaints with the
    /// UA error border; filling it lets the same submit through.
    #[test]
    fn failed_submit_blocks_focuses_and_highlights() {
        let mut b = test_browser(
            "<html><body><form id='f' action='/done'>\
             <input id='name' name='n' required value=''>\
             <input id='mail' name='m' type='email' value='x@y.z'>\
             </form></body></html>",
        );
        assert_eq!(
            b.form_failures("f"),
            vec![b
                .page
                .as_ref()
                .unwrap()
                .dom
                .get_element_by_id("name")
                .unwrap()],
            "only the required-empty input fails"
        );
        assert_eq!(b.submit_form("f"), None, "invalid form does not submit");
        assert_eq!(
            b.focused_label().as_deref(),
            Some("name"),
            "first failure takes focus"
        );
        let dl = b
            .last_display_list()
            .expect("display list after blocked submit");
        assert!(
            dl.rects.iter().any(|r| r.color == 0xD93025FF),
            "blocked submit repaints the error border"
        );
        // Fill the field and submit again — the gate must open.
        assert!(b.set_input_value("name", "kitae"));
        let url = b.submit_form("f").unwrap();
        assert_eq!(url, "https://example.com/done?n=kitae&m=x%40y%2Ez", "{url}");
    }

    #[test]
    fn form_submit_posts_body_and_renders_response() {
        // POST to a file:// action serves the file (no HTTP server in unit
        // tests); the point under test is the POST path — urlencoded body,
        // history records the action URL, and the *response* renders as the
        // new page instead of a GET navigation.
        let dir = std::env::temp_dir().join(format!("kitae_post_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let target = dir.join("result.html");
        std::fs::write(&target, "<html><body><p>posted ok</p></body></html>").expect("write");
        let action = format!("file://{}", target.display());
        let html = format!(
            "<html><body><form id=\"f\" method=\"post\" action=\"{action}\">\
             <input type=\"text\" name=\"q\" value=\"hi\"></form></body></html>"
        );
        let mut b = test_browser_at(&html, &local_base_url());
        let url = b.submit_form("f").unwrap();
        assert_eq!(
            url, action,
            "history records the action URL, not a query URL"
        );
        assert_eq!(b.state().url, action);
        let dl = b.last_display_list().expect("display list");
        assert!(
            dl.texts.iter().any(|t| t.text.contains("posted")),
            "POST response renders: {:?}",
            dl.texts.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // -- find-in-page (P1) + accessibility tree (P2) ------------------------

    #[test]
    fn find_matches_across_run_boundaries_case_insensitively() {
        let mut b = test_browser(
            "<html><body><p>The quick brown fox jumps over the lazy dog</p></body></html>",
        );
        // Spans three per-word runs: one match, one rect per run touched.
        let m = b.find_in_page("quick brown fox");
        assert_eq!(m.len(), 1, "one occurrence");
        assert_eq!(m[0].index, 0);
        assert_eq!(m[0].rects.len(), 3, "one rect per run touched");
        for r in &m[0].rects {
            assert!(r.w > 0.0 && r.h > 0.0, "non-degenerate rect");
        }
        // Case folding.
        assert_eq!(b.find_in_page("QUICK BROWN").len(), 1);
        assert_eq!(b.find_in_page("QUICK BROWN")[0].index, 0);
        // Misses and blanks.
        assert!(b.find_in_page("zebra").is_empty());
        assert!(b.find_in_page("").is_empty());
        assert!(b.find_in_page("   ").is_empty());
    }

    /// Q3: `text-transform` folds case BEFORE the display list is built, so
    /// runs carry the transformed text and find-in-page searches what the
    /// user sees (plan decision; browsers match source — recorded deviation).
    #[test]
    fn find_indexes_transformed_text_q3() {
        let mut b = test_browser(
            "<html><body><p style=\"text-transform:uppercase\">hidden treasure</p></body></html>",
        );
        let dl = b.last_display_list().expect("display list");
        assert!(
            dl.texts.iter().any(|t| t.text == "HIDDEN"),
            "runs carry uppercase text: {:?}",
            dl.texts.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
        assert_eq!(b.find_in_page("hidden treasure").len(), 1);
    }

    #[test]
    fn find_query_steps_and_clears_with_republish() {
        let mut b = test_browser("<html><body><p>one two one two one</p></body></html>");
        b.set_find_query("one");
        assert_eq!(b.find_query(), "one");
        assert_eq!(b.find_matches().len(), 3);
        assert_eq!(b.find_active(), Some(0));
        b.find_next();
        assert_eq!(b.find_active(), Some(1));
        b.find_next();
        b.find_next();
        assert_eq!(b.find_active(), Some(0), "wraps around");
        b.find_prev();
        assert_eq!(b.find_active(), Some(2), "wraps backward");
        b.clear_find();
        assert_eq!(b.find_query(), "");
        assert!(b.find_matches().is_empty());
        assert_eq!(b.find_active(), None);
    }

    #[test]
    fn find_query_survives_navigation_and_resolves_again() {
        let mut b = test_browser("<html><body><p>apple pie</p></body></html>");
        b.set_find_query("apple");
        assert_eq!(b.find_matches().len(), 1);
        b.load_html(
            "<html><body><p>nothing here</p></body></html>",
            "https://example.com/2",
        );
        assert_eq!(b.find_query(), "apple", "query survives navigation");
        assert!(b.find_matches().is_empty(), "no match on the new page");
        assert_eq!(b.find_active(), None);
    }

    #[test]
    fn accessibility_tree_roles_labels_and_bounds() {
        let mut b = test_browser(
            "<html><body>\
             <h1 id=\"t\">Hello</h1>\
             <p>See <a id=\"l\" href=\"/next\">the docs</a> now.</p>\
             <button id=\"b\">Save</button>\
             <img id=\"i\" src=\"pic.png\" alt=\"a photo\">\
             </body></html>",
        );
        // Focus the link: the AX tree must map it.
        assert!(b.focus_by_id("l"));
        assert_eq!(
            b.accessibility_focused(),
            Some("l"),
            "focused id maps into the tree"
        );
        // Re-resolve focus into the cached tree (focus changed post-layout).
        let tree = b.accessibility_tree();
        assert_eq!(tree.len(), 1, "single Document root");
        assert_eq!(tree[0].role, AxRole::Document);
        let mut roles = Vec::new();
        fn walk(n: &AxNode, roles: &mut Vec<(AxRole, String, bool)>) {
            roles.push((n.role, n.label.clone(), n.w > 0.0 && n.h > 0.0));
            for c in &n.children {
                walk(c, roles);
            }
        }
        walk(&tree[0], &mut roles);
        let has = |role: AxRole, label: &str| {
            roles
                .iter()
                .any(|(r, l, _)| *r == role && l.contains(label))
        };
        assert!(has(AxRole::Heading, "Hello"), "h1: {roles:?}");
        assert!(has(AxRole::Link, "the docs"), "a: {roles:?}");
        assert!(has(AxRole::Button, "Save"), "button: {roles:?}");
        assert!(has(AxRole::Image, "a photo"), "img alt: {roles:?}");
        // Rendered text resolves bounds; the link matched its hit area.
        let link = roles.iter().find(|(r, _, _)| *r == AxRole::Link).unwrap();
        assert!(link.2, "link has layout bounds");
        let img = roles.iter().find(|(r, _, _)| *r == AxRole::Image).unwrap();
        assert!(img.2, "image has layout bounds");
    }

    #[test]
    fn accessibility_honors_explicit_roles_labels_and_hidden_subtrees() {
        let mut b = test_browser(
            "<html><body>\
             <div role=\"button\" aria-label=\"Custom action\">x</div>\
             <label for=\"name\">Full name</label><input id=\"name\" type=\"text\">\
             <div aria-labelledby=\"name-heading\">Named</div><h2 id=\"name-heading\">Account</h2>\
             <div aria-hidden=\"true\"><button>Hidden action</button></div>\
             </body></html>",
        );
        let tree = b.accessibility_tree();
        fn walk(n: &AxNode, out: &mut Vec<(AxRole, String)>) {
            out.push((n.role, n.label.clone()));
            for child in &n.children {
                walk(child, out);
            }
        }
        let mut nodes = Vec::new();
        walk(&tree[0], &mut nodes);
        assert!(
            nodes.contains(&(AxRole::Button, "Custom action".to_string())),
            "{nodes:?}"
        );
        assert!(
            nodes.iter().any(|(_, label)| label == "Full name"),
            "{nodes:?}"
        );
        assert!(
            nodes.iter().any(|(_, label)| label == "Account"),
            "{nodes:?}"
        );
        assert!(
            !nodes.iter().any(|(_, label)| label == "Hidden action"),
            "{nodes:?}"
        );
    }

    #[test]
    fn password_values_are_not_exposed_in_accessibility_labels() {
        let mut b = test_browser(
            "<html><body>\
             <input id=\"secret\" type=\"password\" value=\"supersecret\">\
             <input id=\"key\" type=\"password\" placeholder=\"Access key\">\
             </body></html>",
        );
        b.set_input_value("secret", "new-secret");
        let tree = b.accessibility_tree();
        fn walk(n: &AxNode, labels: &mut Vec<String>) {
            labels.push(n.label.clone());
            for child in &n.children {
                walk(child, labels);
            }
        }
        let mut labels = Vec::new();
        walk(&tree[0], &mut labels);
        assert!(labels.iter().any(|label| label == "Password"), "{labels:?}");
        assert!(
            labels.iter().any(|label| label == "Access key"),
            "{labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label.contains("secret")),
            "{labels:?}"
        );
    }

    #[test]
    fn aria_live_marks_elements_as_live_regions() {
        // H4: an `aria-live` element of any base tag surfaces the live-region
        // role so hosts can diff its label across frames and announce
        // updates; `aria-live="off"` (and elements without the attribute)
        // keep their base roles.
        let mut b = test_browser(
            "<html><body>\
             <div id=\"status\" aria-live=\"polite\">Saved</div>\
             <div id=\"alert\" aria-live=\"assertive\">Expired</div>\
             <div id=\"quiet\" aria-live=\"off\">Silent</div>\
             <div id=\"plain\">Other</div>\
             </body></html>",
        );
        let tree = b.accessibility_tree();
        let mut roles = Vec::new();
        fn walk(n: &AxNode, roles: &mut Vec<(AxRole, String)>) {
            roles.push((n.role, n.label.clone()));
            for c in &n.children {
                walk(c, roles);
            }
        }
        walk(&tree[0], &mut roles);
        // polite/assertive share the single live-region role; each keeps its
        // text content as the label.
        let live = roles
            .iter()
            .filter(|(r, _)| *r == AxRole::LiveRegion)
            .map(|(_, l)| l.as_str())
            .collect::<Vec<_>>();
        assert!(live.contains(&"Saved"), "polite div is live: {roles:?}");
        assert!(
            live.contains(&"Expired"),
            "assertive div is live: {roles:?}"
        );
        assert_eq!(live.len(), 2, "no stray live regions: {roles:?}");
        // aria-live=off and the plain div keep their base roles: they show
        // up as Groups (labelled by text), never as live regions.
        let groups = roles
            .iter()
            .filter(|(r, _)| *r == AxRole::Group)
            .map(|(_, l)| l.as_str())
            .collect::<Vec<_>>();
        assert!(
            groups.contains(&"Silent"),
            "aria-live=off stays a group: {roles:?}"
        );
        assert!(
            groups.contains(&"Other"),
            "plain div stays a group: {roles:?}"
        );
    }

    #[test]
    fn accessibility_semantics_include_values_states_and_levels() {
        let b = test_browser(
            "<html><body>\
             <h2 id=\"heading\">Heading</h2>\
             <input id=\"text\" value=\"hello\">\
             <input id=\"check\" type=\"checkbox\" checked>\
             <input id=\"radio\" type=\"radio\" aria-checked=\"mixed\">\
             <input id=\"range\" type=\"range\" min=\"2\" max=\"8\" value=\"5\">\
             <button id=\"off\" disabled>Off</button>\
             <details open><summary id=\"sum\">More</summary><p>Body</p></details>\
             <select id=\"select\"><option id=\"one\">One</option>\
             <option id=\"two\" selected>Two</option></select>\
             <div id=\"live\" aria-live=\"polite\">Saved</div>\
             <div id=\"alert\" role=\"alert\">Bad</div>\
             <main id=\"main\">Main</main>\
             <ul id=\"list\"><li id=\"item\">Item</li></ul>\
             <textarea id=\"area\">Text</textarea>\
             </body></html>",
        );
        let tree = b.accessibility_tree();
        let mut nodes = Vec::new();
        fn collect<'a>(node: &'a AxNode, nodes: &mut Vec<&'a AxNode>) {
            nodes.push(node);
            for child in &node.children {
                collect(child, nodes);
            }
        }
        collect(&tree[0], &mut nodes);
        let get = |id: &str| {
            nodes
                .iter()
                .copied()
                .find(|node| node.id == id)
                .unwrap_or_else(|| panic!("missing AX node {id}"))
        };

        let heading = get("heading");
        assert_eq!(heading.role, AxRole::Heading);
        assert_eq!(heading.heading_level, 2);
        let text = get("text");
        assert_eq!(text.role, AxRole::Textbox);
        assert_eq!(text.value.as_deref(), Some("hello"));
        assert_eq!(get("check").role, AxRole::Checkbox);
        assert_eq!(get("check").checked, AxChecked::Checked);
        assert_eq!(get("radio").role, AxRole::RadioButton);
        assert_eq!(get("radio").checked, AxChecked::Mixed);
        let range = get("range");
        assert_eq!(range.role, AxRole::Slider);
        assert_eq!(range.value.as_deref(), Some("5"));
        assert_eq!(range.value_min, Some(2.0));
        assert_eq!(range.value_max, Some(8.0));
        assert!(get("off").disabled);
        let summary = get("sum");
        assert_eq!(summary.role, AxRole::DisclosureTriangle);
        assert_eq!(summary.expanded, Some(true));
        assert_eq!(get("select").role, AxRole::ComboBox);
        assert_eq!(get("select").value.as_deref(), Some("Two"));
        assert_eq!(get("two").selected, Some(true));
        let live = get("live");
        assert_eq!(live.role, AxRole::LiveRegion);
        assert_eq!(live.live_politeness, AxLivePoliteness::Polite);
        let alert = get("alert");
        assert_eq!(alert.role, AxRole::Alert);
        assert_eq!(alert.live_politeness, AxLivePoliteness::Assertive);
        assert_eq!(get("main").role, AxRole::Main);
        assert_eq!(get("list").role, AxRole::List);
        assert_eq!(get("item").role, AxRole::ListItem);
        assert_eq!(get("area").role, AxRole::TextArea);
        assert_eq!(get("area").value.as_deref(), Some("Text"));
    }

    #[test]
    fn selection_select_all_and_clear() {
        let mut b = test_browser("<html><body><p>alpha</p><p>beta</p></body></html>");
        assert_eq!(b.selection_text(), None);
        let text = b.select_all();
        assert!(text.contains("alpha") && text.contains("beta"), "{text:?}");
        assert_eq!(b.selection_text().as_deref(), Some(text.as_str()));
        assert_eq!(b.state().selection.as_deref(), Some(text.as_str()));
        b.clear_selection();
        assert_eq!(b.selection_text(), None);
    }

    #[test]
    fn focus_and_selection_publish_without_relayout() {
        let mut b = test_browser("<html><body><a href='/x' id='l'>go</a><p>text</p></body></html>");
        let base = b.repaint_count();
        b.focus_next();
        assert_eq!(b.repaint_count(), base, "focus never relayouts");
        assert_eq!(b.focused_label().as_deref(), Some("l"));
        b.select_all();
        assert_eq!(b.repaint_count(), base, "selection never relayouts");
        b.set_input_value("l", "x");
        // <a> is not a value control: no relayout.
        assert_eq!(b.repaint_count(), base);
    }

    // ── Web fonts (N5) ──

    #[test]
    fn unsupported_font_formats_are_skipped() {
        assert!(!font_format_supported(Some("eot")));
        assert!(!font_format_supported(Some("svg")));
        assert!(font_format_supported(Some("woff2")));
        assert!(font_format_supported(Some("truetype")));
        assert!(font_format_supported(None));
    }

    #[test]
    fn garbage_font_url_registers_nothing() {
        let face = kitae_css::FontFace {
            family: "KitaeNopeFace".into(),
            srcs: vec![kitae_css::FontSrc {
                url: "file:///definitely/not/here.ttf".into(),
                format: None,
            }],
            ..Default::default()
        };
        assert!(!ensure_web_font(
            &face,
            "https://example.com/",
            "https://example.com/"
        ));
        assert!(kitae_shape::font::registered_web_fonts()
            .iter()
            .all(|f| f != "KitaeNopeFace"));
    }

    /// Real Core Text installation from a system font file (macOS-only by
    /// standing decision; exercises fetch → stage → register → registry).
    #[cfg(target_os = "macos")]
    #[test]
    fn system_font_file_registers_through_loader() {
        let family = format!("KitaeSysFace{}", std::process::id());
        let face = kitae_css::FontFace {
            family: family.clone(),
            srcs: vec![kitae_css::FontSrc {
                url: "file:///System/Library/Fonts/Helvetica.ttc".into(),
                format: None,
            }],
            ..Default::default()
        };
        assert!(
            ensure_web_font(&face, "https://example.com/", "https://example.com/"),
            "CT registration works headless"
        );
        assert!(kitae_shape::font::registered_web_fonts()
            .iter()
            .any(|f| f == &family));
        // Second load short-circuits through the registry (no refetch).
        let mut b = test_browser(&format!(
            "<html><head><style>@font-face {{ font-family: '{family}'; src: url(file:///System/Library/Fonts/Helvetica.ttc); }}</style></head><body><p>hi</p></body></html>"
        ));
        assert!(
            b.ensure_page_fonts().is_empty(),
            "already registered: nothing new"
        );
    }

    /// Q11: a real WOFF2 registers through the in-memory CGFont path
    /// (Core Text cannot parse the container itself).
    #[cfg(target_os = "macos")]
    #[test]
    fn woff2_font_file_registers_in_memory() {
        let family = format!("KitaeWoff2Face{}", std::process::id());
        let url = format!(
            "file://{}/../kitae_media/tests/fixtures/tiny.woff2",
            env!("CARGO_MANIFEST_DIR")
        );
        let face = kitae_css::FontFace {
            family: family.clone(),
            srcs: vec![kitae_css::FontSrc {
                url,
                format: Some("woff2".into()),
            }],
            ..Default::default()
        };
        assert!(
            ensure_web_font(&face, "https://example.com/", "https://example.com/"),
            "woff2 decoded + registered"
        );
        assert!(kitae_shape::font::registered_web_fonts()
            .iter()
            .any(|f| f == &family));
    }

    /// Q11: a container that fails to decode/register falls back to the
    /// next `src:` entry (spec behavior).
    #[cfg(target_os = "macos")]
    #[test]
    fn broken_woff2_falls_back_to_next_src() {
        let dir = std::env::temp_dir().join(format!("kitae_q11_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.woff2");
        std::fs::write(&bad, b"wOF2\x00\x01not really a font").unwrap();
        let good = dir.join("good.ttf");
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../kitae_media/tests/fixtures/tiny.ttf"
            ),
            &good,
        )
        .unwrap();
        let family = format!("KitaeFallFace{}", std::process::id());
        let face = kitae_css::FontFace {
            family: family.clone(),
            srcs: vec![
                kitae_css::FontSrc {
                    url: format!("file://{}", bad.display()),
                    format: Some("woff2".into()),
                },
                kitae_css::FontSrc {
                    url: format!("file://{}", good.display()),
                    format: Some("truetype".into()),
                },
            ],
            ..Default::default()
        };
        assert!(
            ensure_web_font(&face, "https://example.com/", "https://example.com/"),
            "fallback src registers"
        );
        assert!(kitae_shape::font::registered_web_fonts()
            .iter()
            .any(|f| f == &family));
    }

    /// B11: a `unicode-range` face covering none of the page's code points
    /// is never fetched; one that matches installs.
    #[cfg(target_os = "macos")]
    #[test]
    fn b11_out_of_range_face_is_not_installed() {
        let family = format!("KitaeRangeFace{}", std::process::id());
        let mk = |range: &str, text: &str| {
            test_browser_at(
                &format!(
                    "<html><head><style>@font-face {{ font-family: '{family}'; \
                     src: url(file:///System/Library/Fonts/Avenir.ttc); \
                     unicode-range: {range}; }}</style></head><body><p>{text}</p></body></html>"
                ),
                &local_base_url(),
            )
        };
        // Cyrillic-only face on a pure-ASCII page → skipped.
        let mut b = mk("U+400-4FF", "plain ascii only");
        assert!(
            b.ensure_page_fonts().is_empty(),
            "out-of-range face not fetched"
        );
        assert!(kitae_shape::font::registered_web_fonts()
            .iter()
            .all(|f| f != &family));
        // The same face with Cyrillic text → installed (the load path
        // already fetched it; an explicit call reports nothing new).
        let mut b = mk("U+400-4FF", "Кирилица");
        assert!(b.ensure_page_fonts().is_empty(), "load path installed it");
        assert!(kitae_shape::font::registered_web_fonts()
            .iter()
            .any(|f| f == &family));
    }

    /// B11: `font-display: optional` faces are never fetched — the page
    /// draws with system fallbacks.
    #[cfg(target_os = "macos")]
    #[test]
    fn b11_optional_display_skips_fetch() {
        let family = format!("KitaeOptFace{}", std::process::id());
        let mut b = test_browser_at(
            &format!(
                "<html><head><style>@font-face {{ font-family: '{family}'; \
                 src: url(file:///System/Library/Fonts/Avenir.ttc); \
                 font-display: optional; }}</style></head><body><p>hi</p></body></html>"
            ),
            &local_base_url(),
        );
        assert!(b.ensure_page_fonts().is_empty());
        assert!(kitae_shape::font::registered_web_fonts()
            .iter()
            .all(|f| f != &family));
    }

    /// B11: the install skip is per face (family *and* descriptors), not
    /// per family — a second weight or slant of a loaded family still
    /// installs, while a repeat of the same descriptors and the B11
    /// `unicode-range` / `font-display` filters do not. No real font is
    /// involved, so this runs on every platform.
    #[test]
    fn font_face_install_is_per_weight_and_style() {
        use kitae_css::{FontDisplay, FontStyle};
        let family = format!("KitaeNeedFace{}", std::process::id());
        let face = |weight: u32, style: FontStyle| kitae_css::FontFace {
            family: family.clone(),
            srcs: vec![kitae_css::FontSrc {
                url: "u.ttf".into(),
                format: None,
            }],
            font_weight: weight,
            font_style: style,
            ..Default::default()
        };
        let reg = face(400, FontStyle::Normal);
        let bold = face(700, FontStyle::Normal);
        let ital = face(400, FontStyle::Italic);
        assert!(face_needs_install(&reg, "hi"), "unseen face installs");
        kitae_shape::font::register_web_font_face(
            &family,
            reg.font_weight,
            reg.font_style.keyword(),
        );
        assert!(
            !face_needs_install(&reg, "hi"),
            "same descriptors → no refetch"
        );
        assert!(
            face_needs_install(&bold, "hi"),
            "a second weight is its own face"
        );
        assert!(
            face_needs_install(&ital, "hi"),
            "a second slant is its own face"
        );
        // A bold-only family still installs: the descriptors name the face,
        // not the weight the page happens to ask for (matching falls back
        // inside the family — see `StyleEngine::font_face_for`).
        let lone = format!("KitaeLoneFace{}", std::process::id());
        let bold_only = kitae_css::FontFace {
            family: lone,
            font_weight: 700,
            ..face(400, FontStyle::Normal)
        };
        assert!(face_needs_install(&bold_only, "hi"));
        // The B11 filters still run per face.
        let mut ranged = bold.clone();
        ranged.unicode_ranges = vec![(0x400, 0x4FF)];
        assert!(!face_needs_install(&ranged, "plain ascii"));
        assert!(face_needs_install(&ranged, "Кирилица"));
        let mut optional = ital.clone();
        optional.font_display = FontDisplay::Optional;
        assert!(!face_needs_install(&optional, "hi"));
    }

    /// B11: two weights of one family both install through the real Core
    /// Text path — the family-level skip used to drop the second face
    /// before it was ever fetched. Uses two distinct system files so each
    /// registration succeeds (a file already registered in this process is
    /// refused by Core Text).
    #[cfg(target_os = "macos")]
    #[test]
    fn b11_two_weights_of_one_family_both_install() {
        let family = format!("KitaeTwoWeightFace{}", std::process::id());
        let mut b = test_browser_at(
            &format!(
                "<html><head><style>\
                 @font-face {{ font-family: '{family}'; font-weight: 400; \
                  src: url(file:///System/Library/Fonts/Courier.ttc); }}\
                 @font-face {{ font-family: '{family}'; font-weight: 700; font-style: italic; \
                  src: url(file:///System/Library/Fonts/Menlo.ttc); }}\
                 </style></head><body><p>hi</p></body></html>"
            ),
            &local_base_url(),
        );
        assert!(
            kitae_shape::font::web_font_registered(&family, 400, "normal"),
            "regular face installed"
        );
        assert!(
            kitae_shape::font::web_font_registered(&family, 700, "italic"),
            "bold italic face installed too"
        );
        // The family list is still one entry, and a second pass is a no-op.
        assert_eq!(
            kitae_shape::font::registered_web_fonts()
                .iter()
                .filter(|f| f.eq_ignore_ascii_case(&family))
                .count(),
            1
        );
        assert!(b.ensure_page_fonts().is_empty(), "nothing new to fetch");
    }

    // ── Dynamic pseudo-classes (N6) ──

    fn hover_browser() -> Browser {
        test_browser(
            "<html><head><style>a:hover { color: #ff0000 } button:active { color: #00ff00 }</style></head>\
             <body><a href='/x' id='l'>go</a><button id='b'>ok</button></body></html>",
        )
    }

    fn link_color(b: &Browser) -> u32 {
        b.last_dl
            .as_ref()
            .and_then(|dl| dl.texts.iter().find(|t| t.text == "go"))
            .map(|t| t.color)
            .unwrap_or(0)
    }

    #[test]
    fn hover_applies_and_clears_with_repaint() {
        let mut b = hover_browser();
        assert_eq!(link_color(&b), 0x0B57D0FF, "resting: UA link blue");
        let base = b.repaint_count();
        assert!(b.set_hover_id(Some("l")));
        assert_eq!(b.repaint_count(), base + 1, "hover restyles");
        assert_eq!(link_color(&b), 0xFF0000FF);
        // Setting the same node again is a no-op (no repaint storm on
        // mousemove).
        assert!(!b.set_hover_id(Some("l")));
        assert_eq!(b.repaint_count(), base + 1);
        assert!(b.set_hover_id(None));
        assert_eq!(link_color(&b), 0x0B57D0FF, "cleared");
        // Unknown ids leave state untouched.
        assert!(!b.set_hover_id(Some("missing")));
        assert_eq!(link_color(&b), 0x0B57D0FF);
    }

    #[test]
    fn active_matches_pressed_button_and_resets_on_navigate() {
        let mut b = hover_browser();
        let btn = |b: &Browser| {
            b.last_dl
                .as_ref()
                .and_then(|dl| dl.texts.iter().find(|t| t.text == "ok"))
                .map(|t| t.color)
                .unwrap_or(0)
        };
        assert!(b.set_active_id(Some("b")));
        assert_eq!(btn(&b), 0x00FF00FF);
        assert!(b.set_active_id(None));
        // Navigate-away clears hover/active like focus.
        assert!(b.set_hover_id(Some("l")));
        b.navigate("about:blank");
        assert!(b.hover.is_none() && b.active.is_none());
    }

    /// I1: on a page whose sheets never key off hover/active, the state
    /// updates but the full relayout (and its repaint) is skipped.
    #[test]
    fn hover_without_interaction_rules_skips_relayout() {
        let mut b = test_browser(
            "<html><head><style>p { color: #123456 }</style></head>\
             <body><a href='/x' id='l'>go</a><button id='b'>ok</button></body></html>",
        );
        let base = b.repaint_count();
        assert!(b.set_hover_id(Some("l")));
        assert_eq!(
            b.repaint_count(),
            base,
            "insensitive page: no relayout on hover"
        );
        assert!(b.set_active_id(Some("b")));
        assert_eq!(
            b.repaint_count(),
            base,
            "insensitive page: no relayout on active"
        );
        assert!(b.set_hover_id(None));
        assert_eq!(b.repaint_count(), base);
    }

    // ── Environment media features (Q6) ──

    #[test]
    fn environment_flip_restyles_pages_without_reload() {
        let mut b = test_browser(
            "<html><head><style>@media (prefers-color-scheme: dark) { #t { color: #101010 } }</style>\
             </head><body><p id='t'>scheme</p></body></html>",
        );
        let text_color = |b: &Browser| {
            b.last_dl
                .as_ref()
                .and_then(|dl| dl.texts.iter().find(|t| t.text == "scheme"))
                .map(|t| t.color)
                .unwrap_or(0)
        };
        let dark = EnvironmentState {
            color_scheme: ColorScheme::Dark,
            ..Default::default()
        };
        assert_eq!(text_color(&b), 0x1A1D23FF, "default environment is light");
        let base = b.repaint_count();
        assert!(b.set_environment(dark), "change restyles");
        assert_eq!(b.repaint_count(), base + 1);
        assert_eq!(text_color(&b), 0x101010FF, "dark rule applies, no reload");
        // Same environment again is a no-op (no repaint storm).
        assert!(!b.set_environment(dark));
        assert_eq!(b.repaint_count(), base + 1);
        assert!(b.set_environment(EnvironmentState::default()));
        assert_eq!(text_color(&b), 0x1A1D23FF, "back to light");
    }

    #[test]
    fn environment_survives_navigation_and_applies_to_new_pages() {
        let mut b = test_browser("<p>first</p>");
        assert!(b.set_environment(EnvironmentState {
            color_scheme: ColorScheme::Dark,
            reduced_motion: true,
            ..Default::default()
        }));
        b.load_html(
            "<style>@media (prefers-reduced-motion: reduce) { p { letter-spacing: 3px } }</style><p>second</p>",
            "https://example.com/second",
        );
        let spacing = b
            .last_dl
            .as_ref()
            .and_then(|dl| dl.texts.iter().find(|t| t.text == "second"))
            .map(|t| t.letter_spacing)
            .unwrap_or(0.0);
        assert!(
            (spacing - 3.0).abs() < 0.01,
            "environment applies to pages loaded after the flip"
        );
    }

    // ── Declarative animations (N8) ──

    #[test]
    fn animations_sample_per_node() {
        let mut b = test_browser(
            "<html><head><style>@keyframes fade { from { opacity: 0 } to { opacity: 1 } } \
             #a { animation: fade 2s linear; }</style></head>\
             <body><div id='a'>x</div><div id='b'>y</div></body></html>",
        );
        let node_a = b.page.as_ref().unwrap().dom.get_element_by_id("a").unwrap();
        // Mid-animation: only #a samples, opacity halfway.
        let at1 = b.sample_animations(1.0);
        assert_eq!(at1.len(), 1, "{at1:?}");
        assert_eq!(at1[0].0, node_a);
        assert_eq!(at1[0].1, "opacity");
        assert_eq!(at1[0].2, "0.500");
        // Before start (t=0 samples offset 0 → opacity 0.000 — active).
        let at0 = b.sample_animations(0.0);
        assert_eq!(at0.len(), 1);
        assert_eq!(at0[0].2, "0.000");
        // After the single iteration: nothing (no fill model).
        assert!(b.sample_animations(3.0).is_empty());
    }

    // ── I2: incremental layout through the browser ──────────────────────────

    fn frag_counters(b: &Browser) -> (u64, u64) {
        let page = b.page.as_ref().unwrap();
        (page.frag_cache.hits, page.frag_cache.misses)
    }

    /// An unchanged document relaid out reuses every body-level fragment —
    /// hits grow, misses stand still, and the published frame is identical.
    #[test]
    fn i2_browser_relayout_reuses_fragments() {
        let mut b = test_browser(
            "<html><body><p>alpha bravo charlie</p><div style='height:40px;background:#eeeeee'></div><p>delta echo</p></body></html>",
        );
        // load_html's own relayout stored the fragments (all misses).
        assert!(
            b.page.as_ref().unwrap().frag_cache.len() >= 3,
            "cold pass stored entries"
        );
        let (paint0, _) = (b.repaint_count(), b.content_height());
        let (h0, m0) = frag_counters(&b);
        b.relayout();
        let (h1, m1) = frag_counters(&b);
        assert!(h1 - h0 >= 3, "warm relayout hits, got {}", h1 - h0);
        assert_eq!(m1, m0, "warm relayout adds no misses");
        assert_eq!(b.repaint_count(), paint0 + 1);
        let dl1 = b.last_dl.clone().unwrap();
        b.relayout();
        let dl2 = b.last_dl.clone().unwrap();
        assert_eq!(dl2.content_height, dl1.content_height);
        assert_eq!(dl2.texts.len(), dl1.texts.len());
    }

    /// A hover move is an interaction-state change: the fragment cache
    /// clears, the full relayout applies :hover styles, and once the state
    /// settles the cache warms again.
    #[test]
    fn i2_hover_invalidates_and_restores_fragments() {
        let mut b = test_browser(
            "<html><head><style>p { color: #000000 } p:hover { color: #ff0000 }</style></head>\
             <body><p id='t'>hover me</p></body></html>",
        );
        b.relayout(); // warm
        let p = b.page.as_ref().unwrap().dom.get_element_by_id("t").unwrap();

        assert!(b.set_hover(Some(p)), "hover triggers a relayout");
        // State changed → cache cleared → everything re-measured.
        let (_, m_after_hover) = frag_counters(&b);
        let dl = b.last_dl.clone().unwrap();
        let run = dl
            .texts
            .iter()
            .find(|t| t.text.contains("hover"))
            .expect("text run");
        assert_eq!(
            run.color, 0xFF0000FF,
            ":hover color applied, got {:08X}",
            run.color
        );

        assert!(b.set_hover(None), "unhover triggers a relayout");
        let (_, m_after_unhover) = frag_counters(&b);
        assert!(
            m_after_unhover > m_after_hover,
            "each state change re-measures"
        );
        let dl = b.last_dl.clone().unwrap();
        let run = dl.texts.iter().find(|t| t.text.contains("hover")).unwrap();
        assert_eq!(
            run.color, 0x000000FF,
            "base color restored, got {:08X}",
            run.color
        );

        // State settled: the next relayout is warm again.
        let (h0, m0) = frag_counters(&b);
        b.relayout();
        let (h1, m1) = frag_counters(&b);
        assert!(
            h1 > h0 && m1 == m0,
            "warm after settle: {:?}",
            (h1 - h0, m1 - m0)
        );
    }

    /// While a CSS transition runs, eased styles change every tick while
    /// node revisions stand still — the engine must bypass (and clear) the
    /// fragment cache instead of serving stale frames.
    #[test]
    fn i2_running_transitions_bypass_the_fragment_cache() {
        let mut b = test_browser(
            "<html><head><style>p { color: #000000; transition: color 0.05s } \
             .go { color: #0000ff }</style></head><body><p id='t'>shift</p></body></html>",
        );
        b.relayout(); // warm
        assert!(frag_counters(&b).0 > 0, "warm before the transition");
        // Changing the class starts a color transition on the relayout diff.
        let t = b.page.as_ref().unwrap().dom.get_element_by_id("t").unwrap();
        b.page.as_mut().unwrap().dom.set_attr(t, "class", "go");
        let (h0, _) = frag_counters(&b);
        b.relayout();
        assert!(!b.transitions.is_empty(), "transition started");
        let (h1, _) = frag_counters(&b);
        assert_eq!(h1, h0, "animation relayouts never serve cached fragments");
        // Ticks during the animation stay on the plain path.
        b.tick_transitions_at(0.02);
        assert_eq!(frag_counters(&b).0, h0, "still bypassing mid-animation");
    }

    // ── Track B / B5: page-as-data extraction ────────────────────────────

    fn blocks_of(doc: &kitae_json::Value) -> &[kitae_json::Value] {
        doc.get("blocks").and_then(|v| v.as_arr()).unwrap_or(&[])
    }

    fn kinds_of(doc: &kitae_json::Value) -> Vec<&str> {
        blocks_of(doc)
            .iter()
            .map(|b| b.get("kind").and_then(|v| v.as_str()).unwrap_or(""))
            .collect()
    }

    fn text_of_block(block: &kitae_json::Value) -> &str {
        block.get("text").and_then(|v| v.as_str()).unwrap_or("")
    }

    fn rect_of(block: &kitae_json::Value) -> Vec<f64> {
        block
            .get("rect")
            .and_then(|r| r.as_arr())
            .map(|a| a.iter().filter_map(|v| v.as_f64()).collect())
            .unwrap_or_default()
    }

    /// The extraction fixture: every block kind the walker knows, plus a
    /// hidden subtree and a pruned `<style>` body to prove what is skipped.
    const B5_PAGE: &str = "<html lang=\"en\"><head><title>Tea Guide</title>\
<meta name=\"description\" content=\"How to brew tea\">\
<style>p { color: #123456 }</style></head><body>\
<h1>Brewing</h1>\
<p>Water matters. <a href=\"/temp\">Temperature</a> decides everything.</p>\
<h2>Steps</h2>\
<ol><li>Boil</li><li>Steep</li></ol>\
<table><tr><th>Leaf</th><th>Time</th></tr><tr><td>Green</td><td>2m</td></tr></table>\
<img src=\"/pot.png\" alt=\"A teapot\">\
<form action=\"/brew\" method=\"post\"><label for=\"amt\">Amount</label>\
<input id=\"amt\" name=\"amount\" type=\"number\" required placeholder=\"grams\"></form>\
<div style=\"display: none\"><p>hidden text</p></div>\
</body></html>";

    #[test]
    fn b5_extraction_reports_blocks_sections_and_provenance() {
        let b = test_browser_at(B5_PAGE, "https://tea.test/guide");
        let doc = extract::document_json(&b);

        assert_eq!(
            doc.get("url").and_then(|v| v.as_str()),
            Some("https://tea.test/guide")
        );
        assert_eq!(doc.get("title").and_then(|v| v.as_str()), Some("Tea Guide"));
        assert_eq!(
            doc.get("description").and_then(|v| v.as_str()),
            Some("How to brew tea")
        );
        assert_eq!(doc.get("lang").and_then(|v| v.as_str()), Some("en"));

        // Document order, one block per semantic unit — and the hidden
        // subtree plus the `<style>` body appear nowhere.
        assert_eq!(
            kinds_of(&doc),
            vec![
                "heading",
                "paragraph",
                "heading",
                "list",
                "table",
                "image",
                "form"
            ]
        );
        let serialized = doc.to_json();
        assert!(
            !serialized.contains("hidden text") && !serialized.contains("123456"),
            "pruned content stayed out: {serialized}"
        );

        // Provenance: every block cites the DOM node it came from, and the
        // text blocks carry a real box from the last published frame.
        for block in blocks_of(&doc) {
            let node = block.get("node").and_then(|v| v.as_f64()).unwrap_or(0.0);
            assert!(node > 0.0, "block cites a node: {block:?}");
        }
        let rect = rect_of(&blocks_of(&doc)[0]);
        assert_eq!(
            rect.len(),
            4,
            "heading has a rect: {:?}",
            blocks_of(&doc)[0]
        );
        assert!(
            rect[2] > 0.0 && rect[3] > 0.0,
            "heading box is real: {rect:?}"
        );

        // The paragraph: text and its resolved outbound link.
        let para = &blocks_of(&doc)[1];
        assert_eq!(
            text_of_block(para),
            "Water matters. Temperature decides everything."
        );
        let links = para
            .get("links")
            .and_then(|v| v.as_arr())
            .expect("paragraph links");
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].get("href").and_then(|v| v.as_str()),
            Some("https://tea.test/temp")
        );
        assert_eq!(
            links[0].get("text").and_then(|v| v.as_str()),
            Some("Temperature")
        );

        // The list keeps its items and order.
        let list = &blocks_of(&doc)[3];
        assert_eq!(list.get("ordered").and_then(|v| v.as_bool()), Some(true));
        let items = list.get("items").and_then(|v| v.as_arr()).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(text_of_block(&items[0]), "Boil");
        assert_eq!(text_of_block(&items[1]), "Steep");

        // The table keeps its rows, cells and header flag.
        let table = &blocks_of(&doc)[4];
        assert_eq!(table.get("rowCount").and_then(|v| v.as_f64()), Some(2.0));
        let rows = table.get("rows").and_then(|v| v.as_arr()).unwrap();
        let first_row = rows[0].as_arr().unwrap();
        assert_eq!(text_of_block(&first_row[0]), "Leaf");
        assert_eq!(
            first_row[0].get("header").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(text_of_block(&rows[1].as_arr().unwrap()[1]), "2m");

        // The image carries its resolved src and alt.
        let image = &blocks_of(&doc)[5];
        assert_eq!(
            image.get("src").and_then(|v| v.as_str()),
            Some("https://tea.test/pot.png")
        );
        assert_eq!(image.get("alt").and_then(|v| v.as_str()), Some("A teapot"));

        // The form is fillable data: action, method, and one labelled field.
        let form = &blocks_of(&doc)[6];
        assert_eq!(
            form.get("action").and_then(|v| v.as_str()),
            Some("https://tea.test/brew")
        );
        assert_eq!(form.get("method").and_then(|v| v.as_str()), Some("post"));
        let fields = form.get("fields").and_then(|v| v.as_arr()).unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(
            fields[0].get("name").and_then(|v| v.as_str()),
            Some("amount")
        );
        assert_eq!(
            fields[0].get("type").and_then(|v| v.as_str()),
            Some("number")
        );
        assert_eq!(
            fields[0].get("required").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            fields[0].get("placeholder").and_then(|v| v.as_str()),
            Some("grams")
        );
        assert_eq!(
            fields[0].get("label").and_then(|v| v.as_str()),
            Some("Amount")
        );

        // The outline: two headings, indices matching their blocks, and
        // sections that partition the block list without gaps or overlap.
        let headings = doc.get("headings").and_then(|v| v.as_arr()).unwrap();
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].get("block").and_then(|v| v.as_f64()), Some(0.0));
        assert_eq!(headings[0].get("level").and_then(|v| v.as_f64()), Some(1.0));
        assert_eq!(headings[1].get("block").and_then(|v| v.as_f64()), Some(2.0));
        assert_eq!(headings[1].get("level").and_then(|v| v.as_f64()), Some(2.0));
        assert_eq!(
            headings[1].get("text").and_then(|v| v.as_str()),
            Some("Steps")
        );
        let sections = doc.get("sections").and_then(|v| v.as_arr()).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(
            (
                sections[0].get("from").and_then(|v| v.as_f64()),
                sections[0].get("to").and_then(|v| v.as_f64()),
            ),
            (Some(1.0), Some(2.0))
        );
        assert_eq!(
            (
                sections[1].get("from").and_then(|v| v.as_f64()),
                sections[1].get("to").and_then(|v| v.as_f64()),
            ),
            (Some(3.0), Some(7.0))
        );
        // Section weight counts nested text (list items, table cells) too:
        // section one holds the five-word paragraph, section two the two
        // list items plus the four table cells.
        assert_eq!(
            sections[0].get("words").and_then(|v| v.as_f64()),
            Some(5.0),
            "first section counts its paragraph: {:?}",
            sections[0]
        );
        assert_eq!(
            sections[1].get("words").and_then(|v| v.as_f64()),
            Some(6.0),
            "second section counts list + table text: {:?}",
            sections[1]
        );

        // Counts the agent can budget against.
        let telemetry = doc.get("telemetry").unwrap();
        assert_eq!(telemetry.get("images").and_then(|v| v.as_f64()), Some(1.0));
        assert_eq!(telemetry.get("forms").and_then(|v| v.as_f64()), Some(1.0));
        assert!(telemetry.get("words").and_then(|v| v.as_f64()).unwrap() >= 10.0);
        assert_eq!(telemetry.get("truncated"), None);
    }

    #[test]
    fn b5_extraction_reports_third_party_hosts_and_mixed_content_refusals() {
        // No tracker denylist exists in the engine (ROADMAP's not-do
        // discipline), so the telemetry reports the refusals the engine
        // itself makes — mixed content — plus the third-party hosts the
        // page embeds. A denylist is policy, and policy lives in the
        // agent's permission hook, not in a fact report.
        let b = test_browser_at(
            "<html><body><p>Words.</p>\
             <img src=\"/local.png\" alt=\"first party\">\
             <img src=\"https://cdn.other.test/a.png\" alt=\"third party\">\
             <img src=\"http://plain.test/b.png\" alt=\"mixed content\">\
             <iframe src=\"https://embed.other.test/frame\"></iframe>\
             </body></html>",
            "https://tea.test/guide",
        );
        let doc = extract::document_json(&b);
        let telemetry = doc.get("telemetry").unwrap();
        assert_eq!(telemetry.get("images").and_then(|v| v.as_f64()), Some(3.0));
        assert_eq!(telemetry.get("iframes").and_then(|v| v.as_f64()), Some(1.0));

        let third_party = telemetry.get("thirdParty").unwrap();
        let hosts: Vec<&str> = third_party
            .get("hosts")
            .and_then(|v| v.as_arr())
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(
            hosts,
            vec!["cdn.other.test", "plain.test", "embed.other.test"],
            "document-order hosts, first-party excluded"
        );
        assert_eq!(third_party.get("count").and_then(|v| v.as_f64()), Some(3.0));

        let refused = telemetry.get("refused").unwrap();
        assert_eq!(refused.get("count").and_then(|v| v.as_f64()), Some(1.0));
        let urls: Vec<&str> = refused
            .get("urls")
            .and_then(|v| v.as_arr())
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(urls, vec!["http://plain.test/b.png"]);
    }

    #[test]
    fn b5_extraction_skips_unrendered_subtrees() {
        let b = test_browser_at(
            "<html><head><title>t</title><style>.x{color:#ff0000}</style>\
             <script>var secret = 1;</script></head><body>\
             <p>real words</p>\
             <div style=\"display: none\"><p>ghost words</p></div>\
             <span style=\"visibility: hidden\">invisible words</span>\
             <template><p>template words</p></template>\
             </body></html>",
            "https://tea.test/",
        );
        let doc = extract::document_json(&b).to_json();
        assert!(doc.contains("real words"), "visible text extracted: {doc}");
        for absent in [
            "ghost words",
            "invisible words",
            "template words",
            "var secret",
            "ff0000",
        ] {
            assert!(!doc.contains(absent), "`{absent}` stayed out of {doc}");
        }
    }

    #[test]
    fn b5_extraction_bounds_a_hostile_document() {
        let long = "a".repeat(extract::BLOCK_TEXT_MAX * 2);
        let html = format!("<html><body><p>{long}</p></body></html>");
        let b = test_browser_at(&html, "https://tea.test/");
        let doc = extract::document_json(&b);
        assert_eq!(kinds_of(&doc), vec!["paragraph"]);
        let text = text_of_block(&blocks_of(&doc)[0]);
        assert_eq!(text.chars().count(), extract::BLOCK_TEXT_MAX);
        assert_eq!(
            doc.get("telemetry")
                .unwrap()
                .get("truncated")
                .and_then(|v| v.as_bool()),
            Some(true),
            "a bounded report says so"
        );
    }

    #[test]
    fn b5_extraction_without_a_page_reports_an_error() {
        let b = Browser::new(
            Box::new(NullHost { frames: 0 }),
            Box::new(ApproxMeasurer),
            (400.0, 600.0),
        );
        let doc = extract::document_json(&b);
        assert_eq!(
            doc.get("error").and_then(|v| v.as_str()),
            Some("no page is open")
        );
        assert_eq!(kinds_of(&doc).len(), 0);
    }

    /// Extraction reads the *live* tree, not the fetched bytes: the same
    /// `Page` the script session mutated is the one that gets reported.
    #[cfg(feature = "page-js")]
    #[test]
    fn b5_extraction_sees_the_post_script_document() {
        let mut b = js_browser();
        b.load_html(
            "<html><body><p id=\"x\">before</p><script>\
             var h = document.createElement('h1');\
             h.textContent = 'Late Heading';\
             document.body.insertBefore(h, document.getElementById('x'));\
             </script></body></html>",
            "https://example.test/js",
        );
        let doc = extract::document_json(&b);
        assert_eq!(kinds_of(&doc), vec!["heading", "paragraph"]);
        assert_eq!(text_of_block(&blocks_of(&doc)[0]), "Late Heading");
        assert_eq!(text_of_block(&blocks_of(&doc)[1]), "before");
    }

    // ── Track B / B2-B4: the agent host (AgentSession) ───────────────────

    /// A headless session with everything allowed — the posture a container
    /// or a CLI with `--allow "*"` runs in.
    fn agent_all() -> agent::AgentSession {
        agent::AgentSession::headless((400.0, 600.0)).with_policy(Box::new(agent::AllowList::all()))
    }

    const AGENT_PAGE: &str = "<html><head><title>Agent Fixture</title></head><body>\
<h1>Sign in</h1>\
<p>The <a href=\"/help\">help page</a> explains the form.</p>\
<form action=\"/session\" method=\"post\">\
<label for=\"user\">User</label><input id=\"user\" name=\"user\" type=\"text\" required>\
<input name=\"token\" type=\"text\" placeholder=\"token\">\
<button type=\"submit\">Go</button>\
</form>\
<p id=\"status\">Idle</p>\
</body></html>";

    #[test]
    fn agent_default_policy_is_read_only() {
        // A session nobody configured cannot mutate anything, and the
        // refusal names the flag that would allow it.
        let mut session = agent::AgentSession::headless((400.0, 600.0));
        let denied = session
            .permit_and_run(&kitae_agent::Verb::Navigate {
                url: "https://example.test/".into(),
            })
            .unwrap_err();
        assert!(
            denied.contains("permission denied") && denied.contains("--allow navigate"),
            "refusal teaches the fix: {denied}"
        );
        // Read-only verbs go through (they cannot change the page).
        assert!(session.permit_and_run(&kitae_agent::Verb::Read).is_ok());
        assert!(session.permit_and_run(&kitae_agent::Verb::Extract).is_ok());
        assert_eq!(session.journal().len(), 3, "refusals are journaled too");
        assert!(session.journal()[0].denied.is_some());
        assert!(session.journal()[1].denied.is_none());
    }

    #[test]
    fn agent_fill_read_links_find_and_extract_work_on_a_page() {
        let mut session = agent_all();
        session
            .browser_mut()
            .load_html(AGENT_PAGE, "https://example.test/form");

        // fill: by id, and by a selector that matches an element with no id
        // (the engine's node-keyed write path is what makes that work).
        let filled = session
            .permit_and_run(&kitae_agent::Verb::Fill {
                target: kitae_agent::Target::Selector("#user".into()),
                text: "ada".into(),
            })
            .expect("fill by id");
        assert!(filled.contains("ada"), "{filled}");
        assert_eq!(
            session.browser().input_value("user").as_deref(),
            Some("ada")
        );
        session
            .permit_and_run(&kitae_agent::Verb::Fill {
                target: kitae_agent::Target::Selector("input[name=token]".into()),
                text: "t0k".into(),
            })
            .expect("fill by attribute selector");
        let doc = extract::document_json(session.browser());
        let fields = blocks_of(&doc)
            .iter()
            .find(|b| b.get("kind").and_then(|v| v.as_str()) == Some("form"))
            .and_then(|f| f.get("fields"))
            .and_then(|v| v.as_arr())
            .expect("form fields");
        let token_value = fields
            .iter()
            .find(|f| f.get("name").and_then(|v| v.as_str()) == Some("token"))
            .and_then(|f| f.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(token_value, "t0k", "value landed on the id-less control");

        // read: the page as text.
        let read = session.permit_and_run(&kitae_agent::Verb::Read).unwrap();
        assert!(
            read.contains("Sign in") && read.contains("help page"),
            "{read}"
        );

        // links: resolved hrefs with the text that was painted.
        let links = session.permit_and_run(&kitae_agent::Verb::Links).unwrap();
        let links: kitae_json::Value = kitae_json::parse(&links).unwrap();
        // One anchor, one entry — its two word runs merge into the box an
        // agent would click.
        assert_eq!(links.get("count").and_then(|v| v.as_f64()), Some(1.0));
        let first = &links.get("links").unwrap().as_arr().unwrap()[0];
        assert_eq!(
            first.get("href").and_then(|v| v.as_str()),
            Some("https://example.test/help")
        );
        assert_eq!(
            first.get("text").and_then(|v| v.as_str()),
            Some("help page")
        );
        let rect = first.get("rect").unwrap().as_arr().unwrap();
        assert!(
            rect[2].as_f64().unwrap() > 60.0,
            "the merged box spans both words: {rect:?}"
        );

        // find: matches in page order.
        let found = session
            .permit_and_run(&kitae_agent::Verb::Find {
                query: "help".into(),
            })
            .unwrap();
        let found: kitae_json::Value = kitae_json::parse(&found).unwrap();
        // One match: find-in-page reports one per painted text run, and
        // only the "help" run contains the query ("page" is its own run).
        assert_eq!(found.get("count").and_then(|v| v.as_f64()), Some(1.0));
        let rects = found.get("matches").unwrap().as_arr().unwrap()[0]
            .get("rects")
            .unwrap()
            .as_arr()
            .unwrap();
        assert_eq!(rects.len(), 1, "each match cites the run it paints");
    }

    #[test]
    fn agent_click_reports_the_node_it_hit_and_watch_resolves_selectors() {
        let mut session = agent_all();
        session
            .browser_mut()
            .load_html(AGENT_PAGE, "https://example.test/form");

        let clicked = session
            .permit_and_run(&kitae_agent::Verb::Click {
                target: kitae_agent::Target::Selector("h1".into()),
            })
            .expect("click a heading is harmless");
        let clicked: kitae_json::Value = kitae_json::parse(&clicked).unwrap();
        assert!(clicked.get("node").and_then(|v| v.as_f64()).unwrap() > 0.0);
        assert_eq!(
            clicked.get("navigated").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(
            clicked.get("x").and_then(|v| v.as_f64()).is_some(),
            "the click point is reported: {clicked:?}"
        );

        // A point target hit-tests through the display list.
        let by_point = session
            .permit_and_run(&kitae_agent::Verb::Click {
                target: kitae_agent::Target::Point { x: 20.0, y: 20.0 },
            })
            .unwrap();
        let by_point: kitae_json::Value = kitae_json::parse(&by_point).unwrap();
        assert!(by_point.get("node").and_then(|v| v.as_f64()).unwrap() > 0.0);

        // watch: an already-present selector matches immediately, with the
        // box an agent would click next.
        let watched = session
            .permit_and_run(&kitae_agent::Verb::Watch {
                selector: "#status".into(),
                timeout_ms: 50,
            })
            .unwrap();
        let watched: kitae_json::Value = kitae_json::parse(&watched).unwrap();
        assert_eq!(watched.get("matched").and_then(|v| v.as_bool()), Some(true));
        assert!(watched.get("rect").and_then(|v| v.as_arr()).is_some());

        // …and a selector that never appears times out *honestly* rather
        // than hanging or claiming success.
        let missed = session
            .permit_and_run(&kitae_agent::Verb::Watch {
                selector: ".never-there".into(),
                timeout_ms: 30,
            })
            .unwrap();
        let missed: kitae_json::Value = kitae_json::parse(&missed).unwrap();
        assert_eq!(missed.get("matched").and_then(|v| v.as_bool()), Some(false));
    }

    /// The engine's link hit test must use the *click point*: a link is
    /// only activated by a click inside its box. (Regression: the sticky
    /// shift was applied by passing the link's own y as the point's y,
    /// which made the vertical test vacuous — every click in the link's
    /// x-span activated it, however far away it landed.)
    #[test]
    fn click_outside_a_link_box_does_not_activate_it() {
        let html = "<html><body><h1>Sign in</h1>\
<p>The <a href=\"/help\">help page</a> explains.</p>\
<p>Filler far below the link, at a completely different height.</p></body></html>";
        let link = test_browser_at(html, "https://example.test/form")
            .links()
            .first()
            .cloned()
            .expect("the fixture has a link");
        // 300px below the link, inside its x-span: must not activate.
        let mut below = test_browser_at(html, "https://example.test/form");
        below.click(link.x + 4.0, link.y + 300.0);
        assert_eq!(
            below.page_url, "https://example.test/form",
            "a click far below a link must not follow it"
        );
        // Inside the box: activates.
        let mut inside = test_browser_at(html, "https://example.test/form");
        inside.click(link.x + 4.0, link.y + link.h / 2.0);
        assert_eq!(inside.page_url, "https://example.test/help");
    }

    #[test]
    fn agent_submit_blocks_on_validation_before_navigating() {
        // A required field left empty must not reach the network: the
        // engine's own validation stops it, and the verb reports which
        // controls failed.
        let mut session = agent_all();
        session
            .browser_mut()
            .load_html(AGENT_PAGE, "https://example.test/form");
        session
            .permit_and_run(&kitae_agent::Verb::Fill {
                target: kitae_agent::Target::Selector("input[name=token]".into()),
                text: "only-token".into(),
            })
            .unwrap();
        let submitted = session
            .permit_and_run(&kitae_agent::Verb::Submit {
                form: Some("form".into()),
            })
            .unwrap();
        let submitted: kitae_json::Value = kitae_json::parse(&submitted).unwrap();
        assert_eq!(
            submitted.get("validationFailed").and_then(|v| v.as_bool()),
            Some(true),
            "{submitted:?}"
        );
        assert_eq!(
            submitted.get("navigated").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(submitted
            .get("invalidControls")
            .and_then(|v| v.as_arr())
            .map(|a| !a.is_empty())
            .unwrap_or(false));
    }

    #[test]
    fn agent_screenshot_and_pdf_say_what_is_missing() {
        // No rasterizer installed: the verbs refuse *by name* instead of
        // returning an empty image an agent would believe.
        let mut session = agent_all();
        session
            .browser_mut()
            .load_html(AGENT_PAGE, "https://example.test/form");
        let err = session
            .permit_and_run(&kitae_agent::Verb::Screenshot { path: None })
            .unwrap_err();
        assert!(err.contains("no page rasterizer is installed"), "{err}");
        assert!(
            err.contains("kitae_headless"),
            "the error names the fix: {err}"
        );
        let err = session
            .permit_and_run(&kitae_agent::Verb::Pdf { path: None })
            .unwrap_err();
        assert!(err.contains("no page rasterizer is installed"), "{err}");
    }

    /// A rasterizer that records what it was asked for: proves the verb
    /// plumbing (path vs inline) without needing a font stack.
    struct RecordingRaster;

    impl agent::PageRaster for RecordingRaster {
        fn png(&self, browser: &Browser) -> Result<Vec<u8>, String> {
            Ok(format!("PNG:{}:{}", browser.page_url, browser.page_title).into_bytes())
        }
        fn pdf(&self, browser: &Browser) -> Result<Vec<u8>, String> {
            Ok(format!("PDF:{}", browser.page_url).into_bytes())
        }
    }

    #[test]
    fn agent_screenshot_writes_a_path_or_returns_inline_base64() {
        let mut session = agent_all().with_raster(std::sync::Arc::new(RecordingRaster));
        session
            .browser_mut()
            .load_html(AGENT_PAGE, "https://example.test/form");

        let dir = std::env::temp_dir().join(format!("kitae_shot_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("page.png");
        let written = session
            .permit_and_run(&kitae_agent::Verb::Screenshot {
                path: Some(path.to_string_lossy().to_string()),
            })
            .unwrap();
        let written: kitae_json::Value = kitae_json::parse(&written).unwrap();
        assert_eq!(
            written.get("mime").and_then(|v| v.as_str()),
            Some("image/png")
        );
        let bytes = std::fs::read(&path).expect("file written");
        assert!(String::from_utf8_lossy(&bytes).starts_with("PNG:https://example.test/form"));

        // No path: the bytes ride in the result, base64.
        let inline = session
            .permit_and_run(&kitae_agent::Verb::Screenshot { path: None })
            .unwrap();
        let inline: kitae_json::Value = kitae_json::parse(&inline).unwrap();
        let encoded = inline.get("png").and_then(|v| v.as_str()).unwrap();
        assert!(!encoded.is_empty() && inline.get("path").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_prompt_policy_denies_when_nobody_answers() {
        // The app's policy shape: an unanswered prompt must become a refusal
        // (never an allow), and it must not block forever.
        let policy = agent::PromptPolicy::new(Box::new(|_verb| None))
            .with_timeout(std::time::Duration::from_millis(60));
        let answer = agent::PermissionPolicy::check(&policy, &kitae_agent::Verb::Read);
        assert!(matches!(answer, kitae_agent::Decision::Deny(_)));

        let policy = agent::PromptPolicy::new(Box::new(|verb| Some(verb.name() == "navigate")));
        assert!(matches!(
            agent::PermissionPolicy::check(
                &policy,
                &kitae_agent::Verb::Navigate {
                    url: "https://a.test/".into()
                }
            ),
            kitae_agent::Decision::Allow
        ));
        assert!(matches!(
            agent::PermissionPolicy::check(&policy, &kitae_agent::Verb::Read),
            kitae_agent::Decision::Deny(_)
        ));
    }

    #[test]
    fn agent_allow_list_grammar_is_verb_names_and_a_star() {
        use kitae_agent::Verb;
        let list = agent::AllowList::new(&["navigate", "click"]);
        assert!(list.allows(&Verb::Navigate {
            url: "https://a.test/".into()
        }));
        assert!(list.allows(&Verb::Click {
            target: kitae_agent::Target::Selector("#a".into())
        }));
        // Read-only verbs ride along by default.
        assert!(list.allows(&Verb::Extract));
        assert!(!list.allows(&Verb::Fill {
            target: kitae_agent::Target::Selector("#a".into()),
            text: "x".into()
        }));

        let strict = agent::AllowList::none().with_read_only(false);
        assert!(!strict.allows(&Verb::Extract));
        assert!(agent::AllowList::all().allows(&Verb::Archive { dir: None }));
    }

    // ── Track B / B6: archive-and-verify ─────────────────────────────────

    fn evidence_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kitae_evidence_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn evidence_round_trips_archives_verifies_and_replays() {
        let dir = evidence_dir("round");
        let mut session = agent_all();
        session
            .browser_mut()
            .load_html(AGENT_PAGE, "https://example.test/form");
        let archived = session
            .permit_and_run(&kitae_agent::Verb::Archive {
                dir: Some(dir.to_string_lossy().to_string()),
            })
            .expect("archive");
        let archived: kitae_json::Value = kitae_json::parse(&archived).unwrap();
        assert_eq!(
            archived.get("signature").and_then(|v| v.as_str()),
            Some("valid"),
            "the archive is signed by this device: {archived:?}"
        );
        assert!(archived.get("deviceSignPk").is_some());
        assert!(archived
            .get("contentSha256")
            .and_then(|v| v.as_str())
            .map(|h| h.len() == 64)
            .unwrap_or(false));

        // Verify through the manifest path.
        let manifest_path = archived
            .get("manifestPath")
            .and_then(|v| v.as_str())
            .expect("manifest path");
        let verified = evidence::verify(manifest_path).expect("verify");
        assert_eq!(
            verified.get("ok").and_then(|v| v.as_bool()),
            Some(true),
            "{verified:?}"
        );
        assert_eq!(
            verified.get("replay").and_then(|v| v.as_str()),
            Some("match")
        );
        let checks = verified.get("checks").and_then(|v| v.as_arr()).unwrap();
        let names: Vec<&str> = checks
            .iter()
            .filter_map(|c| c.get("check").and_then(|v| v.as_str()))
            .collect();
        for expected in ["qtsq", "signature", "qtsqSha256", "contentSha256", "replay"] {
            assert!(
                names.contains(&expected),
                "missing check `{expected}`: {names:?}"
            );
        }

        // …and the archive path alone still yields the signature verdict.
        let qtsq_path = archived.get("path").and_then(|v| v.as_str()).unwrap();
        let verified = evidence::verify(qtsq_path).expect("verify by archive");
        assert_eq!(
            verified.get("signature").and_then(|v| v.as_str()),
            Some("valid")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evidence_verify_names_the_check_that_fails() {
        let dir = evidence_dir("tamper");
        let mut session = agent_all();
        session
            .browser_mut()
            .load_html(AGENT_PAGE, "https://example.test/form");
        let archived = session
            .permit_and_run(&kitae_agent::Verb::Archive {
                dir: Some(dir.to_string_lossy().to_string()),
            })
            .unwrap();
        let archived: kitae_json::Value = kitae_json::parse(&archived).unwrap();
        let manifest_path = archived
            .get("manifestPath")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        // A manifest whose content digest was edited must fail *that* check,
        // and only that one.
        let text = std::fs::read_to_string(&manifest_path).unwrap();
        let tampered = text.replace(
            archived
                .get("contentSha256")
                .and_then(|v| v.as_str())
                .unwrap(),
            &"0".repeat(64),
        );
        assert_ne!(text, tampered, "the digest was there to edit");
        std::fs::write(&manifest_path, &tampered).unwrap();

        let verified = evidence::verify(&manifest_path).expect("verify");
        assert_eq!(verified.get("ok").and_then(|v| v.as_bool()), Some(false));
        let checks = verified.get("checks").and_then(|v| v.as_arr()).unwrap();
        let failed: Vec<&str> = checks
            .iter()
            .filter(|c| c.get("ok").and_then(|v| v.as_bool()) == Some(false))
            .filter_map(|c| c.get("check").and_then(|v| v.as_str()))
            .collect();
        assert_eq!(failed, vec!["contentSha256"], "{verified:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evidence_refuses_to_archive_a_session_with_no_page() {
        let dir = evidence_dir("nopage");
        let mut session = agent_all();
        let err = session
            .permit_and_run(&kitae_agent::Verb::Archive {
                dir: Some(dir.to_string_lossy().to_string()),
            })
            .unwrap_err();
        assert!(err.contains("nothing to archive"), "{err}");
        assert!(evidence::verify("/nonexistent/evidence.qtsq").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn node_at_point_in(dl: &DisplayList, x: f32, y: f32) -> Option<NodeId> {
    let mut text_hit: Option<(f32, u32)> = None;
    for t in &dl.texts {
        if t.size <= 0.0 {
            continue;
        }
        let top = t.y - t.size * 0.9;
        let bottom = t.y + t.size * 0.35;
        if y >= top && y <= bottom && x >= t.x - 2.0 {
            if text_hit.map_or(true, |(bx, _)| t.x > bx) {
                text_hit = Some((t.x, t.node));
            }
        }
    }
    if let Some((_, node)) = text_hit {
        return Some(NodeId(node));
    }
    for img in dl.images.iter().rev() {
        if x >= img.x && x <= img.x + img.w && y >= img.y && y <= img.y + img.h {
            return Some(NodeId(img.node));
        }
    }
    // Painted boxes. Matrix-transformed rects store pre-transform geometry
    // (the dialog hit-test maps corners through the matrix); this query
    // tolerates the gap for those rare items.
    for r in dl.rects.iter().rev() {
        if r.transform.is_some() {
            continue;
        }
        if x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h {
            return Some(NodeId(r.node));
        }
    }
    None
}
