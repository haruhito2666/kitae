//! Page-as-data extraction — Track B's tier B5 (`docs/ROADMAP.md`).
//!
//! The agent's input layer: one call turns the *live* document
//! (post-script, post-layout) into structured JSON a model can act on,
//! with provenance on every block — the engine's own node id and the
//! layout box that node occupies — and honest telemetry. It is
//! deliberately not a readability heuristic: it reports what the engine
//! laid out, in document order, and leaves the "what matters" judgement to
//! the consumer, which is the party holding the task.
//!
//! Geometry comes from the accessibility tree's bounds when the element
//! has them (one box per element — the click target an agent wants) and
//! falls back to the union of display-list boxes the node painted. Both
//! are page-space CSS pixels of the last published frame.
//!
//! What "refused" means here: the engine ships **no tracker denylist**
//! (ROADMAP's not-do discipline), so the telemetry reports the refusals
//! the engine actually makes — mixed-content subresources on an `https`
//! page — alongside the third-party hosts the document embeds. A denylist
//! would be a policy layer, and policy belongs in the agent's permission
//! hook (`kitae_script::agent::AgentApi`), not in a fact report.
//!
//! Bounds, so a hostile page cannot balloon an agent's context:
//! [`TEXT_BUDGET`] characters of block text, [`BLOCK_BUDGET`] blocks,
//! [`BLOCK_TEXT_MAX`] per block, [`LIST_BUDGET`] third-party/refused
//! entries. When a bound bites, `telemetry.truncated` says so.

use std::collections::{HashMap, HashSet};

use kitae_dom::{Dom, NodeId, NodeKind};
use kitae_json::Value;
use kitae_style::{ComputedStyle, Display, Visibility};

use crate::{AxNode, Browser};

/// Total characters of block text across one document.
pub const TEXT_BUDGET: usize = 256 * 1024;
/// Maximum blocks reported.
pub const BLOCK_BUDGET: usize = 4_000;
/// Per-block text cap.
pub const BLOCK_TEXT_MAX: usize = 8 * 1024;
/// Maximum third-party hosts / refused URLs reported.
pub const LIST_BUDGET: usize = 64;

/// One element's page-space box: `(x, y, w, h)` in CSS pixels.
pub type Box2 = (f32, f32, f32, f32);

/// The structured document for the browser's current page.
pub fn document_json(browser: &Browser) -> Value {
    let Some(page) = browser.page.as_ref() else {
        return Value::obj(vec![
            ("url", Value::s(browser.page_url.clone())),
            ("title", Value::s(browser.page_title.clone())),
            ("blocks", Value::arr(Vec::new())),
            ("error", Value::s("no page is open")),
        ]);
    };
    let Some(root) = page.dom.root() else {
        return Value::obj(vec![
            ("url", Value::s(browser.page_url.clone())),
            ("title", Value::s(browser.page_title.clone())),
            ("blocks", Value::arr(Vec::new())),
            ("error", Value::s("the document has no element tree")),
        ]);
    };
    let body = find_tag(&page.dom, root, "body").unwrap_or(root);
    let geometry = geometry(browser);

    let mut walker = Walker {
        dom: &page.dom,
        styles: &page.styles,
        geometry: &geometry,
        base: if page.base_url.is_empty() {
            browser.page_url.clone()
        } else {
            page.base_url.clone()
        },
        doc_url: browser.page_url.clone(),
        blocks: Vec::new(),
        headings: Vec::new(),
        telemetry: Telemetry::default(),
        text_left: TEXT_BUDGET,
        third_party_seen: HashSet::new(),
    };
    walker.walk(body, 0);

    let sections = sections_of(&walker.blocks);
    let telemetry = walker.telemetry.into_json();

    let mut pairs: Vec<(String, Value)> = vec![
        ("url".into(), Value::s(browser.page_url.clone())),
        (
            "title".into(),
            Value::s(
                page_title_of(&page.dom, root)
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| browser.page_title.clone()),
            ),
        ),
    ];
    if let Some(description) = meta_description(&page.dom, root) {
        pairs.push(("description".into(), Value::s(description)));
    }
    pairs.push(("lang".into(), Value::s(lang_of(&page.dom, root))));
    pairs.push(("blocks".into(), Value::arr(walker.blocks)));
    pairs.push(("headings".into(), Value::arr(walker.headings)));
    pairs.push(("sections".into(), sections));
    pairs.push(("telemetry".into(), telemetry));
    Value::Obj(pairs)
}

/// Node → page-space box, in two passes: the accessibility tree's bounds
/// (one box per built element — the click target) first, then the union of
/// display-list boxes the node painted, for nodes the AX tree skipped.
///
/// Public because the agent plane's selector targets need exactly this
/// answer: the box an agent should click is the box extraction reports.
pub fn geometry(browser: &Browser) -> HashMap<u32, Box2> {
    fn collect_ax(node: &AxNode, out: &mut HashMap<String, Box2>) {
        if node.w > 0.0 && node.h > 0.0 {
            out.insert(node.id.clone(), (node.x, node.y, node.w, node.h));
        }
        for child in &node.children {
            collect_ax(child, out);
        }
    }
    let mut out: HashMap<u32, Box2> = HashMap::new();
    // Pass 1: AX bounds, keyed back to DOM nodes through `ax_ids`.
    let mut ax_bounds: HashMap<String, Box2> = HashMap::new();
    for node in &browser.ax_tree {
        collect_ax(node, &mut ax_bounds);
    }
    for (id, ax_id) in &browser.ax_ids {
        if let Some(box2) = ax_bounds.get(ax_id) {
            out.insert(id.0, *box2);
        }
    }
    // Pass 2: painted geometry, unioned per node, only where pass 1 had
    // nothing. The AX tree covers every non-pruned element (it claims text
    // runs for the ones with no identity geometry), so this pass is what
    // reaches elements the AX tree skips: hidden-from-AX chrome that still
    // paints, and empty boxes carrying a background.
    if let Some(dl) = browser.last_dl.as_ref() {
        for r in &dl.rects {
            let entry = out.entry(r.node).or_insert((r.x, r.y, r.w, r.h));
            let x0 = entry.0.min(r.x);
            let y0 = entry.1.min(r.y);
            let x1 = (entry.0 + entry.2).max(r.x + r.w);
            let y1 = (entry.1 + entry.3).max(r.y + r.h);
            *entry = (x0, y0, x1 - x0, y1 - y0);
        }
    }
    out
}

/// One node's page-space box, if the last frame gave it one.
pub fn element_box(browser: &Browser, node: NodeId) -> Option<Box2> {
    geometry(browser).get(&node.0).copied()
}

#[derive(Default)]
struct Telemetry {
    words: usize,
    images: usize,
    iframes: usize,
    forms: usize,
    links: usize,
    third_party: Vec<String>,
    refused: Vec<String>,
    truncated: bool,
}

impl Telemetry {
    fn into_json(self) -> Value {
        let mut pairs: Vec<(String, Value)> = vec![
            ("words".into(), Value::Num(self.words as f64)),
            ("images".into(), Value::Num(self.images as f64)),
            ("iframes".into(), Value::Num(self.iframes as f64)),
            ("forms".into(), Value::Num(self.forms as f64)),
            ("links".into(), Value::Num(self.links as f64)),
        ];
        let hosts: Vec<Value> = self.third_party.into_iter().map(Value::s).collect();
        pairs.push((
            "thirdParty".into(),
            Value::obj(vec![
                ("count", Value::Num(hosts.len() as f64)),
                ("hosts", Value::arr(hosts)),
            ]),
        ));
        let refused: Vec<Value> = self.refused.into_iter().map(Value::s).collect();
        pairs.push((
            "refused".into(),
            Value::obj(vec![
                ("count", Value::Num(refused.len() as f64)),
                ("urls", Value::arr(refused)),
            ]),
        ));
        if self.truncated {
            pairs.push(("truncated".into(), Value::Bool(true)));
        }
        Value::Obj(pairs)
    }
}

struct Walker<'a> {
    dom: &'a Dom,
    styles: &'a [(NodeId, ComputedStyle)],
    geometry: &'a HashMap<u32, Box2>,
    base: String,
    doc_url: String,
    blocks: Vec<Value>,
    headings: Vec<Value>,
    telemetry: Telemetry,
    text_left: usize,
    third_party_seen: HashSet<String>,
}

impl<'a> Walker<'a> {
    fn style(&self, node: NodeId) -> Option<&ComputedStyle> {
        self.styles.iter().find(|(n, _)| *n == node).map(|(_, s)| s)
    }

    /// Whether this node renders at all. Unstyled nodes (text inside a
    /// styled block) inherit visibility from the block that holds them.
    fn visible(&self, node: NodeId) -> bool {
        match self.style(node) {
            Some(style) => style.display != Display::None && style.visibility != Visibility::Hidden,
            None => true,
        }
    }

    fn rect_json(&self, node: NodeId) -> Option<Value> {
        self.geometry.get(&node.0).map(|(x, y, w, h)| {
            Value::arr(vec![
                Value::Num(round2(*x)),
                Value::Num(round2(*y)),
                Value::Num(round2(*w)),
                Value::Num(round2(*h)),
            ])
        })
    }

    /// Whitespace-collapsed text of a subtree, charged against the
    /// document's text budget.
    fn text_of(&mut self, node: NodeId) -> String {
        let mut raw = String::new();
        collect_text(self.dom, node, &mut raw);
        let collapsed = collapse(&raw);
        let take = self.text_left.min(BLOCK_TEXT_MAX);
        let capped: String = collapsed.chars().take(take).collect();
        if capped.chars().count() < collapsed.chars().count() {
            self.telemetry.truncated = true;
        }
        self.text_left = self.text_left.saturating_sub(capped.chars().count());
        self.telemetry.words = self.telemetry.words.saturating_add(count_words(&capped));
        capped
    }

    /// Links inside a subtree, resolved and deduplicated by href.
    fn links_of(&self, node: NodeId) -> Value {
        let mut out: Vec<Value> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        self.dom.walk(node, &mut |n| {
            if !self.dom.tag(n).eq_ignore_ascii_case("a") || !self.visible(n) {
                return;
            }
            let Some(href) = self.dom.element(n).and_then(|e| e.attr("href")) else {
                return;
            };
            let resolved = kitae_net::resolve(&self.base, href).unwrap_or_else(|| href.to_string());
            if !seen.insert(resolved.clone()) {
                return;
            }
            let mut text = String::new();
            collect_text(self.dom, n, &mut text);
            out.push(Value::obj(vec![
                ("href", Value::s(resolved)),
                ("text", Value::s(collapse(&text))),
                ("node", Value::Num(n.0 as f64)),
            ]));
        });
        Value::arr(out)
    }

    /// Record a subresource URL: count its host as third-party when it is,
    /// and note when the engine's own policy refuses it.
    fn note_resource(&mut self, raw: &str) {
        let Some(resolved) = kitae_net::resolve(&self.base, raw) else {
            return;
        };
        if !kitae_net::mixed_content_allowed(&self.doc_url, &resolved)
            && self.telemetry.refused.len() < LIST_BUDGET
            && !self.telemetry.refused.contains(&resolved)
        {
            self.telemetry.refused.push(resolved.clone());
        }
        if kitae_net::is_third_party_request(&self.doc_url, &resolved) {
            if let Some(host) = host_of(&resolved) {
                if self.telemetry.third_party.len() < LIST_BUDGET
                    && self.third_party_seen.insert(host.clone())
                {
                    self.telemetry.third_party.push(host);
                }
            }
        }
    }

    fn push_block(&mut self, kind: &str, tag: &str, node: NodeId, extra: Vec<(&str, Value)>) {
        if self.blocks.len() >= BLOCK_BUDGET {
            self.telemetry.truncated = true;
            return;
        }
        let mut pairs: Vec<(String, Value)> = vec![
            ("kind".into(), Value::s(kind)),
            ("tag".into(), Value::s(tag)),
            ("node".into(), Value::Num(node.0 as f64)),
        ];
        if let Some(rect) = self.rect_json(node) {
            pairs.push(("rect".into(), rect));
        }
        pairs.extend(extra.into_iter().map(|(k, v)| (k.to_string(), v)));
        let index = self.blocks.len();
        if kind == "heading" {
            let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
            self.headings.push(Value::obj(vec![
                ("level", get("level").unwrap_or(Value::Num(0.0))),
                ("block", Value::Num(index as f64)),
                ("node", Value::Num(node.0 as f64)),
                ("text", get("text").unwrap_or(Value::Null)),
            ]));
        }
        self.blocks.push(Value::Obj(pairs));
    }

    fn walk(&mut self, node: NodeId, depth: usize) {
        if depth > 40 {
            self.telemetry.truncated = true;
            return;
        }
        for child in self.dom.children(node).to_vec() {
            let tag = self.dom.tag(child).to_ascii_lowercase();
            match tag.as_str() {
                // Controls and form machinery are reported by their
                // enclosing block (a list's items, a form's fields), so
                // they must not be walked as content.
                "option" | "optgroup" | "input" | "select" | "textarea" | "button" | "label"
                | "fieldset" | "legend" | "datalist" | "output" | "progress" | "meter" => {
                    continue;
                }
                _ => {}
            }
            if !self.visible(child) {
                continue;
            }
            match tag.as_str() {
                // Never content: script/style bodies, the head, templates.
                "script" | "style" | "head" | "title" | "meta" | "link" | "base" | "template"
                | "noscript" => {}
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let level = tag[1..].parse::<f64>().unwrap_or(0.0);
                    let text = self.text_of(child);
                    let links = self.links_of(child);
                    self.push_block(
                        "heading",
                        &tag,
                        child,
                        vec![
                            ("level", Value::Num(level)),
                            ("text", Value::s(text)),
                            ("links", links),
                        ],
                    );
                }
                "p" | "pre" | "blockquote" | "figcaption" | "dd" | "dt" | "li" | "summary" => {
                    let kind = match tag.as_str() {
                        "pre" => "code",
                        "blockquote" => "quote",
                        _ => "paragraph",
                    };
                    let text = self.text_of(child);
                    let links = self.links_of(child);
                    self.push_block(
                        kind,
                        &tag,
                        child,
                        vec![("text", Value::s(text)), ("links", links)],
                    );
                }
                "ul" | "ol" => {
                    let ordered = tag == "ol";
                    let mut items: Vec<Value> = Vec::new();
                    for item in self.dom.children(child).to_vec() {
                        if !self.dom.tag(item).eq_ignore_ascii_case("li") {
                            continue;
                        }
                        let text = self.text_of(item);
                        let mut pairs = vec![
                            ("text".to_string(), Value::s(text)),
                            ("node".to_string(), Value::Num(item.0 as f64)),
                        ];
                        if let Some(rect) = self.rect_json(item) {
                            pairs.push(("rect".to_string(), rect));
                        }
                        items.push(Value::Obj(pairs));
                    }
                    self.push_block(
                        "list",
                        &tag,
                        child,
                        vec![
                            ("ordered", Value::Bool(ordered)),
                            ("items", Value::arr(items)),
                        ],
                    );
                }
                "table" => {
                    // The tree carries the spec's implied `<tbody>` (and may
                    // carry `<thead>`/`<tfoot>`), so rows are found by
                    // descending through table-internal containers — never
                    // into a nested table, whose rows are its own block.
                    let mut row_nodes: Vec<NodeId> = Vec::new();
                    fn rows_under(dom: &Dom, node: NodeId, out: &mut Vec<NodeId>) {
                        for kid in dom.children(node) {
                            let tag = dom.tag(*kid).to_ascii_lowercase();
                            match tag.as_str() {
                                "tr" => out.push(*kid),
                                "thead" | "tbody" | "tfoot" => rows_under(dom, *kid, out),
                                _ => {}
                            }
                        }
                    }
                    rows_under(self.dom, child, &mut row_nodes);
                    let mut rows: Vec<Value> = Vec::new();
                    for row in row_nodes {
                        let mut cells: Vec<Value> = Vec::new();
                        for cell in self.dom.children(row).to_vec() {
                            let cell_tag = self.dom.tag(cell).to_ascii_lowercase();
                            if cell_tag != "td" && cell_tag != "th" {
                                continue;
                            }
                            let text = self.text_of(cell);
                            let mut pairs = vec![
                                ("text".to_string(), Value::s(text)),
                                ("header".to_string(), Value::Bool(cell_tag == "th")),
                            ];
                            if let Some(rect) = self.rect_json(cell) {
                                pairs.push(("rect".to_string(), rect));
                            }
                            cells.push(Value::Obj(pairs));
                        }
                        rows.push(Value::arr(cells));
                    }
                    let row_count = rows.len();
                    self.push_block(
                        "table",
                        &tag,
                        child,
                        vec![
                            ("rows", Value::arr(rows)),
                            ("rowCount", Value::Num(row_count as f64)),
                        ],
                    );
                }
                "img" => {
                    self.telemetry.images += 1;
                    let element = self.dom.element(child);
                    let src = element.and_then(|e| e.attr("src")).unwrap_or("");
                    if !src.is_empty() {
                        self.note_resource(src);
                    }
                    let resolved =
                        kitae_net::resolve(&self.base, src).unwrap_or_else(|| src.into());
                    let alt = element
                        .and_then(|e| e.attr("alt"))
                        .unwrap_or("")
                        .to_string();
                    self.push_block(
                        "image",
                        &tag,
                        child,
                        vec![("src", Value::s(resolved)), ("alt", Value::s(alt))],
                    );
                }
                "iframe" | "video" | "audio" | "canvas" | "object" | "embed" => {
                    if tag == "iframe" {
                        self.telemetry.iframes += 1;
                    }
                    let element = self.dom.element(child);
                    let src = element
                        .and_then(|e| e.attr("src"))
                        .unwrap_or("")
                        .to_string();
                    let title = element
                        .and_then(|e| e.attr("title"))
                        .unwrap_or("")
                        .to_string();
                    let mut extra: Vec<(&str, Value)> = vec![];
                    if !src.is_empty() {
                        self.note_resource(&src);
                        let resolved =
                            kitae_net::resolve(&self.base, &src).unwrap_or_else(|| src.clone());
                        extra.push(("src", Value::s(resolved)));
                    }
                    if !title.is_empty() {
                        extra.push(("title", Value::s(title)));
                    }
                    self.push_block("widget", &tag, child, extra);
                }
                "form" => {
                    self.telemetry.forms += 1;
                    let fields = form_fields(self.dom, child);
                    let action = self
                        .dom
                        .element(child)
                        .and_then(|e| e.attr("action"))
                        .unwrap_or("")
                        .to_string();
                    let resolved =
                        kitae_net::resolve(&self.base, &action).unwrap_or_else(|| action.clone());
                    let method = self
                        .dom
                        .element(child)
                        .and_then(|e| e.attr("method"))
                        .unwrap_or("get")
                        .to_ascii_lowercase();
                    self.push_block(
                        "form",
                        &tag,
                        child,
                        vec![
                            ("action", Value::s(resolved)),
                            ("method", Value::s(method)),
                            ("fields", Value::arr(fields)),
                        ],
                    );
                }
                "a" => {
                    self.telemetry.links += 1;
                    let text = self.text_of(child);
                    let links = self.links_of(child);
                    self.push_block(
                        "link",
                        &tag,
                        child,
                        vec![("text", Value::s(text)), ("links", links)],
                    );
                }
                "hr" => self.push_block("separator", &tag, child, vec![]),
                "br" => {}
                // Everything else — structural containers and inline
                // elements alike — descends: the blocks inside carry the
                // content, and `sections` groups them by heading.
                _ => self.walk(child, depth + 1),
            }
        }
    }
}

/// The flat outline of heading-bounded sections: each heading opens a
/// section that runs until the next heading, whatever its level. Levels
/// are on the heading entry itself, so a consumer can nest them without
/// the engine guessing at a tree the author did not write.
fn sections_of(blocks: &[Value]) -> Value {
    let mut out: Vec<Value> = Vec::new();
    // (heading block index, level, words so far)
    let mut current: Option<(usize, usize, usize)> = None;
    for (i, block) in blocks.iter().enumerate() {
        let kind = block.get("kind").and_then(Value::as_str).unwrap_or("");
        let words = block_words(block);
        if kind == "heading" {
            let level = block.get("level").and_then(Value::as_f64).unwrap_or(1.0) as usize;
            if let Some((head, head_level, w)) = current.take() {
                out.push(section_json(head, head_level, w, i));
            }
            current = Some((i, level, 0));
        } else if let Some((_, _, w)) = current.as_mut() {
            *w += words;
        }
    }
    if let Some((head, level, w)) = current.take() {
        out.push(section_json(head, level, w, blocks.len()));
    }
    Value::arr(out)
}

/// Every word a block carries, including the text of its nested items and
/// cells — a section's weight must count what a reader would read.
fn block_words(block: &Value) -> usize {
    fn sum(value: &Value, out: &mut usize) {
        match value {
            Value::Arr(items) => {
                for item in items {
                    sum(item, out);
                }
            }
            Value::Obj(pairs) => {
                for (key, value) in pairs {
                    // A block's own text already contains its links' words
                    // (they are rendered text), so counting the `links`
                    // array again would double every anchor.
                    if key == "links" {
                        continue;
                    }
                    if key == "text" {
                        if let Some(text) = value.as_str() {
                            *out += count_words(text);
                        }
                    }
                    sum(value, out);
                }
            }
            _ => {}
        }
    }
    let mut out = 0;
    sum(block, &mut out);
    out
}

fn section_json(heading: usize, level: usize, words: usize, end: usize) -> Value {
    Value::obj(vec![
        ("heading", Value::Num(heading as f64)),
        ("level", Value::Num(level as f64)),
        ("from", Value::Num((heading + 1) as f64)),
        ("to", Value::Num(end as f64)),
        ("words", Value::Num(words as f64)),
    ])
}

/// One form's fields, in document order: what an agent needs to fill it.
fn form_fields(dom: &Dom, form: NodeId) -> Vec<Value> {
    let mut fields = Vec::new();
    dom.walk(form, &mut |n| {
        let tag = dom.tag(n).to_ascii_lowercase();
        if !matches!(tag.as_str(), "input" | "select" | "textarea" | "button") {
            return;
        }
        let Some(el) = dom.element(n) else { return };
        let kind = if tag == "input" {
            el.attr("type").unwrap_or("text").to_ascii_lowercase()
        } else {
            tag.clone()
        };
        fields.push(Value::obj(vec![
            ("name", Value::s(el.attr("name").unwrap_or("").to_string())),
            ("type", Value::s(kind)),
            (
                "value",
                Value::s(el.attr("value").unwrap_or("").to_string()),
            ),
            ("required", Value::Bool(el.attr("required").is_some())),
            (
                "placeholder",
                Value::s(el.attr("placeholder").unwrap_or("").to_string()),
            ),
            ("label", Value::s(label_for(dom, n))),
            ("node", Value::Num(n.0 as f64)),
        ]));
    });
    fields
}

/// A control's label: a wrapping `<label>`, else a `<label for=…>` naming
/// it, else the control's own `aria-label`.
fn label_for(dom: &Dom, node: NodeId) -> String {
    let element = dom.element(node);
    let id = element.and_then(|e| e.attr("id")).unwrap_or("");
    let mut label = String::new();
    let mut parent = dom.node(node).parent;
    while let Some(p) = parent {
        if dom.tag(p).eq_ignore_ascii_case("label") {
            collect_text(dom, p, &mut label);
            break;
        }
        parent = dom.node(p).parent;
    }
    if label.trim().is_empty() && !id.is_empty() {
        if let Some(root) = dom.root() {
            dom.walk(root, &mut |n| {
                if !label.is_empty() || !dom.tag(n).eq_ignore_ascii_case("label") {
                    return;
                }
                if let Some(el) = dom.element(n) {
                    if el.attr("for") == Some(id) {
                        collect_text(dom, n, &mut label);
                    }
                }
            });
        }
    }
    if label.trim().is_empty() {
        if let Some(el) = element {
            label = el.attr("aria-label").unwrap_or("").to_string();
        }
    }
    collapse(&label)
}

fn collect_text(dom: &Dom, node: NodeId, out: &mut String) {
    dom.walk(node, &mut |n| {
        let node = dom.node(n);
        if matches!(node.kind, NodeKind::Text) {
            out.push_str(&node.text);
            out.push(' ');
        }
    });
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}

fn round2(v: f32) -> f64 {
    (v * 100.0).round() as f64 / 100.0
}

/// The host of an absolute URL, lowercased (`None` for relative input).
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = authority.split(':').next().unwrap_or(authority);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn find_tag(dom: &Dom, root: NodeId, tag: &str) -> Option<NodeId> {
    let mut found = None;
    dom.walk(root, &mut |n| {
        if found.is_none() && dom.tag(n).eq_ignore_ascii_case(tag) {
            found = Some(n);
        }
    });
    found
}

fn page_title_of(dom: &Dom, root: NodeId) -> Option<String> {
    let title = find_tag(dom, root, "title")?;
    let mut text = String::new();
    collect_text(dom, title, &mut text);
    Some(collapse(&text))
}

fn meta_description(dom: &Dom, root: NodeId) -> Option<String> {
    let mut found = None;
    dom.walk(root, &mut |n| {
        if found.is_some() || !dom.tag(n).eq_ignore_ascii_case("meta") {
            return;
        }
        if let Some(el) = dom.element(n) {
            if el
                .attr("name")
                .is_some_and(|v| v.eq_ignore_ascii_case("description"))
            {
                found = el.attr("content").map(str::to_string);
            }
        }
    });
    found.filter(|d| !d.trim().is_empty())
}

fn lang_of(dom: &Dom, root: NodeId) -> String {
    find_tag(dom, root, "html")
        .and_then(|n| dom.element(n))
        .and_then(|el| el.attr("lang"))
        .unwrap_or("")
        .to_string()
}
