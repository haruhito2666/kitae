//! HTML tree builder: tokens → DOM. Implements the WHATWG "in body" rules
//! with the full recovery machinery (port of html5lib 1.1's InBody phase):
//! the adoption agency algorithm for misnested formatting, formatting
//! reconstruction across structure, button-scope paragraph closing, scope
//! variants (default/button/list/table), implied-end-tag generation, foster
//! parenting with implied `<tbody>`/`<tr>` wrappers, implicit head/body
//! synthesis, whitespace-sensitive raw content, and a fragment entry point
//! ([`parse_fragment`]) for parsing into a context element.

use kitae_dom::{Dom, NodeId};

mod entities;
pub mod tokenizer;

use crate::tokenizer::{Token, Tokenizer};

/// Void elements never have children (HTML5 §4.8.1).
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Tags that imply the end of an open `<p>` when they start (html5lib
/// InBody: the startTagCloseP group plus pre/listing, form, li/dt/dd,
/// headings, hr, xmp, ruby parts (rt/rp/rb close p via implied end tags),
/// and table — all p-close in button scope).
const CLOSES_P: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "dd",
    "fieldset",
    "figure",
    "figcaption",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "li",
    "listing",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "rb",
    "rp",
    "rt",
    "section",
    "summary",
    "table",
    "ul",
    "xmp",
];

/// Consecutive headings imply each other's end (`<h1>a<h2>` closes the h1).
const HEADINGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];

/// Formatting elements (A1): text-level tags that reconstruct across
/// misnested structure — the adoption-agency family.
const FORMATTING: &[&str] = &[
    "a", "b", "big", "code", "em", "font", "i", "nobr", "s", "small", "strike", "strong", "tt", "u",
];

/// Structural elements (A1): the spec's specialElements (html5lib
/// constants.py, HTML namespace subset). They end formatting scope for the
/// furthest-block scan, gate stray end tags in `end_tag_other`, and stop
/// the list-item sibling scan.
const SPECIAL: &[&str] = &[
    "address",
    "applet",
    "area",
    "article",
    "aside",
    "base",
    "basefont",
    "bgsound",
    "blockquote",
    "body",
    "br",
    "button",
    "caption",
    "center",
    "col",
    "colgroup",
    "command",
    "dd",
    "details",
    "dir",
    "div",
    "dl",
    "dt",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hgroup",
    "hr",
    "html",
    "iframe",
    "img",
    "input",
    "isindex",
    "keygen",
    "li",
    "link",
    "listing",
    "main",
    "marquee",
    "menu",
    "meta",
    "nav",
    "noembed",
    "noframes",
    "noscript",
    "object",
    "ol",
    "p",
    "param",
    "plaintext",
    "pre",
    "script",
    "section",
    "select",
    "source",
    "style",
    "summary",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
    "wbr",
    "xmp",
];

/// A1: start tags that do NOT reconstruct active formatting (html5lib InBody
/// dispatch — every start tag outside this set runs reconstruct first).
/// `button`/`nobr`/`a`/`input`/`select`/`option`/`optgroup` reconstruct inside
/// their own handlers; the table parts are handled by the table phases, which
/// never reconstruct.
const NO_RECONSTRUCT: &[&str] = &[
    // skeleton and head-only content
    "html",
    "head",
    "body",
    "frameset",
    "frame",
    "base",
    "basefont",
    "bgsound",
    "command",
    "link",
    "meta",
    "noscript",
    "script",
    "style",
    "title",
    // the closeP block group (startTagCloseP, no reconstruct)
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "header",
    "hgroup",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "section",
    "summary",
    "ul",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    // other explicit handlers without reconstruction
    "pre",
    "listing",
    "form",
    "li",
    "dd",
    "dt",
    "plaintext",
    "hr",
    "param",
    "source",
    "track",
    "textarea",
    "iframe",
    "noembed",
    "noframes",
    "rp",
    "rt",
    // table parts (the table phases never reconstruct)
    "table",
    "caption",
    "col",
    "colgroup",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
];

/// End tags handled by html5lib endTagBlock: scope-gated, implied end tags
/// generated, then popped. Anything else falls to `end_tag_other` (a special
/// element above the target makes the end tag a no-op).
const END_TAG_BLOCK: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "button",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "header",
    "hgroup",
    "listing",
    "main",
    "menu",
    "nav",
    "ol",
    "pre",
    "section",
    "summary",
    "ul",
];

/// Elements that legitimately live inside a `<table>`. Anything else met in
/// table context is foster-parented above the table; a `<tr>` met directly in
/// the table implies `<tbody>`, a cell directly in a section implies `<tr>`.
const TABLE_SECTIONS: &[&str] = &[
    "thead", "tbody", "tfoot", "tr", "td", "th", "caption", "colgroup", "col",
];

/// A1: sibling-scoped tags (html5lib startTagListItem / startTagDDDt): a
/// same-group tag closes its nearest open sibling, but the scan stops at the
/// enclosing container (`<li>` inside an outer `<ul>` is not ours).
const SCOPE_CLOSERS: &[(&str, &[&str], &[&str])] = &[
    // (tag, same-group siblings, containers that stop the scan)
    ("li", &["li"], &["ul", "ol"]),
    ("dd", &["dd", "dt"], &["dl"]),
    ("dt", &["dd", "dt"], &["dl"]),
];

/// A1: tags that break out of foreign content (an svg/math subtree) back
/// into HTML processing (spec "in foreign content" breakout list). `font`
/// counts only when it carries color/face/size — checked at the call site.
const BREAKOUT: &[&str] = &[
    "b",
    "big",
    "blockquote",
    "br",
    "center",
    "code",
    "dd",
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "li",
    "listing",
    "menu",
    "meta",
    "nobr",
    "ol",
    "p",
    "pre",
    "ruby",
    "s",
    "small",
    "span",
    "strong",
    "strike",
    "sub",
    "sup",
    "table",
    "tt",
    "u",
    "ul",
    "var",
];

/// A1: SVG elements whose camelCase the tokenizer's ASCII-lowercasing
/// would mangle; the spec restores it when creating elements in the SVG
/// namespace (html5lib adjustForeignAttributes / SVG element table).
/// Serialization feeds resvg, which is case-sensitive.
const SVG_TAG_FIXES: &[(&str, &str)] = &[
    ("altglyph", "altGlyph"),
    ("altglyphdef", "altGlyphDef"),
    ("altglyphitem", "altGlyphItem"),
    ("animatecolor", "animateColor"),
    ("animatemotion", "animateMotion"),
    ("animatetransform", "animateTransform"),
    ("clippath", "clipPath"),
    ("feblend", "feBlend"),
    ("fecolormatrix", "feColorMatrix"),
    ("fecomponenttransfer", "feComponentTransfer"),
    ("fecomposite", "feComposite"),
    ("feconvolvematrix", "feConvolveMatrix"),
    ("fediffuselighting", "feDiffuseLighting"),
    ("fedisplacementmap", "feDisplacementMap"),
    ("fedistantlight", "feDistantLight"),
    ("fedropshadow", "feDropShadow"),
    ("feflood", "feFlood"),
    ("fefunca", "feFuncA"),
    ("fefuncb", "feFuncB"),
    ("fefuncg", "feFuncG"),
    ("fefuncr", "feFuncR"),
    ("fegaussianblur", "feGaussianBlur"),
    ("feimage", "feImage"),
    ("femerge", "feMerge"),
    ("femergenode", "feMergeNode"),
    ("femorphology", "feMorphology"),
    ("feoffset", "feOffset"),
    ("fepointlight", "fePointLight"),
    ("fespecularlighting", "feSpecularLighting"),
    ("fespotlight", "feSpotLight"),
    ("fetile", "feTile"),
    ("feturbulence", "feTurbulence"),
    ("foreignobject", "foreignObject"),
    ("glyphref", "glyphRef"),
    ("lineargradient", "linearGradient"),
    ("radialgradient", "radialGradient"),
    ("textpath", "textPath"),
];

/// A1: camelCase SVG/MathML attributes restored the same way (`viewBox`
/// on `<svg>` itself is the most common; resvg needs it verbatim).
const SVG_ATTR_FIXES: &[(&str, &str)] = &[
    ("attributename", "attributeName"),
    ("attributetype", "attributeType"),
    ("basefrequency", "baseFrequency"),
    ("baseprofile", "baseProfile"),
    ("calcmode", "calcMode"),
    ("clippathunits", "clipPathUnits"),
    ("definitionurl", "definitionURL"),
    ("diffuseconstant", "diffuseConstant"),
    ("edgemode", "edgeMode"),
    ("filterunits", "filterUnits"),
    ("glyphref", "glyphRef"),
    ("gradienttransform", "gradientTransform"),
    ("gradientunits", "gradientUnits"),
    ("kernelmatrix", "kernelMatrix"),
    ("kernelunitlength", "kernelUnitLength"),
    ("keypoints", "keyPoints"),
    ("keysplines", "keySplines"),
    ("keytimes", "keyTimes"),
    ("lengthadjust", "lengthAdjust"),
    ("limitingconeangle", "limitingConeAngle"),
    ("markerheight", "markerHeight"),
    ("markerunits", "markerUnits"),
    ("markerwidth", "markerWidth"),
    ("maskcontentunits", "maskContentUnits"),
    ("maskunits", "maskUnits"),
    ("numoctaves", "numOctaves"),
    ("pathlength", "pathLength"),
    ("patterncontentunits", "patternContentUnits"),
    ("patterntransform", "patternTransform"),
    ("patternunits", "patternUnits"),
    ("pointsatx", "pointsAtX"),
    ("pointsaty", "pointsAtY"),
    ("pointsatz", "pointsAtZ"),
    ("preservealpha", "preserveAlpha"),
    ("preserveaspectratio", "preserveAspectRatio"),
    ("primitiveunits", "primitiveUnits"),
    ("refx", "refX"),
    ("refy", "refY"),
    ("repeatcount", "repeatCount"),
    ("repeatdur", "repeatDur"),
    ("requiredextensions", "requiredExtensions"),
    ("requiredfeatures", "requiredFeatures"),
    ("specularconstant", "specularConstant"),
    ("specularexponent", "specularExponent"),
    ("spreadmethod", "spreadMethod"),
    ("startoffset", "startOffset"),
    ("stddeviation", "stdDeviation"),
    ("stitchtiles", "stitchTiles"),
    ("surfacescale", "surfaceScale"),
    ("systemlanguage", "systemLanguage"),
    ("tablevalues", "tableValues"),
    ("targetx", "targetX"),
    ("targety", "targetY"),
    ("textlength", "textLength"),
    ("viewbox", "viewBox"),
    ("viewtarget", "viewTarget"),
    ("xchannelselector", "xChannelSelector"),
    ("ychannelselector", "yChannelSelector"),
    ("zoomandpan", "zoomAndPan"),
];

/// A1: HTML integration points — content inside them parses with HTML rules
/// (foreignObject compared case-insensitively; the DOM keeps camelCase).
const HTML_IPS: &[&str] = &["foreignobject", "desc", "title", "annotation-xml"];

/// A1: MathML text integration points — HTML tags (and text) inside them
/// parse with HTML rules instead of breaking out of the foreign subtree.
const MATHML_TEXT_IPS: &[&str] = &["mi", "mo", "mn", "ms", "mtext"];

/// A1: canonical SVG spelling for a tokenizer-lowercased tag.
fn svg_tag_case(tag: &str) -> &str {
    SVG_TAG_FIXES
        .iter()
        .find(|(lower, _)| *lower == tag)
        .map_or(tag, |(_, canon)| *canon)
}

/// A1: canonical SVG spelling for a tokenizer-lowercased attribute name.
fn svg_attr_case(name: String) -> String {
    match SVG_ATTR_FIXES
        .iter()
        .find(|(lower, _)| *lower == name.as_str())
    {
        Some((_, canon)) => canon.to_string(),
        None => name,
    }
}

/// Where we are inside an open table, nearest boundary first.
#[derive(Debug, Clone, Copy, PartialEq)]
enum TableCtx {
    /// Directly inside `<table>`.
    Table,
    /// Inside a `<tbody>`/`<thead>`/`<tfoot>`.
    Section,
    /// Inside a `<tr>` with no open cell.
    Row,
    /// Inside a cell, caption, or colgroup.
    Cell,
}

pub struct TreeBuilder {
    dom: Dom,
    stack: Vec<NodeId>,
    /// Character content of the current raw-text element (script/style/...).
    /// Layout consumes `<style>` content for CSS; script content is dropped.
    head: Option<NodeId>,
    body: Option<NodeId>,
    /// Fragment mode: parse into this context element, synthesizing nothing.
    fragment: Option<NodeId>,
    /// Incremental input (Phase O4): chunks accumulate in the tokenizer
    /// across [`feed`] calls; [`finish`] drains the tail.
    tokenizer: Tokenizer,
    /// A whitespace-only run arrived while the current node already has
    /// children (Phase O4): it may be a fragment of a larger text run split
    /// across chunks, so it waits as one pending space instead of dropping
    /// like a complete whitespace-only token. Element boundaries resolve it
    /// (flush after text, drop after elements); text consumes it.
    pending_space: bool,
    /// A1: the next text run starts inside a fresh pre/listing/textarea —
    /// the spec drops one leading newline right after the open tag.
    skip_leading_newline: bool,
    /// List of active formatting elements (A1): the spec's reconstruction
    /// bookkeeping — markers (None) partition scope; entries name both the
    /// element identity (tag + key attrs) and the live node, so clones share
    /// identity but keep their own subtree position.
    active_formatting: Vec<Option<FormatEntry>>,
    /// A1: the open `<form>` element (html5lib formPointer). A second
    /// `<form>` start is dropped while one is open; `</form>` removes the
    /// pointer from the stack by identity without closing its children.
    form_pointer: Option<NodeId>,
}

/// One active-formatting-list entry (A1). `key` is the element's identity
/// for the Noah's Ark clause: at most three same-identity entries survive.
/// `attrs` travel with the entry so reconstructed/adoption clones keep the
/// original element's attributes (id, class, href, …).
#[derive(Clone)]
struct FormatEntry {
    key: String,
    tag: String,
    node: NodeId,
    attrs: Vec<(String, String)>,
}

impl FormatEntry {
    /// Identity key for the Noah's Ark clause: tag plus every attribute
    /// name/value (html5lib compares full attribute dicts, so `<font
    /// color=red>` and `<font color=blue>` are different identities).
    fn key_of(tag: &str, attrs: &[(String, String)]) -> String {
        let mut key = String::from(tag);
        for (name, value) in attrs {
            key.push(':');
            key.push_str(name);
            key.push('=');
            key.push_str(value);
        }
        key
    }
}

impl TreeBuilder {
    pub fn new() -> TreeBuilder {
        TreeBuilder {
            dom: Dom::new(),
            stack: Vec::new(),
            head: None,
            body: None,
            fragment: None,
            tokenizer: Tokenizer::new_streaming(),
            pending_space: false,
            skip_leading_newline: false,
            active_formatting: Vec::new(),
            form_pointer: None,
        }
    }

    /// Feed one response chunk: tokenize what completes and build that much
    /// of the tree immediately (progressive first paint reads `dom`).
    pub fn feed(&mut self, chunk: &str) {
        self.tokenizer.feed(chunk);
        self.drain_tokens();
    }

    /// No more chunks: tokenize the tail and return the tree.
    pub fn finish(mut self) -> (Dom, Option<NodeId>) {
        self.tokenizer.finish();
        self.drain_tokens();
        self.ensure_eof_body();
        let root = self.dom.root();
        (self.dom, root)
    }

    /// The document skeleton always ends with a `<body>` (the corpus's
    /// after-head EOF shows an empty body; browsers do the same).
    fn ensure_eof_body(&mut self) {
        if self.fragment.is_none() {
            if let Some(root) = self.dom.root() {
                if self.body.is_none() {
                    let body = self.dom.create_element("body", vec![]);
                    self.dom.append(root, body);
                    self.body = Some(body);
                }
            }
        }
    }

    /// Fragment mode: everything parses into a synthetic `context` element —
    /// the innerHTML-shaped entry point (no html/head/body synthesis).
    fn new_fragment(context: &str) -> TreeBuilder {
        let mut tb = TreeBuilder::new();
        let ctx = tb.dom.create_element(context, vec![]);
        tb.fragment = Some(ctx);
        tb.stack.push(ctx);
        if context.eq_ignore_ascii_case("plaintext") {
            tb.tokenizer.enter_plaintext();
        }
        tb
    }

    fn current(&self) -> Option<NodeId> {
        self.stack.last().copied()
    }

    fn has_element_in_stack(&self, tag: &str) -> bool {
        self.stack.iter().any(|&n| self.dom.tag(n) == tag)
    }

    /// Pop elements until `tag` is closed. If `tag` isn't open, does nothing
    /// (stray end tags are ignored). The fragment context element is never
    /// popped — a stray `</td>` in a `td`-context fragment must not unwind it.
    fn pop_until(&mut self, tag: &str) {
        if self.has_element_in_stack(tag) {
            while let Some(top) = self.stack.pop() {
                if Some(top) == self.fragment || self.dom.tag(top) == tag {
                    break;
                }
            }
        }
    }

    /// Handle `li`, `dt`/`dd`, table cells, options, ruby annotations: close
    /// the nearest open sibling in the group, but only before the enclosing
    /// container (an `<li>` in an outer `<ul>` is not ours).
    fn close_scope_sibling(&mut self, tag: &str) {
        let Some((_, group, stop)) = SCOPE_CLOSERS.iter().find(|(t, _, _)| *t == tag) else {
            return;
        };
        for &n in self.stack.iter().rev() {
            let t = self.dom.tag(n).to_string();
            if group.iter().any(|g| *g == t) {
                self.pop_until(&t);
                return;
            }
            if stop.iter().any(|s| *s == t) {
                return;
            }
        }
    }

    fn close_paragraph_if_open(&mut self) {
        if self.has_element_in_stack("p") {
            self.pop_until("p");
        }
    }

    /// A1: push a formatting element onto the active list (Noah's Ark: when
    /// three entries with the same identity key are already there, the
    /// earliest is dropped first).
    fn push_formatting_keyed(
        &mut self,
        tag: &str,
        key: String,
        node: NodeId,
        attrs: Vec<(String, String)>,
    ) {
        let same = self
            .active_formatting
            .iter()
            .filter(|e| e.as_ref().is_some_and(|e| e.key == key))
            .count();
        if same >= 3 {
            if let Some(pos) = self
                .active_formatting
                .iter()
                .position(|e| e.as_ref().is_some_and(|e| e.key == key))
            {
                self.active_formatting.remove(pos);
            }
        }
        self.active_formatting.push(Some(FormatEntry {
            key,
            tag: tag.to_string(),
            node,
            attrs,
        }));
    }

    /// A1: the last entry before the nearest marker (formatting scope).
    /// A1: entering a scope boundary (cell/caption/template/select) pushes
    /// a marker: reconstruction never crosses these. Other structural
    /// elements (p, div, …) nest inside open formatting without a marker —
    /// the spec reconstructs the chain inside them.
    fn formatting_scope_boundary(&mut self, tag: &str) {
        if matches!(tag, "td" | "th" | "caption" | "template" | "select") {
            self.active_formatting.push(None);
        }
    }

    /// A1: reconstruct the active formatting elements (html5lib-exact). If
    /// the innermost entry is a marker or still open, the chain is live at
    /// this insertion point and nothing happens; otherwise clones of the
    /// dead chain re-open outermost-first under the current node, each list
    /// entry replaced in place by its live clone (the originals keep their
    /// earlier subtrees).
    fn reconstruct_formatting(&mut self) {
        // Step 1: nothing to do when the list is empty, the innermost entry
        // is a marker, or it is still open (liveness is judged on the LAST
        // entry only, not a parent walk).
        let last = match self.active_formatting.last() {
            None => return,
            Some(None) => return,
            Some(Some(e)) if self.stack.contains(&e.node) => return,
            Some(Some(_)) => self.active_formatting.len() - 1,
        };
        // Rewind to the outermost dead entry (the dead run after the last
        // marker or live entry).
        let mut first = last;
        while first > 0 {
            match &self.active_formatting[first - 1] {
                None => break,
                Some(e) if self.stack.contains(&e.node) => break,
                _ => first -= 1,
            }
        }
        // Re-open the dead chain outermost-first under the current node.
        // The insert goes through the foster path (a clone landing in table
        // context sits above the table), and every clone joins the open
        // stack.
        for i in first..=last {
            let (tag, attrs) = match &self.active_formatting[i] {
                Some(e) => (e.tag.clone(), e.attrs.clone()),
                None => continue,
            };
            let clone = self.dom.create_element(&tag, attrs);
            if self.foster_for(&tag) {
                if let Some((parent, before)) = self.foster_target() {
                    self.dom.insert_before(parent, before, clone);
                }
            } else if let Some(parent) = self.current() {
                self.dom.append(parent, clone);
            }
            self.stack.push(clone);
            if let Some(Some(e)) = self.active_formatting.get_mut(i) {
                e.node = clone;
            }
        }
    }

    /// True when `node` is `ancestor` itself or a strict descendant.
    fn is_descendant(&self, node: NodeId, ancestor: NodeId) -> bool {
        let mut a = Some(node);
        while let Some(n) = a {
            if n == ancestor {
                return true;
            }
            a = self.dom.node(n).parent;
        }
        false
    }

    /// A1: adoption agency (spec 13.2.6.1, html5lib `endTagFormatting`).
    /// On `</fmt>` the innermost active formatting element of that tag is
    /// re-homed so misnested structure stays browser-shaped:
    /// `<b>1<p>2</b>3` → `<b>1</b><p><b>2</b>3</p>`.
    ///
    /// The outer loop (≤8) re-runs the algorithm with the same token; after
    /// the first pass the clone sits just above the furthest block with no
    /// special element above it, so the second pass takes the no-furthest-
    /// block exit and pops the clone — leaving the stack parked at the block
    /// (the tail "3" then lands inside the <p>, as in browsers).
    ///
    /// Returns true when the end tag is fully handled (the caller must not
    /// pop again); false falls through to the "any other end tag" path.
    fn adopt_misnested(&mut self, tag: &str) -> bool {
        if !FORMATTING.contains(&tag) {
            return false;
        }
        let mut outer = 0;
        while outer < 8 {
            outer += 1;
            // Step 4: the innermost active entry with this tag, searching
            // back only until the last marker (formatting scope).
            let mut fmt_idx = None;
            for (i, e) in self.active_formatting.iter().enumerate().rev() {
                match e {
                    None => break,
                    Some(e) if e.tag == tag => {
                        fmt_idx = Some(i);
                        break;
                    }
                    Some(_) => {}
                }
            }
            let Some(fmt_idx) = fmt_idx else {
                return false; // no such node: "any other end tag"
            };
            let fmt_node = self.active_formatting[fmt_idx].as_ref().unwrap().node;
            if !self.stack.contains(&fmt_node) {
                // The element is not open: drop the dead entry and stop.
                self.active_formatting.remove(fmt_idx);
                return true;
            }
            // In the stack but below a scope boundary → parse error: the
            // token is IGNORED (html5lib adoption-agency-4.4; no pop).
            if !self.element_in_scope(fmt_node) {
                return true;
            }
            // Step 5: furthest block = topmost special element above fmt.
            let fmt_stack = self
                .stack
                .iter()
                .position(|&n| n == fmt_node)
                .expect("fmt on stack");
            let Some(furthest) = self.stack[fmt_stack + 1..]
                .iter()
                .copied()
                .find(|&n| SPECIAL.contains(&self.dom.tag(n)))
            else {
                // Step 6: no furthest block — pop through the formatting
                // element and drop its active entry (well-nested close).
                while let Some(top) = self.stack.pop() {
                    if top == fmt_node {
                        break;
                    }
                }
                self.active_formatting.remove(fmt_idx);
                return true;
            };
            let furthest_stack = self
                .stack
                .iter()
                .position(|&n| n == furthest)
                .expect("furthest on stack");
            // Step 7/8: common ancestor + bookmark (the position the clone's
            // active entry takes over).
            let common_ancestor = self.stack[fmt_stack - 1];
            let mut bookmark = fmt_idx;
            // Step 9: clone chain between the furthest block and the
            // formatting element (≤3 iterations; the spec's inner loop).
            let mut last_node = furthest;
            let mut idx = furthest_stack;
            let mut inner = 0;
            while inner < 3 {
                inner += 1;
                if idx == 0 {
                    break;
                }
                idx -= 1;
                let node = self.stack[idx];
                let Some(entry_idx) = self
                    .active_formatting
                    .iter()
                    .position(|e| e.as_ref().is_some_and(|e| e.node == node))
                else {
                    // Not an active formatting element: it stays in the DOM
                    // but is no longer treated as open.
                    self.stack.remove(idx);
                    continue;
                };
                if node == fmt_node {
                    break;
                }
                if last_node == furthest {
                    bookmark = entry_idx + 1;
                }
                // Clone with attributes; take over the entry and the stack
                // slot, then adopt the previous last node as the only child.
                let (ctag, cattrs) = {
                    let e = self.active_formatting[entry_idx].as_ref().unwrap();
                    (e.tag.clone(), e.attrs.clone())
                };
                let clone = self.dom.create_element(&ctag, cattrs);
                if let Some(Some(e)) = self.active_formatting.get_mut(entry_idx) {
                    e.node = clone;
                }
                self.stack[idx] = clone;
                if let Some(p) = self.dom.node(last_node).parent {
                    self.dom.detach(p, last_node);
                }
                self.dom.append(clone, last_node);
                last_node = clone;
            }
            // Step 10: move last_node into the common ancestor (fostered
            // above the table when the ancestor is table context).
            if let Some(p) = self.dom.node(last_node).parent {
                self.dom.detach(p, last_node);
            }
            if matches!(
                self.dom.tag(common_ancestor),
                "table" | "tbody" | "tfoot" | "thead" | "tr"
            ) {
                if let Some((parent, before)) = self.foster_target() {
                    self.dom.insert_before(parent, before, last_node);
                }
                // no table ancestor: the node is dropped (html5lib behavior)
            } else {
                self.dom.append(common_ancestor, last_node);
            }
            // Step 11: clone the formatting element — attributes travel with
            // it (id/class/href …), which is what makes the adopted subtree
            // render with the original formatting.
            let (ftag, fattrs, fkey) = {
                let e = self.active_formatting[fmt_idx].as_ref().unwrap();
                (e.tag.clone(), e.attrs.clone(), e.key.clone())
            };
            let fmt_clone = self.dom.create_element(&ftag, fattrs.clone());
            // Step 12/13: reparent the furthest block's children into the
            // clone, then append the clone INTO the furthest block.
            for c in self.dom.children(furthest).to_vec() {
                self.dom.detach(furthest, c);
                self.dom.append(fmt_clone, c);
            }
            self.dom.append(furthest, fmt_clone);
            // Step 14: active list — the clone takes over the formatting
            // entry at the bookmark position.
            self.active_formatting.remove(fmt_idx);
            let bm = if bookmark > fmt_idx {
                bookmark - 1
            } else {
                bookmark
            };
            self.active_formatting.insert(
                bm.min(self.active_formatting.len()),
                Some(FormatEntry {
                    key: fkey,
                    tag: ftag,
                    node: fmt_clone,
                    attrs: fattrs,
                }),
            );
            // Step 15: stack — swap the formatting element for the clone
            // immediately above the furthest block, then loop: the next pass
            // finds no furthest block and pops the clone (see doc comment).
            let fpos = self
                .stack
                .iter()
                .position(|&n| n == fmt_node)
                .expect("fmt on stack");
            self.stack.remove(fpos);
            let upos = self
                .stack
                .iter()
                .position(|&n| n == furthest)
                .expect("furthest on stack");
            self.stack.insert(upos + 1, fmt_clone);
        }
        true
    }

    /// A1: default-scope membership for an open element — true while no
    /// scoping boundary (table cell, table, template, …) sits between the
    /// top of the stack and `node`.
    fn element_in_scope(&self, node: NodeId) -> bool {
        for &n in self.stack.iter().rev() {
            if n == node {
                return true;
            }
            if matches!(
                self.dom.tag(n),
                "applet"
                    | "caption"
                    | "html"
                    | "table"
                    | "td"
                    | "th"
                    | "marquee"
                    | "object"
                    | "template"
            ) {
                return false;
            }
        }
        false
    }

    /// The foster target for stray table content: (foster parent, insert
    /// before this node) — the last open table's position. `None` when the
    /// table is rootless, in which case the spec drops the node.
    fn foster_target(&self) -> Option<(NodeId, NodeId)> {
        let table = *self
            .stack
            .iter()
            .rev()
            .find(|&&n| self.dom.tag(n) == "table")?;
        let parent = self.dom.node(table).parent?;
        Some((parent, table))
    }

    /// True when `tag`, starting now, must be foster-parented: it is not a
    /// table section and the current node is table/section/row context.
    fn foster_for(&self, tag: &str) -> bool {
        if TABLE_SECTIONS.contains(&tag) {
            return false;
        }
        let Some(cur) = self.current() else {
            return false;
        };
        if !matches!(
            self.dom.tag(cur),
            "table" | "tbody" | "thead" | "tfoot" | "tr"
        ) && !(tag == "plaintext" && self.dom.tag(cur) == "colgroup")
        {
            return false;
        }
        self.foster_target().is_some()
    }

    /// Where we are inside an open table, nearest boundary first.
    fn table_context(&self) -> Option<TableCtx> {
        for &n in self.stack.iter().rev() {
            match self.dom.tag(n) {
                "table" => return Some(TableCtx::Table),
                "tbody" | "thead" | "tfoot" => return Some(TableCtx::Section),
                "tr" => return Some(TableCtx::Row),
                "td" | "th" | "caption" | "colgroup" => return Some(TableCtx::Cell),
                _ => {}
            }
        }
        None
    }

    /// html5lib `clearStackToTableContext`, approximated for the phaseless
    /// builder: pop fostered junk (formatting elements left on the stack by
    /// table voodoo) until the current node is a table-structure element.
    fn clear_stack_to_table_context(&mut self) {
        while self.stack.len() > 2 {
            let Some(cur) = self.current() else { break };
            if matches!(
                self.dom.tag(cur),
                "table"
                    | "tbody"
                    | "thead"
                    | "tfoot"
                    | "tr"
                    | "td"
                    | "th"
                    | "caption"
                    | "colgroup"
                    | "col"
                    | "html"
            ) {
                break;
            }
            self.stack.pop();
        }
    }

    /// Push the implied `<tbody>`/`<tr>` wrappers a table section start needs:
    /// a `<tr>` straight in the table implies `<tbody>`; a cell straight in a
    /// section implies `<tr>`; a cell straight in the table implies both.
    fn ensure_table_ancestry(&mut self, tag: &str, ctx: Option<TableCtx>) {
        match ctx {
            Some(TableCtx::Table) => match tag {
                "tr" => {
                    self.push_element("tbody", vec![]);
                }
                "td" | "th" => {
                    self.push_element("tbody", vec![]);
                    self.push_element("tr", vec![]);
                }
                _ => {}
            },
            Some(TableCtx::Section) => {
                if matches!(tag, "td" | "th") {
                    self.push_element("tr", vec![]);
                }
            }
            _ => {}
        }
    }

    /// Insert a text run into the current element, merging with the previous
    /// sibling when both are text (keeps the node count small). Stray text in
    /// table context (not inside a cell) is foster-parented above the table.
    fn insert_text(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if let Some(cur) = self.current() {
            if matches!(
                self.dom.tag(cur),
                "table" | "tbody" | "thead" | "tfoot" | "tr"
            ) {
                if let Some((parent, before)) = self.foster_target() {
                    // html5lib inserts fostered text at the table's position
                    // with text-merge semantics: a run continues the text node
                    // immediately before the table rather than starting a new
                    // sibling (so `<i>1<table>…</table>` fostered text lands as
                    // "1…" inside the formatting element).
                    let prev_text = {
                        let kids = self.dom.children(parent);
                        kids.iter()
                            .position(|&k| k == before)
                            .filter(|&i| i > 0)
                            .map(|i| kids[i - 1])
                            .filter(|&k| self.dom.node(k).kind == kitae_dom::NodeKind::Text)
                    };
                    match prev_text {
                        Some(pt) => {
                            let merged = format!("{}{}", self.dom.node(pt).text, text);
                            self.dom.detach(parent, pt);
                            let tid = self.dom.create_text(merged);
                            self.dom.insert_before(parent, before, tid);
                        }
                        None => {
                            let tid = self.dom.create_text(text);
                            self.dom.insert_before(parent, before, tid);
                        }
                    }
                }
                // No foster target (rootless table): the text is dropped,
                // matching the spec's "insert nowhere".
                return;
            }
        }
        let Some(parent) = self.current() else {
            // Text before <html>: synthesize the root.
            let root = self.dom.create_element("html", vec![]);
            self.stack.push(root);
            let tid = self.dom.create_text(text);
            self.dom.append(root, tid);
            return;
        };
        if let Some(&last) = self.dom.children(parent).last() {
            let last_node = self.dom.node(last);
            if last_node.kind == kitae_dom::NodeKind::Text {
                let merged = format!("{}{}", last_node.text, text);
                // Rebuild the text node's content by replacing it: drop + append.
                self.dom.detach(parent, last);
                let tid = self.dom.create_text(merged);
                self.dom.append(parent, tid);
                return;
            }
        }
        let tid = self.dom.create_text(text);
        self.dom.append(parent, tid);
    }

    fn push_element(&mut self, tag: &str, attrs: Vec<(String, String)>) -> NodeId {
        let el = self.dom.create_element(tag, attrs);
        if let Some(parent) = self.current() {
            self.dom.append(parent, el);
        }
        if !VOID_ELEMENTS.contains(&tag) {
            self.stack.push(el);
        }
        el
    }

    pub fn build(mut self, html: &str) -> (Dom, Option<NodeId>) {
        self.build_from(html);
        self.ensure_eof_body();
        let root = self.dom.root();
        (self.dom, root)
    }

    fn build_from(&mut self, html: &str) {
        self.tokenizer.feed(html);
        self.tokenizer.finish();
        self.drain_tokens();
    }

    /// Run ready tokens through the tree-construction handlers until the
    /// tokenizer waits for more input.
    fn drain_tokens(&mut self) {
        while let Some(token) = self.tokenizer.next_token() {
            match token {
                // Doctypes and comments are parsed faithfully by the
                // tokenizer but not yet carried in the DOM (known gap,
                // docs/WPT_SCORECARD.md).
                Token::Doctype { .. } | Token::Comment(_) => {}
                Token::Text(text) => {
                    let foreign_text = self.foreign_phase();
                    let text: String = text
                        .chars()
                        .filter_map(|c| {
                            if c != '\0' {
                                Some(c)
                            } else if foreign_text {
                                Some('\u{FFFD}')
                            } else {
                                None
                            }
                        })
                        .collect();
                    if text.is_empty() {
                        continue;
                    }
                    let in_raw = self
                        .current()
                        .map(|n| {
                            let t = self.dom.tag(n);
                            matches!(
                                t,
                                "script"
                                    | "style"
                                    | "pre"
                                    | "textarea"
                                    | "xmp"
                                    | "iframe"
                                    | "noembed"
                                    | "noframes"
                            )
                        })
                        .unwrap_or(false);
                    if in_raw {
                        // Keep whitespace verbatim inside raw-content elements.
                        self.pending_space = false;
                        // A1: one leading newline right after the open tag is
                        // source formatting, not content (spec rule).
                        let text = if self.skip_leading_newline {
                            self.skip_leading_newline = false;
                            text.strip_prefix('\n').map(str::to_string).unwrap_or(text)
                        } else {
                            text
                        };
                        self.insert_text(text);
                    } else if text
                        .chars()
                        .any(|c| !c.is_ascii_whitespace() && c != '\u{b}')
                    {
                        // Collapse runs of whitespace to single spaces (CSS-ish
                        // default). ASCII whitespace only per HTML: NBSP and
                        // other Unicode spaces are content and must survive to
                        // layout (UAX #14 keeps them unbreakable, Q13).
                        let is_collapse_ws = |c: char| c.is_ascii_whitespace() || c == '\u{b}';
                        let mut collapsed = String::with_capacity(text.len());
                        let mut last_ws = false;
                        for c in text.chars() {
                            if is_collapse_ws(c) {
                                if !last_ws {
                                    collapsed.push(' ');
                                }
                                last_ws = true;
                            } else {
                                collapsed.push(c);
                                last_ws = false;
                            }
                        }
                        if self.pending_space {
                            // Join the space split across chunks, then collapse
                            // as one run (batch-equivalent).
                            collapsed = collapsed.trim_start().to_string();
                            if !collapsed.starts_with(' ') {
                                collapsed.insert(0, ' ');
                            }
                            self.pending_space = false;
                        }
                        // CSS collapsing: no leading space at the start of a block.
                        let first_child = self
                            .current()
                            .map(|p| self.dom.children(p).is_empty())
                            .unwrap_or(true);
                        if first_child {
                            collapsed = collapsed.trim_start().to_string();
                        }
                        // Spec routing: character tokens arriving in
                        // before-head/in-head/after-head hit the
                        // "anything else" rule — insert <body> and
                        // reprocess, so text never lands under <html>
                        // or <head> (matches the corpus and browsers).
                        if self.fragment.is_none() && self.body.is_none() {
                            let in_pre_body = self
                                .current()
                                .map(|n| {
                                    let t = self.dom.tag(n);
                                    t == "html" || t == "head"
                                })
                                .unwrap_or(true); // rootless: text before <html>
                            if in_pre_body {
                                if self.dom.root().is_none() {
                                    let root = self.dom.create_element("html", vec![]);
                                    self.stack.push(root);
                                }
                                self.handle_start("body", vec![], false);
                            }
                        }
                        // A1: reconstruct active formatting before the text
                        // lands (the spec runs this at every insertion).
                        self.reconstruct_formatting();
                        self.insert_text(collapsed);
                    } else {
                        // Whitespace-only run: drop when the node is still
                        // empty (leading/formatting whitespace, as in batch);
                        // otherwise hold one pending space — it may join text
                        // split across chunks.
                        let has_children = self
                            .current()
                            .map(|p| !self.dom.children(p).is_empty())
                            .unwrap_or(false);
                        if has_children {
                            self.pending_space = true;
                        }
                    }
                }
                Token::StartTag {
                    name,
                    attrs,
                    self_closing,
                } => {
                    self.handle_start(&name, attrs, self_closing);
                }
                Token::EndTag { name } => {
                    self.handle_end(&name);
                }
            }
        }
    }

    fn handle_start(&mut self, tag: &str, attrs: Vec<(String, String)>, self_closing: bool) {
        // Fragments parse into their context element; the document skeleton
        // is never synthesized there.
        if self.fragment.is_some() && matches!(tag, "html" | "head" | "body") {
            return;
        }
        match tag {
            "html" => {
                if self.dom.root().is_none() {
                    let root = self.dom.create_element("html", attrs);
                    self.stack.push(root);
                }
            }
            "head" => {
                if self.head.is_none() {
                    let html = self.dom.root().unwrap_or_else(|| {
                        let r = self.dom.create_element("html", vec![]);
                        self.stack.push(r);
                        r
                    });
                    let head = self.dom.create_element("head", attrs);
                    self.dom.append(html, head);
                    self.stack.push(head);
                    self.head = Some(head);
                }
            }
            "body" => {
                self.pop_until("head");
                if self.body.is_none() {
                    let html = self.dom.root().unwrap_or_else(|| {
                        let r = self.dom.create_element("html", vec![]);
                        self.stack.push(r);
                        r
                    });
                    let body = self.dom.create_element("body", attrs);
                    self.dom.append(html, body);
                    self.stack.push(body);
                    self.body = Some(body);
                }
            }
            _ => {
                // Implicit structure: content before <head> creates head-ish
                // context for meta/title/style/link; content after implies body.
                if self.fragment.is_none() {
                    if self.dom.root().is_none() {
                        let root = self.dom.create_element("html", vec![]);
                        self.stack.push(root);
                    }
                    if self.body.is_none() {
                        // The spec's in-head element set: before <head> they
                        // create it; after a closed <head> they reprocess
                        // there (the after-head "anything else" split).
                        let in_head_tag = matches!(
                            tag,
                            "base"
                                | "basefont"
                                | "bgsound"
                                | "link"
                                | "meta"
                                | "noframes"
                                | "noscript"
                                | "script"
                                | "style"
                                | "template"
                                | "title"
                        );
                        // A2: while a <template> is open its content stays
                        // inside the template element (inert). The template
                        // insertion modes are not modelled, but the content
                        // must never leak into head or body — falling through
                        // appends it to the open template on the stack.
                        let template_open =
                            self.stack.iter().any(|&n| self.dom.tag(n) == "template");
                        if template_open {
                            // content appends to the open template below
                        } else if self.head.is_none() && in_head_tag {
                            self.handle_start("head", vec![], false);
                        } else if self.head.is_some() && in_head_tag {
                            // Reprocess in head: unwind to <head> (inclusive
                            // — popping an already-closed head is a no-op),
                            // then re-push it as the insertion point.
                            self.pop_until("head");
                            self.stack.push(self.head.unwrap());
                        } else {
                            self.handle_start("body", vec![], false);
                        }
                    }
                }

                // A1: `<svg>`/`<math>` keep their lowercase names but restore
                // camelCase attributes (viewBox etc.) the tokenizer lowercased.
                let attrs = if matches!(tag, "svg" | "math") {
                    attrs
                        .into_iter()
                        .map(|(n, v)| (svg_attr_case(n), v))
                        .collect()
                } else {
                    attrs
                };

                // A1: foreign content (spec "in foreign content"). The
                // dispatcher looks only at the current node: an integration
                // point (foreignObject/desc/title, MathML text elements)
                // runs HTML rules in place; otherwise inside svg/math a
                // breakout tag pops the foreign subtree and re-runs with
                // HTML rules below, and any other tag inserts as a foreign
                // element with canonical SVG casing, skipping HTML steps.
                let foreign = self.foreign_phase();
                let breakout = foreign
                    && (BREAKOUT.contains(&tag)
                        || (tag == "font"
                            && attrs
                                .iter()
                                .any(|(n, _)| matches!(n.as_str(), "color" | "face" | "size"))));
                if breakout {
                    while let Some(top) = self.stack.pop() {
                        if matches!(self.dom.tag(top), "svg" | "math") {
                            break;
                        }
                    }
                } else if foreign {
                    if tag == "plaintext" {
                        self.tokenizer.cancel_plaintext();
                    }
                    let attrs = attrs
                        .into_iter()
                        .map(|(n, v)| (svg_attr_case(n), v))
                        .collect();
                    let el = self.dom.create_element_verbatim(svg_tag_case(tag), attrs);
                    if let Some(parent) = self.current() {
                        self.dom.append(parent, el);
                    }
                    self.stack.push(el);
                    if self_closing {
                        self.stack.pop();
                    }
                    return;
                }

                if tag == "plaintext" {
                    let mut p_in_button_scope = false;
                    for &n in self.stack.iter().rev() {
                        if self.dom.tag(n) == "p" {
                            p_in_button_scope = true;
                            break;
                        }
                        if self.dom.tag(n) == "button" {
                            break;
                        }
                    }
                    if p_in_button_scope {
                        self.close_paragraph_if_open();
                    }
                }
                if CLOSES_P.contains(&tag) {
                    self.close_paragraph_if_open();
                }
                // A1: `<image>` is a legacy alias of `<img>`.
                let tag_ref: &str = if tag == "image" { "img" } else { tag };
                let tag = tag_ref.to_string();
                let tag = tag.as_str();
                // A1: one form per document — a nested <form> is dropped.
                if tag == "form" && self.has_element_in_stack("form") {
                    return;
                }
                // A1: a <button> start closes an in-scope open button first
                // (spec startTagButton: implied end tags, pop through button)
                // — even across a nested <p>.
                if tag == "button" && self.element_in_scope_by_tag("button") {
                    self.handle_end("button");
                }
                // A1: <nobr> closes an in-scope nobr via the adoption agency,
                // then opens fresh (spec startTagNobr).
                if tag == "nobr" {
                    self.reconstruct_formatting();
                    if self.element_in_scope_by_tag("nobr") {
                        self.handle_end("nobr");
                        self.reconstruct_formatting();
                    }
                }
                // A1: <select> accepts only options/optgroups/hr — anything
                // else in select context is dropped (no script UI to show).
                if self.in_select_context()
                    && !matches!(tag, "option" | "optgroup" | "hr" | "select" | "plaintext")
                {
                    return;
                }
                // A1: a <select> start while one is open closes it first
                // (selects never nest in practice; spec pops to the select).
                if tag == "select" && self.has_element_in_stack("select") {
                    self.pop_until("select");
                }
                // A1: active-formatting reconstruction — structure that opens
                // while formatting elements hold the tail wraps in them.
                // html5lib runs reconstruct for every start tag except the
                // NO_RECONSTRUCT handlers (closeP blocks, headings, pre,
                // form, list items, hr, rawtext, the table parts, …).
                if !NO_RECONSTRUCT.contains(&tag) {
                    self.reconstruct_formatting();
                }
                self.formatting_scope_boundary(tag);
                // A1: <pre>/<listing>/<textarea> swallow one leading newline
                // (spec: the tokenizer's first \n after the open tag is
                // ignored so a pretty-printed source doesn't shift content).
                if matches!(tag, "pre" | "listing" | "textarea") {
                    self.skip_leading_newline = true;
                }
                // Consecutive headings are siblings.
                if HEADINGS.contains(&tag) {
                    if let Some(cur) = self.current() {
                        if HEADINGS.contains(&self.dom.tag(cur)) {
                            self.stack.pop();
                        }
                    }
                }
                // A new <optgroup> closes a currently-open <option> AND
                // <optgroup> first (html5lib "in select" startTagOptgroup).
                if tag == "optgroup" {
                    while let Some(cur) = self.current() {
                        match self.dom.tag(cur) {
                            "option" | "optgroup" => {
                                self.stack.pop();
                            }
                            _ => break,
                        }
                    }
                }
                if SCOPE_CLOSERS.iter().any(|(t, _, _)| *t == tag) {
                    self.close_scope_sibling(tag);
                }

                // Tables: html5lib table-phase clearing. Fostered formatting
                // left on the stack by "table voodoo" is popped before a
                // section tag re-enters the table, and a nested <table>
                // closes the open one first (end-tag-table + reprocess).
                if tag == "table" && self.table_context().is_some() {
                    self.clear_stack_to_table_context();
                    self.pop_until("table");
                }
                if TABLE_SECTIONS.contains(&tag) {
                    if self.table_context().is_some() {
                        self.clear_stack_to_table_context();
                    }
                    let ctx = self.table_context();
                    self.ensure_table_ancestry(tag, ctx);
                }

                // <a> inside <a> closes the first (browsers do this too).
                if tag == "a" && self.has_element_in_stack("a") {
                    self.pop_until("a");
                }

                // A1: the formatting identity (and the attributes the
                // reconstruction re-creates with) is captured before `attrs`
                // moves into the element.
                let fmt = if FORMATTING.contains(&tag) {
                    Some((FormatEntry::key_of(tag, &attrs), attrs.clone()))
                } else {
                    None
                };
                let inserted = if self.foster_for(tag) {
                    let (parent, before) = self.foster_target().expect("foster_for checked");
                    let el = self.dom.create_element(tag, attrs);
                    self.dom.insert_before(parent, before, el);
                    if !VOID_ELEMENTS.contains(&tag) {
                        self.stack.push(el);
                    }
                    Some(el)
                } else {
                    Some(self.push_element(tag, attrs))
                };
                // A1: track open formatting elements for reconstruction.
                if let (Some(el), Some((key, attrs))) = (inserted, fmt) {
                    if !VOID_ELEMENTS.contains(&tag) {
                        self.push_formatting_keyed(tag, key, el, attrs);
                    }
                }
                // A1: foreign insertion already honored `/`; this path only
                // serves HTML-mode elements, where `/` is ignored.
            }
        }
    }

    /// A1: the spec dispatcher decides foreign vs HTML processing from the
    /// current node alone — walking from the top of the stack, the first
    /// svg/math/integration-point element wins. Inside an integration point
    /// everything (even nested foreign elements) runs HTML rules again.
    fn foreign_phase(&self) -> bool {
        for &n in self.stack.iter().rev() {
            let t = self.dom.tag(n);
            if matches!(t, "svg" | "math") {
                return true;
            }
            if self.is_integration_point(n) {
                return false;
            }
        }
        false
    }

    /// A1: integration points switch content back to HTML rules — svg
    /// foreignObject/desc/title, MathML annotation-xml (only with an HTML
    /// encoding attribute, per spec), and the MathML text integration
    /// points. foreignObject is compared case-insensitively because the DOM
    /// keeps its camelCase spelling.
    fn is_integration_point(&self, n: NodeId) -> bool {
        let t = self.dom.tag(n);
        if HTML_IPS[..3].iter().any(|ip| t.eq_ignore_ascii_case(ip)) {
            return true;
        }
        if t == "annotation-xml" {
            let enc = self
                .dom
                .element(n)
                .and_then(|e| e.attr("encoding"))
                .unwrap_or("");
            return matches!(enc, "text/html" | "application/xhtml+xml");
        }
        MATHML_TEXT_IPS.contains(&t)
    }

    /// A1: default-scope membership by tag (html5lib elementInScope) — walks
    /// the stack from the top, stopping at the scope stoppers.
    fn element_in_scope_by_tag(&self, tag: &str) -> bool {
        for &n in self.stack.iter().rev() {
            let t = self.dom.tag(n);
            if t == tag {
                return true;
            }
            if matches!(
                t,
                "applet"
                    | "caption"
                    | "html"
                    | "table"
                    | "td"
                    | "th"
                    | "marquee"
                    | "object"
                    | "template"
            ) {
                return false;
            }
        }
        false
    }

    /// A1: true while an open `<select>` is the nearest scope (its content
    /// model accepts only option/optgroup/hr; everything else is dropped).
    fn in_select_context(&self) -> bool {
        for &n in self.stack.iter().rev() {
            match self.dom.tag(n) {
                "select" => return true,
                "option" | "optgroup" => {}
                _ => return false,
            }
        }
        false
    }

    fn handle_end(&mut self, tag: &str) {
        match tag {
            "html" | "body" | "head" => { /* keep tree open; stack unwinds at EOF */ }
            "br" => {
                // `</br>` is treated as `<br>` in browsers.
                self.push_element("br", vec![]);
            }
            _ => {
                // A1: restore canonical SVG casing so `</foreignobject>`
                // matches the verbatim-cased DOM element.
                let tag = svg_tag_case(tag);
                // A1: end tags in foreign content pop through the matching
                // foreign element (case-insensitive); reaching the svg/math
                // root first falls through to HTML rules (spec "in foreign
                // content" end tag).
                if self.foreign_phase() {
                    let mut pops = 0usize;
                    let mut matched = false;
                    for &n in self.stack.iter().rev() {
                        let t = self.dom.tag(n);
                        pops += 1;
                        if t.eq_ignore_ascii_case(tag) {
                            matched = true;
                            break;
                        }
                        if matches!(t, "svg" | "math") {
                            break;
                        }
                    }
                    if matched {
                        for _ in 0..pops {
                            self.stack.pop();
                        }
                        return;
                    }
                }
                if VOID_ELEMENTS.contains(&tag) {
                    return;
                }
                // A1: adoption agency runs before the "not on the stack"
                // guard — html5lib consumes `</fmt>` even when the active
                // entry is dead (not-in-stack → drop the entry and stop).
                if self.adopt_misnested(tag) {
                    return;
                }
                if !self.has_element_in_stack(tag) {
                    return; // stray end tag — ignore
                }
                // A1: a formatting end tag that falls through the agency
                // (its entry was marker-blocked, e.g. by an open <td>) is
                // routed to the "any other end tag" path, which IGNORES the
                // token when a special element sits above the target.
                if FORMATTING.contains(&tag) {
                    let mut blocked = false;
                    for &n in self.stack.iter().rev() {
                        let t = self.dom.tag(n);
                        if t == tag {
                            break;
                        }
                        if SPECIAL.contains(&t) {
                            blocked = true;
                            break;
                        }
                    }
                    if blocked {
                        return;
                    }
                }
                let closed = self.stack.iter().copied().find(|&n| self.dom.tag(n) == tag);
                self.pop_until(tag);
                // A1: closed formatting leaves the active list (its node
                // stops wrapping later reconstructed structure). Markers are
                // never dropped here — only the entry naming the closed node.
                if let Some(closed) = closed {
                    self.active_formatting
                        .retain(|e| e.as_ref().map_or(true, |e| e.node != closed));
                }
                // A1: closing a structural element pops the marker it
                // pushed, releasing the formatting scope below it.
                if SPECIAL.contains(&tag) && !FORMATTING.contains(&tag) {
                    if let Some(pos) = self.active_formatting.iter().rposition(|e| e.is_none()) {
                        self.active_formatting.truncate(pos);
                    }
                }
            }
        }
    }
}

/// Parse an HTML document into a Dom. Returns the dom and the `<html>` root.
pub fn parse(html: &str) -> (Dom, Option<NodeId>) {
    TreeBuilder::new().build(html)
}

/// Parse from response chunks (Phase O4): feeds each chunk through the same
/// incremental path the streaming loader uses, then finishes. Output is
/// identical to [`parse`] of the concatenation.
pub fn parse_streaming(chunks: &[&str]) -> (Dom, Option<NodeId>) {
    let mut tb = TreeBuilder::new();
    for chunk in chunks {
        tb.feed(chunk);
    }
    tb.finish()
}

/// Parse an HTML *fragment* into a fresh context element (`<div>`, `<td>`,
/// `<ul>`, …): no html/head/body synthesis, the context element is the root
/// of the returned tree. This is the innerHTML-shaped entry point.
pub fn parse_fragment(html: &str, context: &str) -> (Dom, NodeId) {
    let mut tb = TreeBuilder::new_fragment(context);
    tb.build_from(html);
    let root = tb
        .fragment
        .expect("fragment context is set by new_fragment");
    (tb.dom, root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kitae_dom::NodeKind;

    fn body_children(html: &str) -> Vec<(NodeKind, String)> {
        let (dom, _) = parse(html);
        let mut out = Vec::new();
        if let Some(root) = dom.root() {
            // find body
            let mut body = None;
            dom.walk(root, &mut |n| {
                if dom.tag(n) == "body" {
                    body = Some(n);
                }
            });
            if let Some(b) = body {
                for &c in dom.children(b) {
                    let n = dom.node(c);
                    match n.kind {
                        NodeKind::Element => out.push((n.kind, n.tag().to_string())),
                        NodeKind::Text => out.push((n.kind, n.text.clone())),
                    }
                }
            }
        }
        out
    }

    /// Children of the first `<p>` in the document (element tag or text).
    fn p_children(html: &str) -> Vec<(NodeKind, String)> {
        let (dom, root) = parse(html);
        let mut p = None;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "p" && p.is_none() {
                p = Some(n);
            }
        });
        let mut out = Vec::new();
        if let Some(pn) = p {
            for &c in dom.children(pn) {
                let n = dom.node(c);
                match n.kind {
                    NodeKind::Element => out.push((n.kind, n.tag().to_string())),
                    NodeKind::Text => out.push((n.kind, n.text.clone())),
                }
            }
        }
        out
    }

    /// Path of tags from the document root down to the first element with
    /// `tag`, e.g. `["html", "body", "table", "tbody", "tr", "td"]`.
    fn path_to(html: &str, tag: &str) -> Vec<String> {
        let (dom, root) = parse(html);
        let root = root.expect("document has a root");
        let mut target = None;
        dom.walk(root, &mut |n| {
            if target.is_none() && dom.tag(n) == tag {
                target = Some(n);
            }
        });
        let mut path = Vec::new();
        let mut cur = target.expect("tag present");
        loop {
            path.push(dom.tag(cur).to_string());
            match dom.node(cur).parent {
                Some(p) => cur = p,
                None => break,
            }
        }
        path.reverse();
        path
    }

    #[test]
    fn parses_nested_structure() {
        let (dom, root) = parse("<html><body><div id='a'><p>hi</p></div></body></html>");
        assert!(root.is_some());
        let mut p_count = 0;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "p" {
                p_count += 1;
            }
        });
        assert_eq!(p_count, 1);
    }

    #[test]
    fn implies_head_and_body() {
        let kids = body_children("<title>t</title><p>hello</p>");
        // <title> goes to head, <p> to body
        assert_eq!(kids, vec![(NodeKind::Element, "p".into())]);
    }

    #[test]
    fn void_elements_have_no_children() {
        // p -> [text "a", br, text "b"]
        let kids = p_children("<p>a<br>b</p>");
        assert_eq!(
            kids,
            vec![
                (NodeKind::Text, "a".into()),
                (NodeKind::Element, "br".into()),
                (NodeKind::Text, "b".into()),
            ]
        );
    }

    #[test]
    fn mismatched_tags_recover() {
        let kids = body_children("<div><span>text</div></span><p>after</p>");
        // div closes early; stray </span> ignored; <p> still lands in body
        assert_eq!(kids.len(), 2);
    }

    /// A1: the canonical adoption-agency case. html5lib ground truth for
    /// `<b>1<p>2</b>3`: the p becomes a SIBLING of the original b (depth 2),
    /// the "2" lands in a clone-b INSIDE the p (depth 3), and the tail "3"
    /// also sits inside the p — the AA outer loop re-runs with the same
    /// token and its second pass pops the clone off the stack, parking the
    /// current node at the p.
    #[test]
    fn adoption_agency_b_p_b() {
        let (dom, root) = parse("<b>1<p>2</b>3");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<p>[2] <b>[3] \"2\" \"3\""),
            "p sibling of b at [2], clone-b and tail inside p: {shape}"
        );
    }

    /// A1: `<b>1<p>2</p>3` — no adoption agency here: the p opens while b
    /// is current, so the p nests INSIDE b and stays there; the tail "3"
    /// also stays inside b. No reconstruction of "2" happens because the
    /// b entry is still on the stack (active, not dead).
    #[test]
    fn formatting_held_open_across_p() {
        let (dom, root) = parse("<b>1<p>2</p>3");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<b>[2] \"1\" <p>[3] \"2\" \"3\""),
            "p nests inside the still-open b, tail stays in b: {shape}"
        );
    }

    /// A1: `<p>1<b>2</p>3` — the b closes visually with the p, but the
    /// formatting stays ACTIVE: the tail "3" reconstructs into a new b,
    /// exactly like browsers.
    #[test]
    fn formatting_does_not_leak_after_p_close() {
        let (dom, root) = parse("<p>1<b>2</p>3");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<p>[2] \"1\" <b>[3] \"2\""),
            "b's text stays inside the p: {shape}"
        );
        assert!(
            shape.contains("<b>[2] \"3\""),
            "the tail reconstructs into a sibling b: {shape}"
        );
    }

    // A1 battery — every shape below is html5lib 1.1 output (verified with
    // the same T[depth] annotation tree_shape uses).

    /// Inner loop clone chain: the <i> between the furthest block (p) and
    /// the formatting element (b) is cloned to body level; the p and the
    /// clone-b re-home inside it; the tail stays with the p.
    #[test]
    fn adoption_agency_clone_chain() {
        let (dom, root) = parse("<b>1<i>2<p>3</b>4");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<b>[2] \"1\" <i>[3] \"2\" <i>[2] <p>[3] <b>[4] \"3\" \"4\""),
            "clone chain re-homes under body: {shape}"
        );
    }

    /// Outer-loop re-run with a formatting tag between p and b: after pass
    /// one re-homes b/p, pass two pops the trailing i-clone so the tail "4"
    /// lands directly under the p as a fresh i.
    #[test]
    fn adoption_agency_outer_loop_rerun() {
        let (dom, root) = parse("<b>1<p>2<i>3</b>4");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<p>[2] <b>[3] \"2\" <i>[4] \"3\" <i>[3] \"4\""),
            "tail re-opens as i directly under p: {shape}"
        );
    }

    /// The clone keeps the formatting element's attributes.
    #[test]
    fn adoption_agency_keeps_attributes() {
        let (dom, root) = parse("<a href='/x' id='k'>1<div>2</a>3");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<a>[2] \"1\" <div>[2] <a>[3] \"2\" \"3\""),
            "div sibling of a, clone-a inside div: {shape}"
        );
        // Both anchors carry the original attributes.
        let mut hrefs = Vec::new();
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "a" {
                hrefs.push(
                    dom.element(n)
                        .and_then(|e| e.attr("href").map(str::to_string)),
                );
            }
        });
        assert_eq!(
            hrefs,
            vec![Some("/x".into()), Some("/x".into())],
            "{hrefs:?}"
        );
    }

    /// Two p's inside b: the first stays nested (no AA at </p>), the second
    /// is adopted out with a clone-b; the tail lands inside the second p.
    #[test]
    fn adoption_agency_two_paragraphs() {
        let (dom, root) = parse("<b><p>1<p>2</b>3");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<b>[2] <p>[3] \"1\" <p>[2] <b>[3] \"2\" \"3\""),
            "first p nested, second adopted out: {shape}"
        );
    }

    /// Well-nested close: </b> pops through and clears the active entry, so
    /// nothing reconstructs and the i stays dead.
    #[test]
    fn formatting_well_nested_close_no_reconstruct() {
        let (dom, root) = parse("<b>1<i>2</b>3");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<b>[2] \"1\" <i>[3] \"2\" <i>[2] \"3\""),
            "i reconstructs at body level, b does not: {shape}"
        );
    }

    /// Nested formatting wrapper: em is NOT special, so the p is a furthest
    /// block and the AA fires — the clone-em wraps "text" INSIDE the p
    /// (html5lib ground truth).
    #[test]
    fn adoption_agency_em_p_em() {
        let (dom, root) = parse("<em><p>text</em>");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<em>[2] <p>[2] <em>[3] \"text\""),
            "clone-em inside p wraps the text: {shape}"
        );
    }

    /// Marker-blocked formatting: `<td>` pushes an active-formatting marker,
    /// so `</b>` has no entry before it and falls to the "any other end tag"
    /// path — which IGNORES the token because the special <td> sits above the
    /// <b>. The tail stays inside the cell (html5lib ground truth).
    #[test]
    fn formatting_marker_blocked_ignored_in_cell() {
        let (dom, root) = parse("<b>x<table><td>x</b>y");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(shape.contains("<td>"), "cell exists: {shape}");
        let mut td_text = String::new();
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "td" {
                for &c in dom.children(n) {
                    if let NodeKind::Text = dom.node(c).kind {
                        td_text.push_str(&dom.node(c).text);
                    }
                }
            }
        });
        assert_eq!(
            td_text.split_whitespace().collect::<String>(),
            "xy",
            "</b> ignored: both texts stay inside the td"
        );
    }

    /// Nested formatting wrappers: only the innermost b is adopted; the i
    /// survives open around everything, and the tail stays inside the i.
    /// A1: breakout tags (spec "in foreign content") pop the whole foreign
    /// subtree and re-process at the HTML insertion point — the p/b land as
    /// body-level siblings of the empty `<svg>` (html5lib ground truth).
    #[test]
    fn foreign_breakout_pops_to_html() {
        let (dom, root) = parse("<svg><p>x");
        let (dom2, root2) = parse("<svg><b>y");
        for (d, r, tag) in [(&dom, root.unwrap(), "p"), (&dom2, root2.unwrap(), "b")] {
            let mut svg = None;
            d.walk(r, &mut |n| {
                if d.tag(n) == "svg" && svg.is_none() {
                    svg = Some(n);
                }
            });
            let svg = svg.expect("svg present");
            assert!(
                d.children(svg).is_empty(),
                "{tag} broke out; svg keeps no children"
            );
            let shape = tree_shape(d, r);
            assert!(
                shape.contains(&format!("<{tag}>[2]")),
                "{tag} at body level (sibling of svg): {shape}"
            );
        }
    }

    /// A1: integration points switch back to HTML rules — HTML elements
    /// nest inside them instead of breaking out (html5lib ground truth).
    #[test]
    fn integration_points_keep_html_inside() {
        let (dom, root) = parse("<math><mi><b>x</b></mi></math>");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<mi>[3] <b>[4]"),
            "b stays inside the MathML text integration point mi: {shape}"
        );
        let (dom2, root2) = parse("<svg><foreignObject><div>x</div></foreignObject></svg>");
        let shape2 = tree_shape(&dom2, root2.unwrap());
        assert!(
            shape2.contains("<foreignObject>[3] <div>[4]"),
            "div stays inside foreignObject (HTML integration point): {shape2}"
        );
    }

    /// A1: after a foreign element closes, the svg resumes accepting
    /// foreign content — a following sibling lands on the svg, not inside
    /// the closed integration point.
    #[test]
    fn foreign_end_tag_closes_and_svg_resumes() {
        let (dom, root) =
            parse("<svg><foreignObject><div>x</div></foreignObject><circle></circle></svg>");
        let mut svg = None;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "svg" && svg.is_none() {
                svg = Some(n);
            }
        });
        let svg = svg.expect("svg present");
        let kids: Vec<&str> = dom.children(svg).iter().map(|&c| dom.tag(c)).collect();
        assert_eq!(kids, vec!["foreignObject", "circle"], "both svg children");
    }

    /// A1: the tokenizer lowercases everything, but the SVG namespace is
    /// case-sensitive (resvg consumes these verbatim) — canonical camelCase
    /// is restored for foreign tags/attributes, and attribute lookup works
    /// under either spelling.
    #[test]
    fn svg_case_restored_for_foreign_elements() {
        let (dom, root) = parse(
            "<svg viewbox='0 0 10 10'><clippath/><foreignObject><div/></foreignObject></svg>",
        );
        let mut svg = None;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "svg" && svg.is_none() {
                svg = Some(n);
            }
        });
        let svg = svg.expect("svg present");
        assert_eq!(dom.element(svg).unwrap().attr("viewBox"), Some("0 0 10 10"));
        assert_eq!(dom.element(svg).unwrap().attr("viewbox"), Some("0 0 10 10"));
        let markup = dom.outer_html(svg);
        assert!(markup.contains("<foreignObject"), "{markup}");
        // clippath sits OUTSIDE the integration point → SVG rules apply;
        // div inside foreignObject stays HTML (lowercase).
        assert!(markup.contains("<clipPath/>"), "{markup}");
        assert!(markup.contains("<div/>"), "{markup}");
        assert!(markup.contains("viewBox=\"0 0 10 10\""), "{markup}");
    }

    /// A1: `<nobr>` closes an in-scope open nobr first (spec startTagNobr
    /// via the adoption agency) — consecutive nobrs are siblings.
    #[test]
    fn nobr_scope_close_siblings() {
        let (dom, root) = parse("<nobr>a<nobr>b");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<nobr>[2] \"a\" <nobr>[2] \"b\""),
            "second nobr is a sibling, not nested: {shape}"
        );
    }

    /// A1: `<button>` closes an in-scope open button (spec startTagButton) —
    /// even across a nested `<p>` (html5lib ground truth).
    #[test]
    fn button_scope_close_siblings() {
        let (dom, root) = parse("<button>1<p>2<button>3");
        let shape = tree_shape(&dom, root.unwrap());
        let mut buttons = 0;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "button" {
                buttons += 1;
            }
        });
        assert_eq!(buttons, 2, "two sibling buttons: {shape}");
        assert!(
            shape.contains("<button>[2] \"1\" <p>[3] \"2\" <button>[2] \"3\""),
            "p nests in the first button, second button is a sibling: {shape}"
        );
    }

    /// Nested formatting wrappers: only the innermost b is adopted; the i
    /// survives open around everything, and the tail stays inside the i.
    #[test]
    fn adoption_agency_innermost_only() {
        let (dom, root) = parse("<i><b>1<p>2</b>3</i>4");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<i>[2] <b>[3] \"1\" <p>[2] <i>[3] <b>[4] \"2\" \"3\" \"4\""),
            "p adopted out of b but held by i, tail inside i: {shape}"
        );
    }

    /// The AA outer loop re-runs after an explicit </p>: the p closes, the
    /// clone-b pops, and the tail "3" lands at body level.
    #[test]
    fn adoption_agency_p_explicitly_closed() {
        let (dom, root) = parse("<b>1<p>2</b></p>3");
        let shape = tree_shape(&dom, root.unwrap());
        assert!(
            shape.contains("<b>[2] \"1\" <p>[2] <b>[3] \"2\" \"3\""),
            "p closed after AA; tail at body level: {shape}"
        );
    }

    /// A1: pre/textarea swallow exactly one leading newline.
    #[test]
    fn pre_strips_one_leading_newline() {
        let (dom, root) = parse("<pre>\nline1\nline2</pre>");
        let pre = {
            let mut found = None;
            dom.walk(root.unwrap(), &mut |n| {
                if dom.tag(n) == "pre" && found.is_none() {
                    found = Some(n);
                }
            });
            found.unwrap()
        };
        let text: String = dom
            .children(pre)
            .iter()
            .map(|&c| dom.node(c).text.clone())
            .collect();
        assert_eq!(text, "line1\nline2", "one newline dropped, rest verbatim");

        let (dom, root) = parse("<pre>\n\nline1</pre>");
        let mut found = None;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "pre" && found.is_none() {
                found = Some(n);
            }
        });
        let text: String = dom
            .children(found.unwrap())
            .iter()
            .map(|&c| dom.node(c).text.clone())
            .collect();
        assert_eq!(text, "\nline1", "the second newline is content");
    }

    /// A1: `<image>` is an img alias; nested forms are dropped; consecutive
    /// buttons are siblings; select drops non-option content.
    #[test]
    fn legacy_aliases_and_drops() {
        let (dom, root) = parse("<image src='x.png'>");
        let mut tags = Vec::new();
        dom.walk(root.unwrap(), &mut |n| tags.push(dom.tag(n).to_string()));
        assert!(tags.contains(&"img".to_string()), "image → img: {tags:?}");

        let (dom, root) = parse("<form id='a'><form id='b'></form></form>");
        let mut forms = 0;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "form" {
                forms += 1;
            }
        });
        assert_eq!(forms, 1, "nested form dropped");

        let (dom, root) = parse("<button>a</button><button>b</button>");
        let shape = shape_of(&dom, root.unwrap());
        assert_eq!(
            shape.matches("<button>").count(),
            2,
            "buttons are siblings: {shape}"
        );

        let (dom, root) = parse("<select><option>a</option><div>stray</div></select>");
        let shape = shape_of(&dom, root.unwrap());
        assert!(
            shape.contains("<option>a") && !shape.contains("<div>"),
            "select drops non-option content: {shape}"
        );
    }

    #[test]
    fn sibling_li_closes_previous() {
        let (dom, root) = parse("<ul><li>one<li>two</ul>");
        let mut li_count = 0;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "li" {
                li_count += 1;
            }
        });
        assert_eq!(li_count, 2);
    }

    #[test]
    fn data_nul_is_ignored_by_the_tree_builder() {
        assert_eq!(
            p_children("<p>a\0b</p>"),
            vec![(NodeKind::Text, "ab".into())]
        );
        assert!(p_children("<p>\0</p>").is_empty());
    }

    #[test]
    fn foreign_data_nul_becomes_replacement_but_html_nul_is_ignored() {
        let (dom, root) = parse("<svg>\0a</svg>");
        let mut texts = Vec::new();
        dom.walk(root.unwrap(), &mut |n| {
            if dom.node(n).kind == NodeKind::Text {
                texts.push(dom.node(n).text.clone());
            }
        });
        assert_eq!(texts, vec!["�a"]);

        let (dom, root) = parse("<svg><foreignObject>\0a</foreignObject></svg>");
        let mut texts = Vec::new();
        dom.walk(root.unwrap(), &mut |n| {
            if dom.node(n).kind == NodeKind::Text {
                texts.push(dom.node(n).text.clone());
            }
        });
        assert_eq!(texts, vec!["a"]);
    }

    #[test]
    fn plaintext_keeps_remainder_literal_and_foreign_content_unaffected() {
        let (dom, root) = parse("<plaintext><b>x</b>&amp;\0");
        let mut plaintext = None;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "plaintext" && plaintext.is_none() {
                plaintext = Some(n);
            }
        });
        let plaintext = plaintext.expect("plaintext present");
        let text: String = dom
            .children(plaintext)
            .iter()
            .map(|&child| dom.node(child).text.clone())
            .collect();
        assert_eq!(text, "<b>x</b>&amp;�");

        let (dom, root) = parse("<svg><plaintext>a</plaintext>b");
        let mut svg = None;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "svg" && svg.is_none() {
                svg = Some(n);
            }
        });
        let svg = svg.expect("svg present");
        let kids: Vec<&str> = dom
            .children(svg)
            .iter()
            .map(|&child| dom.tag(child))
            .collect();
        assert_eq!(kids, vec!["plaintext", ""]);
        let plaintext_text: String = dom
            .children(dom.children(svg)[0])
            .iter()
            .map(|&child| dom.node(child).text.clone())
            .collect();
        assert_eq!(plaintext_text, "a");
        assert_eq!(dom.node(dom.children(svg)[1]).text, "b");

        let (dom, ctx) = parse_fragment("</plaintext>", "plaintext");
        assert_eq!(dom.node(dom.children(ctx)[0]).text, "</plaintext>");
    }

    #[test]
    fn text_is_collapsed_and_merged() {
        let kids = p_children("<p>  hello   world  </p>");
        assert_eq!(kids, vec![(NodeKind::Text, "hello world ".into())]);
    }

    #[test]
    fn entities_in_text() {
        let kids = p_children("<p>fish &amp; chips</p>");
        assert_eq!(kids, vec![(NodeKind::Text, "fish & chips".into())]);
    }

    #[test]
    fn full_entity_table_shape_and_spot_checks() {
        use crate::entities::NAMED_ENTITIES;
        // The complete WHATWG set, longest-first for the tokenizer scan.
        assert!(NAMED_ENTITIES.len() >= 2200, "got {}", NAMED_ENTITIES.len());
        assert!(
            NAMED_ENTITIES
                .windows(2)
                .all(|w| w[0].0.len() >= w[1].0.len()),
            "longest-first order"
        );
        assert!(
            NAMED_ENTITIES.iter().all(|(n, _)| n.len() <= 32),
            "tokenizer lookahead covers all"
        );
        let get = |name: &str| {
            NAMED_ENTITIES
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| *v)
        };
        assert_eq!(get("amp;"), Some("&"));
        assert_eq!(get("AMP"), Some("&"), "legacy no-semicolon form");
        assert_eq!(get("CounterClockwiseContourIntegral;"), Some("∳"));
        assert_eq!(
            get("NotEqualTilde;").map(|s| s.chars().count()),
            Some(2),
            "multi-char value"
        );
        assert_eq!(get("NewLine;"), Some("\n"));
        // End to end through the tree builder.
        let kids = p_children("<p>A&CounterClockwiseContourIntegral;B &AMP C</p>");
        assert_eq!(kids, vec![(NodeKind::Text, "A∳B & C".into())]);
    }

    #[test]
    fn implied_tbody_and_tr_wrap_cells() {
        assert_eq!(
            path_to("<table><tr><td>a</td></tr></table>", "td"),
            vec!["html", "body", "table", "tbody", "tr", "td"]
        );
        // A cell straight in the table implies tbody AND tr.
        assert_eq!(
            path_to("<table><td>b</td></table>", "td"),
            vec!["html", "body", "table", "tbody", "tr", "td"]
        );
        // A second row lands in the same tbody, not nested in the first row.
        let (dom, root) = parse("<table><tr><td>a</td><tr><td>b</td></table>");
        let mut trs = 0;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "tr" {
                trs += 1;
            }
        });
        assert_eq!(trs, 2);
    }

    #[test]
    fn stray_content_is_foster_parented_above_the_table() {
        let (dom, root) =
            parse("<body><table><div>stray</div><tr><td>cell</td></tr></table></body>");
        let mut body = None;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "body" {
                body = Some(n);
            }
        });
        let body = body.unwrap();
        let kids: Vec<&str> = dom.children(body).iter().map(|&c| dom.tag(c)).collect();
        // The stray div sits before the table, at its original position.
        assert_eq!(kids, vec!["div", "table"]);

        // The div kept its text; the cell is intact under implied wrappers.
        let div = dom.children(body)[0];
        let div_text = dom.children(div)[0];
        assert_eq!(dom.node(div_text).text, "stray");
        assert_eq!(
            path_to("<table><tr><td>cell</td></tr></table>", "td"),
            vec!["html", "body", "table", "tbody", "tr", "td"]
        );
    }

    #[test]
    fn stray_table_text_is_foster_parented() {
        // Foster parenting hoists the text BEFORE the table.
        let kids = body_children("<table>loose text</table>");
        assert_eq!(
            kids,
            vec![
                (NodeKind::Text, "loose text".into()),
                (NodeKind::Element, "table".into()),
            ]
        );
    }

    #[test]
    fn sibling_headings_close_each_other() {
        let kids = body_children("<h1>one<h2>two</h2>");
        assert_eq!(
            kids,
            vec![
                (NodeKind::Element, "h1".into()),
                (NodeKind::Element, "h2".into()),
            ]
        );
    }

    #[test]
    fn optgroups_close_open_options() {
        let (dom, root) = parse(
            "<select><optgroup label=\"a\"><option>1\
<optgroup label=\"b\"><option>2</select>",
        );
        let mut select = None;
        dom.walk(root.unwrap(), &mut |n| {
            if dom.tag(n) == "select" {
                select = Some(n);
            }
        });
        let select = select.unwrap();
        let groups: Vec<&str> = dom.children(select).iter().map(|&c| dom.tag(c)).collect();
        assert_eq!(groups, vec!["optgroup", "optgroup"]);
        for &g in dom.children(select) {
            assert_eq!(dom.children(g).len(), 1, "one option per group");
        }
    }

    #[test]
    fn fragments_parse_into_the_context_element() {
        let (dom, ctx) = parse_fragment("<b>hi</b> tail", "div");
        assert_eq!(dom.tag(ctx), "div");
        assert_eq!(dom.root(), Some(ctx), "context is the root");
        let kids: Vec<(NodeKind, String)> = dom
            .children(ctx)
            .iter()
            .map(|&c| {
                let n = dom.node(c);
                match n.kind {
                    NodeKind::Element => (n.kind, n.tag().to_string()),
                    NodeKind::Text => (n.kind, n.text.clone()),
                }
            })
            .collect();
        assert_eq!(
            kids,
            vec![
                (NodeKind::Element, "b".into()),
                (NodeKind::Text, " tail".into()),
            ]
        );

        // Fragment markup must not synthesize a document skeleton even for
        // html-looking input, and unclosed tags are fine.
        let (dom, ctx) = parse_fragment("<p><i>unclosed", "td");
        assert_eq!(dom.tag(ctx), "td");
        assert!(dom.get_element_by_id("x").is_none());
        let p = dom.children(ctx)[0];
        assert_eq!(dom.tag(p), "p");
    }

    /// Serialize a tree back to text-ish form for stream/batch comparison:
    /// tags plus trimmed text, in document order.
    fn shape_of(dom: &Dom, root: NodeId) -> String {
        let mut out = String::new();
        dom.walk(root, &mut |n| {
            let node = dom.node(n);
            match node.kind {
                NodeKind::Element => {
                    out.push('<');
                    out.push_str(node.tag());
                    out.push('>');
                }
                NodeKind::Text => {
                    out.push_str(node.text.trim());
                }
            }
        });
        out
    }

    /// A1: nesting-aware shape — each element opens with its depth in
    /// brackets, text collapses to word runs, so structure (not just tag
    /// order) is assertable. Example: `<html>[0]<body>[1]<b>[2]1`.
    fn tree_shape(dom: &Dom, root: NodeId) -> String {
        fn rec(dom: &Dom, n: NodeId, depth: usize, out: &mut String) {
            let node = dom.node(n);
            match node.kind {
                NodeKind::Element => {
                    out.push_str(&format!("<{}>[{}] ", node.tag(), depth));
                    for &c in dom.children(n) {
                        rec(dom, c, depth + 1, out);
                    }
                }
                NodeKind::Text => {
                    let t = node.text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if !t.is_empty() {
                        out.push_str(&format!("{t:?} "));
                    }
                }
            }
        }
        let mut out = String::new();
        rec(dom, root, 0, &mut out);
        out.trim_end().to_string()
    }

    #[test]
    fn streaming_matches_batch_at_every_chunking() {
        let html = "<!DOCTYPE html><html><head><title>t &amp; t</title>\
            <style>p > b { color: red }</style></head>\
            <body><h1 id=\"h\">Hi &mdash; there</h1>\
            <p class=\"a b\">one <b>two</b> <a href=\"/x?a=1&amp;b=2\">three</a></p>\
            <!-- note --><pre>  keep\tthis  </pre>\
            <br><img src=\"i.png\" alt=\"pic\"><!-- tail";
        let (dom, root) = parse(html);
        let want = shape_of(&dom, root.unwrap());
        // Whole, halves, awkward splits, and adversarial 1-char chunks.
        let splits: Vec<Vec<&str>> = vec![
            vec![html],
            vec![&html[..html.len() / 2], &html[html.len() / 2..]],
            vec![&html[..7], &html[7..30], &html[30..]],
        ];
        for chunks in &splits {
            let (dom, root) = parse_streaming(chunks);
            assert_eq!(shape_of(&dom, root.unwrap()), want, "chunks {chunks:?}");
        }
        let chars: Vec<&str> = html.split("").collect();
        let (dom, root) = parse_streaming(&chars);
        assert_eq!(shape_of(&dom, root.unwrap()), want, "1-char chunks");
    }

    #[test]
    fn feed_builds_progressively_and_finish_completes() {
        let mut tb = TreeBuilder::new();
        tb.feed("<html><body><p>hel");
        // Partial tree already has the paragraph text so far.
        let has_hel = {
            let mut found = false;
            if let Some(root) = tb.dom.root() {
                tb.dom.walk(root, &mut |n| {
                    if tb.dom.node(n).text.contains("hel") {
                        found = true;
                    }
                });
            }
            found
        };
        assert!(has_hel, "first paint sees partial text");
        tb.feed("lo &amp; bye</p></body></html>");
        let (dom, _) = tb.finish();
        let mut texts = Vec::new();
        dom.walk(dom.root().unwrap(), &mut |n| {
            if dom.node(n).kind == NodeKind::Text {
                texts.push(dom.node(n).text.clone());
            }
        });
        assert!(texts.iter().any(|t| t.contains("hello & bye")), "{texts:?}");
    }
}
