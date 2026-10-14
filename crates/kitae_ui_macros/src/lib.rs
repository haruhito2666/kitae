//! Proc macros for `kitae_ui`.
//!
//! `view!` turns Rust-syntax markup into a virtual tree constructor;
//! `#[component]` marks functions as components (identity marker; the
//! function stays exactly as written).
//!
//! Implemented directly on `proc_macro::TokenStream` — this crate has zero
//! dependencies.

use proc_macro::{Delimiter, Group, Ident, Punct, Spacing, Span, TokenStream, TokenTree};
use std::str::FromStr;

/// Emit a compile error at the macro call site instead of panicking inside
/// the proc macro (a panic abort the expansion and surfaces as an opaque
/// "proc macro panicked" without pointing at the user's markup).
fn compile_error_at(msg: &str) -> TokenStream {
    let mut out = Vec::new();
    push_path_abs(&mut out, &["core", "compile_error"]);
    out.push(TokenTree::Punct(Punct::new('!', Spacing::Alone)));
    let mut arg = Vec::new();
    // `{:?}` on a &str yields a fully-escaped Rust string literal, so the
    // message may itself contain `{:?}`-rendered tokens with newlines.
    arg.push(lit_token(&format!("{msg:?}")));
    out.push(TokenTree::Group(Group::new(
        Delimiter::Parenthesis,
        arg.into_iter().collect(),
    )));
    out.into_iter().collect()
}

/// Marks a function as a kitae_ui component.
///
/// Components are functions that return a `View`. This attribute injects
/// the scope bookkeeping that makes hooks position-stable: on every call
/// the body runs inside a component scope, so `use_signal`/`use_ref`
/// return the same state for the same invocation across re-renders
/// (rule of hooks: call them unconditionally, in the same order).
///
/// `#[component(skip_eq)]` (A6): the body is skipped entirely — the last
/// built view is reused — whenever the props compare `PartialEq`-equal to
/// the previous build at the same call site. React.memo semantics without
/// the explicit memo call: the 5,000-row list re-renders for free when
/// nothing in a row changed. Requirements and footguns:
///
/// - Every prop (the emitted tuple captures the param bindings) must be
///   `Clone + PartialEq`.
/// - The props tuple compares the PROP VALUES only. Internal state reads
///   (`use_signal`, context) do NOT re-trigger a rebuild — don't use
///   `skip_eq` on a component whose view depends on internal state that
///   changes independently of its props.
/// - A body that destructures its props (`let Row(ver, text) = row;`)
///   still works: the emitted tuple clones the bindings BEFORE the body
///   runs, and the body's own destructuring moves the locals, not the
///   captured tuple.
/// - Not for components taking `Children` or `Slots` — a cached view is
///   stale the moment the caller's braces change (they are not props).
#[proc_macro_attribute]
pub fn component(attr: TokenStream, item: TokenStream) -> TokenStream {
    // The only supported attribute payload is the bare flag `skip_eq`.
    let skip_eq = attr
        .clone()
        .into_iter()
        .any(|t| matches!(t, TokenTree::Ident(ref i) if i.to_string() == "skip_eq"));
    let tokens: Vec<TokenTree> = item.into_iter().collect();
    // Shape: fn NAME <generics?> (args) -> Ret [where …] { body }.
    // Split at the top-level brace group — everything before it is the
    // signature, kept verbatim.
    let body_pos = tokens
        .iter()
        .position(|t| matches!(t, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace));
    let Some(body_pos) = body_pos else {
        return compile_error_at(
            "#[component] expects a function with a body — this item has no `{ … }`",
        );
    };
    let sig: Vec<TokenTree> = tokens[..body_pos].to_vec();
    let body = match &tokens[body_pos] {
        TokenTree::Group(g) => g.clone(),
        _ => unreachable!(),
    };
    // fn NAME(..) -> View {
    //     kitae_ui::hooks::enter_scope(Some("NAME"));
    //     let __kitae_out = { ORIGINAL BODY };
    //     kitae_ui::hooks::exit_scope();
    //     __kitae_out
    // }
    // The name rides along for the DevTools scope tree (RP-C3).
    let fn_name = {
        let mut name = String::new();
        for (i, t) in sig.iter().enumerate() {
            if let TokenTree::Ident(id) = t {
                if id.to_string() == "fn" {
                    if let Some(TokenTree::Ident(n)) = sig.get(i + 1) {
                        name = n.to_string();
                    }
                    break;
                }
            }
        }
        name
    };
    let scope_path = [
        Ident::new("kitae_ui", Span::call_site()).into(),
        Punct::new(':', Spacing::Joint).into(),
        Punct::new(':', Spacing::Alone).into(),
        Ident::new("hooks", Span::call_site()).into(),
        Punct::new(':', Spacing::Joint).into(),
        Punct::new(':', Spacing::Alone).into(),
    ];
    let mut new_body: Vec<TokenTree> = Vec::new();
    new_body.push(Ident::new("let", Span::call_site()).into());
    new_body.push(Ident::new("__kitae_guard", Span::call_site()).into());
    new_body.push(Punct::new('=', Spacing::Alone).into());
    new_body.extend(scope_path.iter().cloned());
    new_body.push(Ident::new("enter_scope", Span::call_site()).into());
    let mut enter_args: Vec<TokenTree> = Vec::new();
    if !fn_name.is_empty() {
        enter_args.push(Ident::new("Some", Span::call_site()).into());
        enter_args.push(
            Group::new(
                Delimiter::Parenthesis,
                str_tokens(&fn_name).into_iter().collect(),
            )
            .into(),
        );
    }
    new_body.push(Group::new(Delimiter::Parenthesis, enter_args.into_iter().collect()).into());
    new_body.push(Punct::new(';', Spacing::Alone).into());
    if skip_eq {
        // A6 auto-skip: wrap the body in a props-comparing slot. The props
        // tuple captures each param binding by clone (so the body's own
        // moves/destructures don't consume the tuple); `with_skip_eq_slot`
        // returns the cached view and never runs the body on equal props.
        let props = prop_binding_idents(&sig);
        new_body.push(Ident::new("let", Span::call_site()).into());
        new_body.push(Ident::new("__kitae_out", Span::call_site()).into());
        new_body.push(Punct::new('=', Spacing::Alone).into());
        new_body.extend(fixed_toks("kitae_ui::hooks::with_skip_eq_slot"));
        let mut props_tup: Vec<TokenTree> = Vec::new();
        for (i, p) in props.iter().enumerate() {
            if i > 0 {
                props_tup.push(Punct::new(',', Spacing::Alone).into());
            }
            props_tup.push(Ident::new(p.as_str(), Span::call_site()).into());
            props_tup.push(Punct::new('.', Spacing::Alone).into());
            props_tup.push(Ident::new("clone", Span::call_site()).into());
            props_tup.push(Group::new(Delimiter::Parenthesis, TokenStream::new()).into());
        }
        if props.len() == 1 {
            props_tup.push(Punct::new(',', Spacing::Alone).into());
        }
        let mut call_args: Vec<TokenTree> = Vec::new();
        call_args.push(Group::new(Delimiter::Parenthesis, props_tup.into_iter().collect()).into());
        call_args.push(Punct::new(',', Spacing::Alone).into());
        let mut build_cl: Vec<TokenTree> = Vec::new();
        build_cl.push(Ident::new("move", Span::call_site()).into());
        build_cl.push(Punct::new('|', Spacing::Joint).into());
        build_cl.push(Punct::new('|', Spacing::Alone).into());
        build_cl.push(TokenTree::Group(Group::new(
            Delimiter::Brace,
            body.stream(),
        )));
        call_args.push(Group::new(Delimiter::Parenthesis, build_cl.into_iter().collect()).into());
        new_body.push(Group::new(Delimiter::Parenthesis, call_args.into_iter().collect()).into());
        new_body.push(Punct::new(';', Spacing::Alone).into());
    } else {
        new_body.push(Ident::new("let", Span::call_site()).into());
        new_body.push(Ident::new("__kitae_out", Span::call_site()).into());
        new_body.push(Punct::new('=', Spacing::Alone).into());
        new_body.push(TokenTree::Group(Group::new(
            Delimiter::Brace,
            body.stream(),
        )));
        new_body.push(Punct::new(';', Spacing::Alone).into());
    }
    new_body.push(Ident::new("__kitae_out", Span::call_site()).into());
    let mut out = sig;
    out.push(TokenTree::Group(Group::new(
        Delimiter::Brace,
        new_body.into_iter().collect(),
    )));
    out.into_iter().collect()
}

/// The binding identifiers of a fn signature's parameters — the A6 props
/// tuple. For `fn Row(row: Row, id: usize)` → `["row", "id"]`; for a tuple
/// pattern `fn Cell((a, b): (A, B))` → `["a", "b"]` (the pattern's own
/// bindings ARE the props). Skips `mut`/`ref` markers and `_`-prefixed
/// names; a constructor-style pattern (`Some(x)`, `Foo { x }`) contributes
/// its inner bindings, not the constructor name.
fn prop_binding_idents(sig: &[TokenTree]) -> Vec<String> {
    let mut out = Vec::new();
    // The param list is the first top-level paren group of the signature.
    let params = sig
        .iter()
        .find_map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => {
                Some(g.clone().stream().into_iter().collect::<Vec<_>>())
            }
            _ => None,
        })
        .unwrap_or_default();
    // Split on the top-level commas (groups arrive as single tokens, so
    // every bare comma here separates params).
    let mut param: Vec<TokenTree> = Vec::new();
    let mut flush = |param: &mut Vec<TokenTree>, out: &mut Vec<String>| {
        // The pattern is everything before the `:` type separator.
        let mut pat: Vec<TokenTree> = Vec::new();
        for t in param.iter() {
            if let TokenTree::Punct(pt) = t {
                if pt.as_char() == ':' {
                    break;
                }
            }
            pat.push(t.clone());
        }
        pattern_bindings(&pat, out);
    };
    for t in params {
        if let TokenTree::Punct(pt) = &t {
            if pt.as_char() == ',' {
                flush(&mut param, &mut out);
                param.clear();
                continue;
            }
        }
        param.push(t);
    }
    flush(&mut param, &mut out);
    out
}

/// Terminal binding idents of one destructuring pattern.
fn pattern_bindings(toks: &[TokenTree], out: &mut Vec<String>) {
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            TokenTree::Group(g) => {
                let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                pattern_bindings(&inner, out);
                i += 1;
            }
            TokenTree::Ident(id) => {
                let s = id.to_string();
                let followed_by_group = matches!(toks.get(i + 1), Some(TokenTree::Group(_)));
                if s != "mut" && s != "ref" && !s.starts_with('_') && !followed_by_group {
                    // A standalone ident binds; an ident heading a group
                    // (`Some(`, `Foo {`) names the constructor — skip it.
                    out.push(s);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
}

/// `kitae_ui::__rt::make_children(move || kitae_ui::view! { … })` — the
/// deferred builder for a component call's braces (the plain-children path
/// and each A3 named slot alike).
fn make_children_expr(body_toks: Vec<TokenTree>) -> Vec<TokenTree> {
    [
        TokenTree::Ident(Ident::new("kitae_ui", Span::call_site())),
        TokenTree::Punct(Punct::new(':', Spacing::Joint)),
        TokenTree::Punct(Punct::new(':', Spacing::Alone)),
        TokenTree::Ident(Ident::new("__rt", Span::call_site())),
        TokenTree::Punct(Punct::new(':', Spacing::Joint)),
        TokenTree::Punct(Punct::new(':', Spacing::Alone)),
        TokenTree::Ident(Ident::new("make_children", Span::call_site())),
        TokenTree::Group(Group::new(
            Delimiter::Parenthesis,
            [
                TokenTree::Ident(Ident::new("move", Span::call_site())),
                TokenTree::Punct(Punct::new('|', Spacing::Joint)),
                TokenTree::Punct(Punct::new('|', Spacing::Alone)),
                TokenTree::Ident(Ident::new("kitae_ui", Span::call_site())),
                TokenTree::Punct(Punct::new(':', Spacing::Joint)),
                TokenTree::Punct(Punct::new(':', Spacing::Alone)),
                TokenTree::Ident(Ident::new("view", Span::call_site())),
                TokenTree::Punct(Punct::new('!', Spacing::Alone)),
                TokenTree::Group(Group::new(
                    Delimiter::Brace,
                    body_toks.into_iter().collect(),
                )),
            ]
            .into_iter()
            .collect(),
        )),
    ]
    .into_iter()
    .collect()
}

/// The `name` value of a `slot(name: "x")` directive's argument list.
/// `None` when the paren isn't a name directive — then the `slot` tokens
/// stay ordinary markup (e.g. a Web-Components `<slot>` element).
fn slot_name(toks: Vec<TokenTree>) -> Option<String> {
    let mut i = 0;
    while i < toks.len() {
        if let TokenTree::Ident(id) = &toks[i] {
            if id.to_string() == "name" {
                if let (Some(TokenTree::Punct(pt)), Some(TokenTree::Literal(lit))) =
                    (toks.get(i + 1), toks.get(i + 2))
                {
                    if pt.as_char() == ':' {
                        let repr = lit.to_string();
                        if repr.starts_with('"') && repr.ends_with('"') && repr.len() >= 2 {
                            return Some(repr[1..repr.len() - 1].to_string());
                        }
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Builds a kitae_ui virtual tree.
///
/// ```rust,ignore
/// view! {
///     section(class: "card") {
///         h2 { "Count: {count}" }
///         button(on:click = move |_| count.update(|c| *c += 1)) { "+1" }
///         if count.get() > 10 {
///             p(class: "warn") { "That's a lot of clicking." }
///         }
///     }
/// }
/// ```
#[proc_macro]
pub fn view(input: TokenStream) -> TokenStream {
    let toks: Vec<TokenTree> = input.into_iter().collect();
    let mut p = P { toks, pos: 0 };
    let nodes = match parse_nodes(&mut p) {
        Ok(n) => n,
        Err(e) => return compile_error_at(&format!("view!: {e}")),
    };
    if p.pos < p.toks.len() {
        return compile_error_at(&format!(
            "view!: unexpected token after markup: {}",
            p.toks[p.pos]
        ));
    }
    let mut out = Vec::new();
    emit_root(&nodes, &mut out);
    out.into_iter().collect()
}

// ---------------------------------------------------------------- parser ---

struct P {
    toks: Vec<TokenTree>,
    pos: usize,
}

impl P {
    fn peek(&self) -> Option<&TokenTree> {
        self.toks.get(self.pos)
    }
    fn bump(&mut self) -> Option<TokenTree> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn eat_punct(&mut self, c: char) -> bool {
        if let Some(TokenTree::Punct(pt)) = self.peek() {
            if pt.as_char() == c {
                self.pos += 1;
                return true;
            }
        }
        false
    }
    fn eat_ident(&mut self, s: &str) -> bool {
        if let Some(TokenTree::Ident(i)) = self.peek() {
            if i.to_string() == s {
                self.pos += 1;
                return true;
            }
        }
        false
    }
}

enum Attr {
    /// `name: value` — a bare identifier goes through `AttrRef` (so a
    /// `Signal<String>` value doubles as a two-way binding); anything else
    /// renders via `Display`.
    Plain {
        name: String,
        expr: Vec<TokenTree>,
        bare_ident: bool,
    },
    /// `on:event = handler`
    On {
        event: String,
        handler: Vec<TokenTree>,
    },
    /// `key: expr`
    Key(Vec<TokenTree>),
    /// `node_ref: handle_expr`
    NodeRef(Vec<TokenTree>),
}

enum Node {
    /// Text with `{ident}` interpolation already split out.
    Text {
        repr: String,
        parts: Option<(String, Vec<String>)>,
    },
    Elem {
        tag: String,
        attrs: Vec<Attr>,
        key: Option<Vec<TokenTree>>,
        node_ref: Option<Vec<TokenTree>>,
        children: Vec<Node>,
    },
    If {
        branches: Vec<(Vec<TokenTree>, Vec<Node>)>,
        else_body: Option<Vec<Node>>,
    },
    For {
        iter: Vec<TokenTree>,
        pat: Vec<TokenTree>,
        body: Vec<Node>,
    },
    Block(Vec<TokenTree>),
}

fn parse_nodes(p: &mut P) -> Result<Vec<Node>, String> {
    let mut out = Vec::new();
    while p.pos < p.toks.len() {
        match p.toks[p.pos].clone() {
            TokenTree::Literal(l) => {
                p.pos += 1;
                out.push(parse_text_lit(&l.to_string())?);
            }
            TokenTree::Ident(i) => {
                let name = i.to_string();
                p.pos += 1;
                if name == "if" {
                    out.push(parse_if(p)?);
                    continue;
                }
                if name == "for" {
                    out.push(parse_for(p)?);
                    continue;
                }
                // Component call: a Capitalized ident followed by an
                // argument list (`CounterDemo()`, `Greeting(name: "Kitae")`).
                // HTML tags are lowercase; an uppercase head means "this is
                // Rust, evaluate me as a child expression".
                //
                // With braces after the args — `Card() { … }` — the brace
                // content becomes the component's `Children` (last arg).
                if name.chars().next().is_some_and(char::is_uppercase)
                    && matches!(
                        p.peek(),
                        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis
                    )
                {
                    let mut call: Vec<TokenTree> = vec![TokenTree::Ident(i.clone())];
                    if let Some(TokenTree::Group(g)) = p.peek().cloned() {
                        p.pos += 1;
                        // Braced children? `Name(args) { kids }` → pass the
                        // kids as a trailing `Children` built by view!.
                        let children = match p.peek().cloned() {
                            Some(TokenTree::Group(kids))
                                if kids.delimiter() == Delimiter::Brace =>
                            {
                                p.pos += 1;
                                Some(kids)
                            }
                            _ => None,
                        };
                        if let Some(kids) = children {
                            // A3 named slots: scan the braces' top level for
                            // `slot(name: "x") { … }` directives. Any found →
                            // the call passes `Slots::new(vec![…])` (leftover
                            // tokens become the implicit "children" slot);
                            // none → the plain `Children` path as before.
                            let kid_toks: Vec<TokenTree> = kids.stream().into_iter().collect();
                            let mut slots: Vec<(String, Vec<TokenTree>)> = Vec::new();
                            let mut leftover: Vec<TokenTree> = Vec::new();
                            let mut i = 0;
                            while i < kid_toks.len() {
                                let mut matched = None;
                                if let TokenTree::Ident(id) = &kid_toks[i] {
                                    if id.to_string() == "slot" {
                                        if let (
                                            Some(TokenTree::Group(args)),
                                            Some(TokenTree::Group(body)),
                                        ) = (kid_toks.get(i + 1), kid_toks.get(i + 2))
                                        {
                                            if args.delimiter() == Delimiter::Parenthesis
                                                && body.delimiter() == Delimiter::Brace
                                            {
                                                if let Some(name) =
                                                    slot_name(args.stream().into_iter().collect())
                                                {
                                                    matched = Some((
                                                        name,
                                                        body.stream().into_iter().collect(),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                }
                                match matched {
                                    Some((name, body)) => {
                                        slots.push((name, body));
                                        i += 3;
                                    }
                                    None => {
                                        leftover.push(kid_toks[i].clone());
                                        i += 1;
                                    }
                                }
                            }
                            // Rebuild the arg list, appending the children/
                            // slots argument (comma only when the component
                            // already takes args).
                            let mut inner: Vec<TokenTree> = g.stream().into_iter().collect();
                            if !inner.is_empty() {
                                inner.push(Punct::new(',', Spacing::Alone).into());
                            }
                            if !slots.is_empty() {
                                if !leftover.is_empty() {
                                    slots.push(("children".to_string(), leftover));
                                }
                                let mut entries: Vec<TokenTree> = Vec::new();
                                for (name, body) in &slots {
                                    if !entries.is_empty() {
                                        entries.push(Punct::new(',', Spacing::Alone).into());
                                    }
                                    let mut pair = str_tokens(name);
                                    pair.push(punct('.'));
                                    pair.push(ident("to_string"));
                                    pair.push(group(Delimiter::Parenthesis, Vec::new()));
                                    pair.push(punct(','));
                                    pair.extend(make_children_expr(body.clone()));
                                    entries.push(group(Delimiter::Parenthesis, pair));
                                }
                                let mut vec_expr = Vec::new();
                                push_path_abs(&mut vec_expr, &["std", "vec"]);
                                vec_expr.push(TokenTree::Punct(Punct::new('!', Spacing::Alone)));
                                vec_expr.push(group(Delimiter::Bracket, entries));
                                let mut slots_call = Vec::new();
                                push_path_abs(
                                    &mut slots_call,
                                    &["kitae_ui", "vnode", "Slots", "new"],
                                );
                                slots_call.push(group(Delimiter::Parenthesis, vec_expr));
                                inner.extend(slots_call);
                            } else {
                                inner.extend(make_children_expr(kid_toks));
                            }
                            call.push(TokenTree::Group(Group::new(
                                Delimiter::Parenthesis,
                                inner.into_iter().collect(),
                            )));
                        } else {
                            call.push(TokenTree::Group(g));
                        }
                    }
                    out.push(Node::Block(call));
                    continue;
                }
                let mut tag = name;
                let mut attrs = Vec::new();
                if tag == "checkbox" {
                    tag = "input".to_string();
                    attrs.push(Attr::Plain {
                        name: "type".into(),
                        expr: str_tokens("checkbox"),
                        bare_ident: false,
                    });
                }
                if let Some(TokenTree::Group(g)) = p.peek() {
                    if g.delimiter() == Delimiter::Parenthesis {
                        let g = g.clone();
                        p.pos += 1;
                        attrs.extend(parse_attrs(g.stream().into_iter().collect())?);
                    }
                }
                let children = brace_group_nodes_opt(p)?.unwrap_or_default();
                let key = extract_key(&mut attrs);
                let node_ref = extract_node_ref(&mut attrs);
                out.push(Node::Elem {
                    tag,
                    attrs,
                    key,
                    node_ref,
                    children,
                });
            }
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
                p.pos += 1;
                out.push(Node::Block(g.stream().into_iter().collect()));
            }
            other => return Err(format!("unexpected token {other:?}")),
        }
    }
    Ok(out)
}

fn brace_group_nodes_opt(p: &mut P) -> Result<Option<Vec<Node>>, String> {
    match p.peek() {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => {
            let g = g.clone();
            p.pos += 1;
            let mut sub = P {
                toks: g.stream().into_iter().collect(),
                pos: 0,
            };
            Ok(Some(parse_nodes(&mut sub)?))
        }
        _ => Ok(None),
    }
}

fn brace_group_nodes(p: &mut P) -> Result<Vec<Node>, String> {
    match brace_group_nodes_opt(p)? {
        Some(n) => Ok(n),
        None => Err(format!("expected '{{' block, found {:?}", p.peek())),
    }
}

fn take_till_brace(p: &mut P) -> Result<Vec<TokenTree>, String> {
    let mut out = Vec::new();
    loop {
        match p.peek() {
            None => return Err("expected '{'".into()),
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => return Ok(out),
            Some(_) => out.push(p.bump().unwrap()),
        }
    }
}

fn parse_if(p: &mut P) -> Result<Node, String> {
    let mut branches = Vec::new();
    let mut else_body = None;
    let cond = take_till_brace(p)?;
    if cond.is_empty() {
        return Err("empty if condition".into());
    }
    let body = brace_group_nodes(p)?;
    branches.push((cond, body));
    loop {
        if !p.eat_ident("else") {
            break;
        }
        if p.eat_ident("if") {
            let cond = take_till_brace(p)?;
            if cond.is_empty() {
                return Err("empty else-if condition".into());
            }
            let body = brace_group_nodes(p)?;
            branches.push((cond, body));
            continue;
        }
        match brace_group_nodes_opt(p)? {
            Some(body) => {
                else_body = Some(body);
                break;
            }
            None => return Err("expected '{' after 'else'".into()),
        }
    }
    Ok(Node::If {
        branches,
        else_body,
    })
}

fn parse_for(p: &mut P) -> Result<Node, String> {
    let mut iter = Vec::new();
    loop {
        match p.peek() {
            None => return Err("expected '->' in for".into()),
            Some(TokenTree::Punct(pt)) if pt.as_char() == '-' => {
                let pt = pt.clone();
                p.pos += 1;
                if p.eat_punct('>') {
                    break;
                }
                iter.push(TokenTree::Punct(pt));
            }
            Some(_) => iter.push(p.bump().unwrap()),
        }
    }
    if iter.is_empty() {
        return Err("empty for iterator".into());
    }
    let mut pat = Vec::new();
    loop {
        match p.peek() {
            None => return Err("expected pattern before '{{' in for".into()),
            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => break,
            Some(_) => pat.push(p.bump().unwrap()),
        }
    }
    if pat.is_empty() {
        return Err("missing loop pattern in for".into());
    }
    let body = brace_group_nodes(p)?;
    Ok(Node::For { iter, pat, body })
}

fn parse_attrs(toks: Vec<TokenTree>) -> Result<Vec<Attr>, String> {
    let mut p = P { toks, pos: 0 };
    let mut out = Vec::new();
    while p.pos < p.toks.len() {
        if p.eat_punct(',') {
            continue;
        }
        // on:event = handler
        if ident_is(p.toks.get(p.pos), "on") && punct_is(p.toks.get(p.pos + 1), ':') {
            p.pos += 2;
            let event = match p.bump() {
                Some(TokenTree::Ident(i)) => i.to_string(),
                _ => return Err("expected event name after 'on:'".into()),
            };
            if !p.eat_punct('=') {
                return Err(format!("expected '=' after on:{event}"));
            }
            let handler = take_till_comma(&mut p);
            if handler.is_empty() {
                return Err("empty event handler".into());
            }
            out.push(Attr::On { event, handler });
            continue;
        }
        // key: expr
        if ident_is(p.toks.get(p.pos), "key") && punct_is(p.toks.get(p.pos + 1), ':') {
            p.pos += 2;
            let e = take_till_comma(&mut p);
            if e.is_empty() {
                return Err("empty key".into());
            }
            out.push(Attr::Key(e));
            continue;
        }
        // node_ref: handle — imperative element access (focus/scroll/rect)
        if ident_is(p.toks.get(p.pos), "node_ref") && punct_is(p.toks.get(p.pos + 1), ':') {
            p.pos += 2;
            let e = take_till_comma(&mut p);
            if e.is_empty() {
                return Err("empty node_ref".into());
            }
            out.push(Attr::NodeRef(e));
            continue;
        }
        // name: value — name may be hyphenated (aria-label, data-x)
        let mut name = String::new();
        loop {
            match p.peek() {
                None => return Err(format!("attribute {name:?} missing ':'")),
                Some(TokenTree::Punct(pt)) if pt.as_char() == ':' => {
                    p.pos += 1;
                    break;
                }
                Some(TokenTree::Punct(pt)) if pt.as_char() == ',' || pt.as_char() == '=' => {
                    return Err(format!("bad attribute name near {name:?}"));
                }
                Some(TokenTree::Ident(i)) => {
                    name.push_str(&i.to_string());
                    p.pos += 1;
                }
                Some(TokenTree::Punct(pt)) => {
                    name.push(pt.as_char());
                    p.pos += 1;
                }
                Some(other) => return Err(format!("bad attribute name token {other:?}")),
            }
        }
        if name.is_empty() {
            return Err("empty attribute name".into());
        }
        let expr = take_till_comma(&mut p);
        if expr.is_empty() {
            return Err(format!("attribute {name:?} has no value"));
        }
        // Two-way binding candidates: a bare identifier (`value: count`) or a
        // single field path (`value: self.draft`). Both go through attr_ref,
        // whose AttrRef impl turns Signal<String> into a two-way binding and
        // falls back to plain Display for everything else. Deeper paths
        // (`a.b.c`) and method calls keep the Display path — the binding
        // write-back would have nowhere to land.
        let bare_ident = matches!(expr.as_slice(), [TokenTree::Ident(_)])
            || matches!(expr.as_slice(),
                [TokenTree::Ident(_), TokenTree::Punct(p), TokenTree::Ident(_)]
                if p.as_char() == '.');
        out.push(Attr::Plain {
            name,
            expr,
            bare_ident,
        });
    }
    Ok(out)
}

fn ident_is(t: Option<&TokenTree>, s: &str) -> bool {
    matches!(t, Some(TokenTree::Ident(i)) if i.to_string() == s)
}

fn punct_is(t: Option<&TokenTree>, c: char) -> bool {
    matches!(t, Some(TokenTree::Punct(pt)) if pt.as_char() == c)
}

fn take_till_comma(p: &mut P) -> Vec<TokenTree> {
    let mut out = Vec::new();
    while let Some(t) = p.peek() {
        if let TokenTree::Punct(pt) = t {
            if pt.as_char() == ',' {
                break;
            }
        }
        out.push(p.bump().unwrap());
    }
    out
}

fn extract_key(attrs: &mut Vec<Attr>) -> Option<Vec<TokenTree>> {
    let mut key = None;
    attrs.retain(|a| match a {
        Attr::Key(e) => {
            key = Some(e.clone());
            false
        }
        _ => true,
    });
    key
}

fn extract_node_ref(attrs: &mut Vec<Attr>) -> Option<Vec<TokenTree>> {
    let mut handle = None;
    attrs.retain(|a| match a {
        Attr::NodeRef(e) => {
            handle = Some(e.clone());
            false
        }
        _ => true,
    });
    handle
}

fn parse_text_lit(repr: &str) -> Result<Node, String> {
    if !(repr.starts_with('"') && repr.ends_with('"') && repr.len() >= 2) {
        // raw strings, byte strings, etc. — pass through untouched
        return Ok(Node::Text {
            repr: repr.to_string(),
            parts: None,
        });
    }
    let content = &repr[1..repr.len() - 1];
    let chars: Vec<char> = content.chars().collect();
    let mut fmt = String::new();
    let mut args: Vec<String> = Vec::new();
    let mut i = 0;
    let mut interp = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' {
            if i + 1 < chars.len() && chars[i + 1] == '{' {
                fmt.push_str("{{");
                i += 2;
                continue;
            }
            let mut j = i + 1;
            let mut inner = String::new();
            let mut closed = false;
            while j < chars.len() {
                if chars[j] == '}' {
                    closed = true;
                    break;
                }
                inner.push(chars[j]);
                j += 1;
            }
            if !closed {
                return Err("unclosed '{' in text".into());
            }
            let trimmed: String = inner.chars().filter(|c| !c.is_whitespace()).collect();
            if trimmed.is_empty() {
                return Err("empty interpolation in text".into());
            }
            args.push(trimmed);
            fmt.push_str("{}");
            i = j + 1;
            interp = true;
        } else if c == '}' {
            if i + 1 < chars.len() && chars[i + 1] == '}' {
                fmt.push_str("}}");
                i += 2;
                continue;
            }
            return Err("unmatched '}' in text".into());
        } else {
            fmt.push(c);
            i += 1;
        }
    }
    if interp {
        Ok(Node::Text {
            repr: String::new(),
            parts: Some((fmt, args)),
        })
    } else {
        Ok(Node::Text {
            repr: repr.to_string(),
            parts: None,
        })
    }
}

// ----------------------------------------------------------------- emit ----

fn ident(s: &str) -> TokenTree {
    TokenTree::Ident(Ident::new(s, Span::call_site()))
}

fn punct(c: char) -> TokenTree {
    TokenTree::Punct(Punct::new(c, Spacing::Alone))
}

fn group(d: Delimiter, toks: Vec<TokenTree>) -> TokenTree {
    TokenTree::Group(Group::new(d, toks.into_iter().collect()))
}

fn push_path(out: &mut Vec<TokenTree>, segs: &[&str]) {
    for (i, s) in segs.iter().enumerate() {
        if i > 0 {
            out.push(TokenTree::Punct(Punct::new(':', Spacing::Joint)));
            out.push(TokenTree::Punct(Punct::new(':', Spacing::Alone)));
        }
        out.push(ident(s));
    }
}

fn push_path_abs(out: &mut Vec<TokenTree>, segs: &[&str]) {
    out.push(TokenTree::Punct(Punct::new(':', Spacing::Joint)));
    out.push(TokenTree::Punct(Punct::new(':', Spacing::Alone)));
    push_path(out, segs);
}

fn lit_token(src: &str) -> TokenTree {
    TokenStream::from_str(src)
        .expect("view!: internal literal error")
        .into_iter()
        .next()
        .expect("view!: internal literal error")
}

fn str_tokens(s: &str) -> Vec<TokenTree> {
    vec![lit_token(&format!("\"{s}\""))]
}

fn push_idents(out: &mut Vec<TokenTree>, words: &[&str]) {
    for w in words {
        if *w == "." {
            out.push(punct('.'));
        } else {
            out.push(ident(w));
        }
    }
}

/// `{`, `}`, `<`, `>` are not legal `Punct` characters — tokenize them.
fn brace_group(toks: Vec<TokenTree>) -> TokenTree {
    group(Delimiter::Brace, toks)
}

/// Parse a fixed token string (`<`/`>` generics cannot be built as `Punct`s).
fn fixed_toks(src: &str) -> Vec<TokenTree> {
    TokenStream::from_str(src)
        .expect("view!: internal token error")
        .into_iter()
        .collect()
}

fn emit_kids_block(nodes: &[Node], out: &mut Vec<TokenTree>) {
    let mut body: Vec<TokenTree> =
        fixed_toks("let mut __kids: ::std::vec::Vec<::kitae_ui::VNode> = ::std::vec::Vec::new();");
    for n in nodes {
        match n {
            Node::For { .. } | Node::Block(_) => {
                push_idents(&mut body, &["__kids", ".", "extend"]);
                let mut arg = Vec::new();
                emit_node(n, &mut arg);
                body.push(group(Delimiter::Parenthesis, arg));
            }
            _ => {
                push_idents(&mut body, &["__kids", ".", "push"]);
                let mut arg = Vec::new();
                emit_node(n, &mut arg);
                body.push(group(Delimiter::Parenthesis, arg));
            }
        }
        body.push(punct(';'));
    }
    // Tail expression (no semicolon): the block evaluates to the child list.
    push_idents(&mut body, &["__kids"]);
    out.push(brace_group(body));
}

fn push_vec_macro(out: &mut Vec<TokenTree>, nodes: &[Node]) {
    push_path_abs(out, &["std", "vec"]);
    out.push(TokenTree::Punct(Punct::new('!', Spacing::Alone)));
    let mut items = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        if i > 0 {
            items.push(punct(','));
        }
        emit_node(n, &mut items);
    }
    out.push(group(Delimiter::Bracket, items));
}

fn push_rt_call(out: &mut Vec<TokenTree>, func: &str, args: Vec<TokenTree>) {
    push_path_abs(out, &["kitae_ui", "__rt", func]);
    out.push(group(Delimiter::Parenthesis, args));
}

fn emit_root(nodes: &[Node], out: &mut Vec<TokenTree>) {
    // Always a `View`: built from the flat child accumulator.
    push_rt_call(out, "view_from", {
        let mut arg = Vec::new();
        emit_kids_block(nodes, &mut arg);
        arg
    });
}

fn emit_node(n: &Node, out: &mut Vec<TokenTree>) {
    match n {
        Node::Text { repr, parts } => {
            let mut arg = Vec::new();
            match parts {
                None => arg.push(lit_token(repr)),
                Some((fmt, args)) => {
                    push_path_abs(&mut arg, &["std", "format"]);
                    arg.push(TokenTree::Punct(Punct::new('!', Spacing::Alone)));
                    let mut call = Vec::new();
                    call.push(lit_token(&format!("\"{fmt}\"")));
                    for a in args {
                        call.push(punct(','));
                        let ts =
                            TokenStream::from_str(a).expect("view!: bad interpolation expression");
                        call.extend(ts);
                    }
                    arg.push(group(Delimiter::Parenthesis, call));
                }
            }
            push_rt_call(out, "text", arg);
        }
        Node::Elem {
            tag,
            attrs,
            key,
            node_ref,
            children,
        } => {
            push_rt_call(out, "elem", str_tokens(tag));
            if let Some(h) = node_ref {
                out.push(punct('.'));
                out.push(ident("node_ref"));
                out.push(group(Delimiter::Parenthesis, h.clone()));
            }
            if let Some(k) = key {
                out.push(punct('.'));
                out.push(ident("key"));
                let mut karg = vec![punct('&')];
                karg.extend(k.clone());
                let mut args = Vec::new();
                push_path_abs(&mut args, &["kitae_ui", "__rt", "key_val"]);
                args.push(group(Delimiter::Parenthesis, karg));
                out.push(group(Delimiter::Parenthesis, args));
            }
            for a in attrs {
                match a {
                    Attr::Key(_) => unreachable!("key extracted before emit"),
                    Attr::NodeRef(_) => unreachable!("node_ref extracted before emit"),
                    Attr::Plain {
                        name,
                        expr,
                        bare_ident,
                    } => {
                        out.push(punct('.'));
                        if *bare_ident {
                            // Bare identifier → AttrRef: renders now and, for
                            // Signal<String>, records a two-way binding.
                            out.push(ident("attr_source"));
                            let mut args = str_tokens(name);
                            args.push(punct(','));
                            push_path_abs(&mut args, &["kitae_ui", "__rt", "attr_ref"]);
                            let mut call = vec![punct('&')];
                            call.extend(expr.clone());
                            args.push(group(Delimiter::Parenthesis, call));
                            out.push(group(Delimiter::Parenthesis, args));
                        } else if matches!(expr.as_slice(), [TokenTree::Literal(l)]
                            if l.to_string().starts_with('"'))
                            && expr.len() == 1
                        {
                            // Static string literal.
                            out.push(ident("attr"));
                            let mut args = str_tokens(name);
                            args.push(punct(','));
                            args.extend(expr.clone());
                            out.push(group(Delimiter::Parenthesis, args));
                        } else {
                            // Dynamic expression → Display.
                            out.push(ident("attr"));
                            let mut args = str_tokens(name);
                            args.push(punct(','));
                            push_path_abs(&mut args, &["kitae_ui", "__rt", "attr_val"]);
                            let mut inner = vec![punct('&')];
                            inner.extend(expr.clone());
                            args.push(group(Delimiter::Parenthesis, inner));
                            out.push(group(Delimiter::Parenthesis, args));
                        }
                    }
                    Attr::On { event, handler } => {
                        out.push(punct('.'));
                        out.push(ident("on"));
                        let mut args = str_tokens(event);
                        args.push(punct(','));
                        push_path_abs(&mut args, &["kitae_ui", "__rt", "box_handler"]);
                        args.push(group(Delimiter::Parenthesis, handler.clone()));
                        out.push(group(Delimiter::Parenthesis, args));
                    }
                }
            }
            out.push(punct('.'));
            out.push(ident("children"));
            let mut arg = Vec::new();
            emit_kids_block(children, &mut arg);
            out.push(group(Delimiter::Parenthesis, arg));
            out.push(punct('.'));
            out.push(ident("build"));
            out.push(group(Delimiter::Parenthesis, Vec::new()));
        }
        Node::If {
            branches,
            else_body,
        } => {
            // if COND { VNode } else if .. else { VNode } — each branch
            // yields one VNode (fragment element when several).
            for (idx, (cond, body)) in branches.iter().enumerate() {
                if idx == 0 {
                    out.push(ident("if"));
                } else {
                    out.push(ident("else"));
                    out.push(ident("if"));
                }
                out.extend(cond.clone());
                let mut body_call = Vec::new();
                emit_branch_nodes(body, &mut body_call);
                out.push(group(Delimiter::Brace, body_call));
            }
            match else_body {
                Some(body) => {
                    out.push(ident("else"));
                    let mut body_call = Vec::new();
                    emit_branch_nodes(body, &mut body_call);
                    out.push(group(Delimiter::Brace, body_call));
                }
                None => {
                    out.push(ident("else"));
                    let mut none_call = Vec::new();
                    push_path_abs(&mut none_call, &["kitae_ui", "__rt", "none"]);
                    none_call.push(group(Delimiter::Parenthesis, Vec::new()));
                    out.push(group(Delimiter::Brace, none_call));
                }
            }
        }
        Node::For { iter, pat, body } => {
            // (ITER).flat_map(|PAT| vec![BODY...]).collect::<Vec<VNode>>()
            let mut arg = Vec::new();
            arg.push(group(Delimiter::Parenthesis, iter.clone()));
            arg.push(punct('.'));
            arg.push(ident("flat_map"));
            let mut cl = vec![punct('|')];
            cl.extend(pat.clone());
            cl.push(punct('|'));
            push_vec_macro(&mut cl, body);
            arg.push(group(Delimiter::Parenthesis, cl));
            arg.extend(fixed_toks(
                ".collect::<::std::vec::Vec<::kitae_ui::VNode>>()",
            ));
            push_rt_call(out, "for_nodes", arg);
        }
        Node::Block(expr) => {
            push_rt_call(out, "view_nodes", expr.clone());
        }
    }
}

/// One if/else branch: single node → itself; several → fragment element.
fn emit_branch_nodes(body: &[Node], out: &mut Vec<TokenTree>) {
    if body.len() == 1 {
        emit_node(&body[0], out);
    } else {
        push_rt_call(out, "frag", {
            let mut arg = Vec::new();
            emit_kids_block(body, &mut arg);
            arg
        });
    }
}
