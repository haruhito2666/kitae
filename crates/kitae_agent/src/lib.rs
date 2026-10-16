//! The agent plane's vocabulary — Track B's tier B1.
//!
//! One reviewed row per verb: the arguments are validated here, and the
//! permission gate is consulted before anything runs. The *meaning* of a
//! verb belongs to the host that implements [`AgentApi`] — in this
//! workspace `kitae_core::agent::AgentSession` drives a real `Browser`;
//! test hosts are recording stubs that assert the surface itself.
//!
//! Nothing here is macOS-, QuickJS- or feature-gated: a headless server
//! answers `click` without linking the script engine, because the shape of
//! an act is not a scripting concern. `kitae_script` installs this same
//! vocabulary as page-facing JS when the `page-js` feature is on.
//!
//! Permission model: every verb consults [`AgentApi::permit`] **before**
//! [`AgentApi::act`], so a denied verb cannot half-run. The default is
//! `Deny`: a host that implements `act` but forgets the policy refuses
//! everything rather than silently allowing it. Headless hosts decide
//! statically (a declared allow-list); the app answers with a prompt.
//! Scripts may declare the verbs they intend to use — a declaration is
//! input to the policy, never a grant.

use kitae_json::Value;

/// Where a verb acts: a CSS selector the engine resolves, or page
/// coordinates (the same hit-testing a real click uses).
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Selector(String),
    Point { x: f32, y: f32 },
}

impl Target {
    pub fn from_json(value: &Value) -> Result<Target, String> {
        if let Some(sel) = value.as_str() {
            if sel.is_empty() {
                return Err("target selector is empty".into());
            }
            return Ok(Target::Selector(sel.to_string()));
        }
        if let Some(x) = value.get("x").and_then(Value::as_f64) {
            let y = value
                .get("y")
                .and_then(Value::as_f64)
                .ok_or("target point needs both x and y")?;
            return Ok(Target::Point {
                x: x as f32,
                y: y as f32,
            });
        }
        Err("target must be a selector string or {x, y}".into())
    }

    pub fn describe(&self) -> String {
        match self {
            Target::Selector(s) => s.clone(),
            Target::Point { x, y } => format!("({x:.0}, {y:.0})"),
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Target::Selector(s) => Value::s(s.clone()),
            Target::Point { x, y } => Value::obj(vec![
                ("x", Value::Num(*x as f64)),
                ("y", Value::Num(*y as f64)),
            ]),
        }
    }
}

/// One act the plane understands.
#[derive(Debug, Clone, PartialEq)]
pub enum Verb {
    /// Load a URL and let the page settle (scripts, fetches, timers).
    Navigate { url: String },
    /// Click an element (its center) or a page point.
    Click { target: Target },
    /// Set a field's value through the engine's own input path (focus,
    /// value write, `input`/`change` events as the engine fires them).
    Fill { target: Target, text: String },
    /// Submit a form: by id/tag selector, or the form owning the focused
    /// control when `form` is `None`.
    Submit { form: Option<String> },
    /// One step back in history, settling the previous document.
    Back,
    /// The page as plain text (the reading path).
    Read,
    /// Every link the layout painted: text, href, painted rect.
    Links,
    /// In-page search: matches with their painted rects.
    Find { query: String },
    /// The structured page-as-data document (tier B5).
    Extract,
    /// Wait until `selector` matches (optionally carrying `text`), up to
    /// `timeout_ms`; reports whether it matched and how long it took.
    Watch { selector: String, timeout_ms: u64 },
    /// Dispatch a DOM event at a target (`click`, `input`, a custom type).
    Dispatch {
        target: Target,
        event: String,
        detail: Option<String>,
    },
    /// PNG of the painted page (device pixels).
    Screenshot { path: Option<String> },
    /// Print-styled PDF of the page.
    Pdf { path: Option<String> },
    /// Archive the page as signed QTSQ evidence (tier B6).
    Archive { dir: Option<String> },
    /// Verify a QTSQ evidence file's signature.
    Verify { path: String },
}

impl Verb {
    /// The verb's stable name (the JS method, the MCP tool, the audit row).
    pub fn name(&self) -> &'static str {
        match self {
            Verb::Navigate { .. } => "navigate",
            Verb::Click { .. } => "click",
            Verb::Fill { .. } => "fill",
            Verb::Submit { .. } => "submit",
            Verb::Back => "back",
            Verb::Read => "read",
            Verb::Links => "links",
            Verb::Find { .. } => "find",
            Verb::Extract => "extract",
            Verb::Watch { .. } => "watch",
            Verb::Dispatch { .. } => "dispatch",
            Verb::Screenshot { .. } => "screenshot",
            Verb::Pdf { .. } => "pdf",
            Verb::Archive { .. } => "archive",
            Verb::Verify { .. } => "verify",
        }
    }

    /// One line a permission prompt can show: what this call would do.
    pub fn describe(&self) -> String {
        match self {
            Verb::Navigate { url } => format!("navigate to {url}"),
            Verb::Click { target } => format!("click {}", target.describe()),
            Verb::Fill { target, text } => {
                format!("fill {} with {:?}", target.describe(), truncate(text, 40))
            }
            Verb::Submit { form } => match form {
                Some(f) => format!("submit {f}"),
                None => "submit the focused form".to_string(),
            },
            Verb::Back => "go back".to_string(),
            Verb::Read => "read the page text".to_string(),
            Verb::Links => "list the page links".to_string(),
            Verb::Find { query } => format!("find {:?}", truncate(query, 40)),
            Verb::Extract => "extract the page as structured data".to_string(),
            Verb::Watch {
                selector,
                timeout_ms,
            } => format!("watch {selector} for up to {timeout_ms} ms"),
            Verb::Dispatch { target, event, .. } => {
                format!("dispatch {event} at {}", target.describe())
            }
            Verb::Screenshot { path } => match path {
                Some(p) => format!("screenshot to {p}"),
                None => "screenshot the page".to_string(),
            },
            Verb::Pdf { path } => match path {
                Some(p) => format!("PDF to {p}"),
                None => "render the page to PDF".to_string(),
            },
            Verb::Archive { dir } => match dir {
                Some(d) => format!("archive the page to {d}"),
                None => "archive the page as evidence".to_string(),
            },
            Verb::Verify { path } => format!("verify evidence {path}"),
        }
    }

    /// Whether this verb changes anything outside the engine's own memory.
    /// Read-only verbs can be auto-allowed by a headless policy; the rest
    /// are the ones a human should see a prompt for.
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Verb::Read
                | Verb::Links
                | Verb::Find { .. }
                | Verb::Extract
                | Verb::Screenshot { .. }
                | Verb::Pdf { .. }
                | Verb::Verify { .. }
        )
    }

    /// Decode the JSON request the JS surface sends (`{ "verb": …, … }`).
    pub fn from_json(value: &Value) -> Result<Verb, String> {
        let name = value
            .get("verb")
            .and_then(Value::as_str)
            .ok_or("request needs a `verb` field")?;
        let str_arg = |key: &str| -> Result<String, String> {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("`{name}` needs a string `{key}`"))
        };
        let target_arg = |key: &str| -> Result<Target, String> {
            value
                .get(key)
                .ok_or_else(|| format!("`{name}` needs a `{key}`"))
                .and_then(Target::from_json)
        };
        Ok(match name {
            "navigate" => Verb::Navigate {
                url: str_arg("url")?,
            },
            "click" => Verb::Click {
                target: target_arg("target")?,
            },
            "fill" => Verb::Fill {
                target: target_arg("target")?,
                text: value
                    .get("text")
                    .map(|t| match t {
                        Value::Str(s) => s.clone(),
                        other => other.to_json(),
                    })
                    .unwrap_or_default(),
            },
            "submit" => Verb::Submit {
                form: value
                    .get("form")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|s| !s.is_empty()),
            },
            "back" => Verb::Back,
            "read" => Verb::Read,
            "links" => Verb::Links,
            "find" => Verb::Find {
                query: str_arg("query")?,
            },
            "extract" => Verb::Extract,
            "watch" => Verb::Watch {
                selector: str_arg("selector")?,
                timeout_ms: value
                    .get("timeoutMs")
                    .and_then(Value::as_f64)
                    .map(|n| n.clamp(0.0, 120_000.0) as u64)
                    .unwrap_or(5_000),
            },
            "dispatch" => Verb::Dispatch {
                target: target_arg("target")?,
                event: str_arg("event")?,
                detail: value
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            "screenshot" => Verb::Screenshot {
                path: optional_path(value, "path"),
            },
            "pdf" => Verb::Pdf {
                path: optional_path(value, "path"),
            },
            "archive" => Verb::Archive {
                dir: optional_path(value, "dir"),
            },
            "verify" => Verb::Verify {
                path: str_arg("path")?,
            },
            other => return Err(format!("unknown verb `{other}`")),
        })
    }

    /// The JSON a script's method received (round-trip helper for tests,
    /// and the audit-log row for every act an agent takes).
    pub fn to_json(&self) -> Value {
        let mut pairs: Vec<(&str, Value)> = vec![("verb", Value::s(self.name()))];
        match self {
            Verb::Navigate { url } => pairs.push(("url", Value::s(url.clone()))),
            Verb::Click { target } => pairs.push(("target", target.to_json())),
            Verb::Fill { target, text } => {
                pairs.push(("target", target.to_json()));
                pairs.push(("text", Value::s(text.clone())));
            }
            Verb::Submit { form } => {
                if let Some(f) = form {
                    pairs.push(("form", Value::s(f.clone())));
                }
            }
            Verb::Find { query } => pairs.push(("query", Value::s(query.clone()))),
            Verb::Watch {
                selector,
                timeout_ms,
            } => {
                pairs.push(("selector", Value::s(selector.clone())));
                pairs.push(("timeoutMs", Value::Num(*timeout_ms as f64)));
            }
            Verb::Dispatch {
                target,
                event,
                detail,
            } => {
                pairs.push(("target", target.to_json()));
                pairs.push(("event", Value::s(event.clone())));
                if let Some(d) = detail {
                    pairs.push(("detail", Value::s(d.clone())));
                }
            }
            Verb::Screenshot { path } | Verb::Pdf { path } => {
                if let Some(p) = path {
                    pairs.push(("path", Value::s(p.clone())));
                }
            }
            Verb::Archive { dir } => {
                if let Some(d) = dir {
                    pairs.push(("dir", Value::s(d.clone())));
                }
            }
            Verb::Verify { path } => pairs.push(("path", Value::s(path.clone()))),
            Verb::Back | Verb::Read | Verb::Links | Verb::Extract => {}
        }
        Value::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// Same, with the wire names the JS surface and the MCP tools send.
    pub fn from_json_str(request: &str) -> Result<Verb, String> {
        let value = kitae_json::parse(request)?;
        Verb::from_json(&value)
    }

    /// One verb per name, without arguments — the shape a `tools/list`
    /// inventory and a policy grammar are both built from.
    pub fn inventory() -> Vec<Verb> {
        vec![
            Verb::Navigate { url: String::new() },
            Verb::Click {
                target: Target::Selector(String::new()),
            },
            Verb::Fill {
                target: Target::Selector(String::new()),
                text: String::new(),
            },
            Verb::Submit { form: None },
            Verb::Back,
            Verb::Read,
            Verb::Links,
            Verb::Find {
                query: String::new(),
            },
            Verb::Extract,
            Verb::Watch {
                selector: String::new(),
                timeout_ms: 0,
            },
            Verb::Dispatch {
                target: Target::Selector(String::new()),
                event: String::new(),
                detail: None,
            },
            Verb::Screenshot { path: None },
            Verb::Pdf { path: None },
            Verb::Archive { dir: None },
            Verb::Verify {
                path: String::new(),
            },
        ]
    }
}

fn optional_path(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// The host's answer to "may this run?".
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    /// Refused, with the reason a script sees.
    Deny(String),
}

/// What the host grants the act surface. Synchronous by design: the verbs
/// are engine work on one thread, and a promise-per-verb would buy
/// concurrency the engine does not have.
///
/// Deliberately **not** `Send + Sync`: a host that drives a real `Browser`
/// cannot be, because the engine is single-threaded by construction (its
/// host, measurer and UI state are `Rc`/`dyn` on one thread). Requiring
/// `Send` here would mean an adapter that cannot exist for the only host
/// that matters, and it would be a lie in the trait's contract.
pub trait AgentApi {
    /// Apply one verb. `Ok(json)` is the verb's result document (the JS
    /// surface parses it into an object); `Err(message)` becomes a thrown
    /// `Error` in the script. Must not panic and must not block forever.
    fn act(&self, verb: &Verb) -> Result<String, String>;

    /// The permission gate, consulted before every `act`. The **default
    /// denies**: forgetting the policy refuses everything rather than
    /// allowing everything.
    fn permit(&self, verb: &Verb) -> Decision {
        Decision::Deny(format!(
            "no permission policy is installed for `{}`",
            verb.name()
        ))
    }

    /// The verbs a script declared. Declarations inform the policy
    /// (headless auto-allow lists, the app's prompt); they never grant by
    /// themselves.
    fn declare(&self, _verbs: &[String]) {}
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_verb_round_trips_through_its_wire_form() {
        let verbs = vec![
            Verb::Navigate {
                url: "https://example.test/a".into(),
            },
            Verb::Click {
                target: Target::Selector("#go".into()),
            },
            Verb::Click {
                target: Target::Point { x: 12.0, y: 34.0 },
            },
            Verb::Fill {
                target: Target::Selector("input[name=q]".into()),
                text: "hello world".into(),
            },
            Verb::Submit {
                form: Some("f".into()),
            },
            Verb::Submit { form: None },
            Verb::Back,
            Verb::Read,
            Verb::Links,
            Verb::Find {
                query: "tea".into(),
            },
            Verb::Extract,
            Verb::Watch {
                selector: ".done".into(),
                timeout_ms: 2_500,
            },
            Verb::Dispatch {
                target: Target::Selector("#b".into()),
                event: "click".into(),
                detail: Some("x".into()),
            },
            Verb::Screenshot {
                path: Some("/tmp/a.png".into()),
            },
            Verb::Screenshot { path: None },
            Verb::Pdf { path: None },
            Verb::Archive { dir: None },
            Verb::Verify {
                path: "/tmp/a.qtsq".into(),
            },
        ];
        for verb in verbs {
            let json = verb.to_json();
            let back = Verb::from_json(&json).expect("decodes");
            assert_eq!(back, verb, "round trip via {json:?}");
            // …and through the text form, which is what actually travels.
            let text = json.to_json();
            assert_eq!(Verb::from_json_str(&text).unwrap(), verb);
        }
    }

    #[test]
    fn fill_stringifies_non_string_arguments() {
        // The JS surface passes what the script wrote; a number is a
        // reasonable thing to type into a field.
        let verb = Verb::from_json_str(r##"{"verb":"fill","target":"#n","text":42}"##).unwrap();
        assert_eq!(
            verb,
            Verb::Fill {
                target: Target::Selector("#n".into()),
                text: "42".into()
            }
        );
    }

    #[test]
    fn argument_errors_name_the_verb_and_the_field() {
        let cases = [
            (r#"{"verb":"navigate"}"#, "`navigate` needs a string `url`"),
            (r#"{"verb":"click"}"#, "`click` needs a `target`"),
            (
                r#"{"verb":"click","target":""}"#,
                "target selector is empty",
            ),
            (
                r#"{"verb":"click","target":{"x":1}}"#,
                "target point needs both x and y",
            ),
            (r#"{"verb":"nope"}"#, "unknown verb `nope`"),
            (r#"{"zoom":1}"#, "request needs a `verb` field"),
        ];
        for (request, expected) in cases {
            let err = Verb::from_json_str(request).unwrap_err();
            assert!(err.contains(expected), "`{request}` -> {err}");
        }
    }

    #[test]
    fn watch_timeout_is_clamped_to_a_bounded_wait() {
        let verb =
            Verb::from_json_str(r##"{"verb":"watch","selector":"#a","timeoutMs":1e12}"##).unwrap();
        assert_eq!(
            verb,
            Verb::Watch {
                selector: "#a".into(),
                timeout_ms: 120_000
            }
        );
        // Absent timeout: a sane default, never an infinite wait.
        let verb = Verb::from_json_str(r##"{"verb":"watch","selector":"#a"}"##).unwrap();
        assert_eq!(
            verb,
            Verb::Watch {
                selector: "#a".into(),
                timeout_ms: 5_000
            }
        );
    }

    #[test]
    fn names_are_unique_and_the_inventory_is_complete() {
        let inventory = Verb::inventory();
        let mut names: Vec<&str> = inventory.iter().map(Verb::name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "verb names are unique");
        assert_eq!(count, 15, "every verb is in the inventory");
        // Descriptions are the prompt text: they name the target.
        assert_eq!(
            Verb::Navigate {
                url: "https://a.test/".into()
            }
            .describe(),
            "navigate to https://a.test/"
        );
        assert!(Verb::Extract.is_read_only());
        assert!(!Verb::Submit { form: None }.is_read_only());
    }
}
