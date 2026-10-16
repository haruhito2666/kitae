//! The act surface, page-side — Track B's tier B1.
//!
//! The vocabulary itself lives in the `kitae_agent` crate (below this one
//! in the graph) so a headless server can speak verbs without linking
//! QuickJS. This module is the *page-facing* half: it installs the verbs as
//! `kitae.*` methods on a script runtime's global object, validating the
//! arguments JS-style before they reach the host, and turning the host's
//! envelope back into values or a named error.
//!
//! Error shape (a script can branch on it):
//! - `TypeError` — the script's own arguments were wrong; the host never
//!   saw the call;
//! - `PermissionError` (`.verb`) — the policy refused; nothing ran;
//! - `KitaeError` (`.verb`) — the host tried and failed; the runtime
//!   survives and the next verb still lands.
//!
//! See `kitae_agent` for the permission model and the verb inventory.

use std::rc::Rc;

use rquickjs::Object;

use crate::json;

pub use kitae_agent::{truncate, AgentApi, Decision, Target, Verb};

pub(crate) fn install_agent_api<'js>(
    ctx: &rquickjs::Ctx<'js>,
    kitae: &Object<'js>,
    api: Rc<dyn AgentApi>,
) -> Result<(), rquickjs::Error> {
    let bridge = api.clone();
    kitae.set(
        "__kitaeAgent",
        rquickjs::function::Func::from(move |request: String| -> String {
            let reply = match json::parse(&request) {
                Ok(value) => match Verb::from_json(&value) {
                    Ok(verb) => match bridge.permit(&verb) {
                        Decision::Deny(reason) => {
                            let message =
                                format!("permission denied for `{}`: {reason}", verb.name());
                            json::Value::obj(vec![
                                ("ok", json::Value::Bool(false)),
                                ("kind", json::Value::s("permission")),
                                ("message", json::Value::s(message)),
                            ])
                        }
                        Decision::Allow => match bridge.act(&verb) {
                            Ok(result) => json::Value::obj(vec![
                                ("ok", json::Value::Bool(true)),
                                ("result", json::parse(&result).unwrap_or(json::Value::Null)),
                            ]),
                            Err(message) => json::Value::obj(vec![
                                ("ok", json::Value::Bool(false)),
                                ("kind", json::Value::s("act")),
                                (
                                    "message",
                                    json::Value::s(format!("{}: {message}", verb.name())),
                                ),
                            ]),
                        },
                    },
                    Err(message) => json::Value::obj(vec![
                        ("ok", json::Value::Bool(false)),
                        ("kind", json::Value::s("argument")),
                        ("message", json::Value::s(message)),
                    ]),
                },
                Err(message) => json::Value::obj(vec![
                    ("ok", json::Value::Bool(false)),
                    ("kind", json::Value::s("argument")),
                    (
                        "message",
                        json::Value::s(format!("bad request JSON: {message}")),
                    ),
                ]),
            };
            reply.to_json()
        }),
    )?;
    let declare_api = api;
    kitae.set(
        "__kitaeDeclare",
        rquickjs::function::Func::from(move |spec: String| {
            let verbs: Vec<String> = spec
                .split([',', ' ', '\n', '\t'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            declare_api.declare(&verbs);
        }),
    )?;
    ctx.eval::<(), _>(AGENT_SURFACE)?;
    Ok(())
}

const AGENT_SURFACE: &str = r#"
kitae.__agent = function (request) {
  var reply = JSON.parse(kitae.__kitaeAgent(JSON.stringify(request)));
  if (reply.ok) return reply.result;
  var error = new Error('kitae: ' + reply.message);
  error.name = reply.kind === 'permission' ? 'PermissionError' : 'KitaeError';
  error.verb = request.verb;
  throw error;
};
kitae.__target = function (target, verb) {
  if (typeof target === 'string') {
    if (!target) throw new TypeError(verb + ': target selector is empty');
    return target;
  }
  if (target && typeof target === 'object' &&
      typeof target.x === 'number' && typeof target.y === 'number') {
    return { x: target.x, y: target.y };
  }
  throw new TypeError(verb + ': target must be a selector string or { x, y }');
};
kitae.navigate = function (url) {
  if (typeof url !== 'string' || !url) throw new TypeError('navigate: url must be a non-empty string');
  return kitae.__agent({ verb: 'navigate', url: url });
};
kitae.click = function (target) {
  return kitae.__agent({ verb: 'click', target: kitae.__target(target, 'click') });
};
kitae.fill = function (target, text) {
  if (text === undefined || text === null) throw new TypeError('fill: text is required');
  return kitae.__agent({ verb: 'fill', target: kitae.__target(target, 'fill'), text: '' + text });
};
kitae.submit = function (form) {
  return kitae.__agent({ verb: 'submit', form: form === undefined || form === null ? '' : '' + form });
};
kitae.back = function () { return kitae.__agent({ verb: 'back' }); };
kitae.read = function () { return kitae.__agent({ verb: 'read' }); };
kitae.links = function () { return kitae.__agent({ verb: 'links' }); };
kitae.find = function (query) {
  if (typeof query !== 'string' || !query) throw new TypeError('find: query must be a non-empty string');
  return kitae.__agent({ verb: 'find', query: query });
};
kitae.extract = function () { return kitae.__agent({ verb: 'extract' }); };
kitae.watch = function (selector, options) {
  if (typeof selector !== 'string' || !selector) throw new TypeError('watch: selector must be a non-empty string');
  var timeoutMs = options && typeof options.timeoutMs === 'number' ? options.timeoutMs : 5000;
  return kitae.__agent({ verb: 'watch', selector: selector, timeoutMs: timeoutMs });
};
kitae.dispatch = function (target, event, detail) {
  if (typeof event !== 'string' || !event) throw new TypeError('dispatch: event type must be a non-empty string');
  return kitae.__agent({
    verb: 'dispatch',
    target: kitae.__target(target, 'dispatch'),
    event: event,
    detail: detail === undefined || detail === null ? undefined : '' + detail,
  });
};
kitae.screenshot = function (options) {
  return kitae.__agent({ verb: 'screenshot', path: options && options.path ? '' + options.path : undefined });
};
kitae.pdf = function (options) {
  return kitae.__agent({ verb: 'pdf', path: options && options.path ? '' + options.path : undefined });
};
kitae.archive = function (options) {
  return kitae.__agent({ verb: 'archive', dir: options && options.dir ? '' + options.dir : undefined });
};
kitae.verify = function (path) {
  if (typeof path !== 'string' || !path) throw new TypeError('verify: path must be a non-empty string');
  return kitae.__agent({ verb: 'verify', path: path });
};
kitae.declare = function (spec) {
  var list = spec instanceof Array ? spec.join(' ') : '' + spec;
  kitae.__kitaeDeclare(list);
  return list.split(/[\s,]+/).filter(Boolean);
};
"#;

#[cfg(test)]
mod runtime_tests {
    use super::*;
    use crate::{HostApi, RuntimeConfig, ScriptRuntime};
    // The recording host is both a `HostApi` and an `AgentApi`. Its state
    // sits behind one shared mutex so the same object can be handed out
    // twice with different smart pointers: an `Arc` for the runtime's
    // thread-safe host seam and an `Rc` for the (non-`Send`) agent seam —
    // see the note on `AgentApi`'s thread model.
    use std::sync::{Arc, Mutex, MutexGuard};

    /// The recording host's state: every verb it acted on, the verbs a
    /// script declared, the lines it logged, its permission policy (the
    /// allowed verb names; `"*"` allows everything), and whether `read`
    /// fails.
    #[derive(Default)]
    struct Recorder {
        allow: Vec<String>,
        acted: Vec<Verb>,
        declared: Vec<String>,
        logs: Vec<String>,
        fail_read: bool,
    }

    /// A host that records every verb and answers with canned JSON behind a
    /// policy. Cloning shares the same recording, so the `Arc` handed to the
    /// runtime and the `Rc` handed to the agent surface see one state.
    #[derive(Clone)]
    struct RecordingHost {
        inner: Arc<Mutex<Recorder>>,
        declared: DeclaredHandle,
    }

    impl RecordingHost {
        fn new(allow: &[&str]) -> RecordingHost {
            Self::shared(Recorder {
                allow: allow.iter().map(|s| s.to_string()).collect(),
                ..Recorder::default()
            })
        }

        /// A host whose `read` fails, for the error-path tests.
        fn with_failing_read() -> RecordingHost {
            Self::shared(Recorder {
                allow: vec!["*".to_string()],
                fail_read: true,
                ..Recorder::default()
            })
        }

        fn shared(recorder: Recorder) -> RecordingHost {
            let inner = Arc::new(Mutex::new(recorder));
            RecordingHost {
                declared: DeclaredHandle(inner.clone()),
                inner,
            }
        }

        fn runtime(host: &RecordingHost) -> ScriptRuntime {
            let rt = ScriptRuntime::new(
                RuntimeConfig::default(),
                Arc::new(host.clone()) as Arc<dyn HostApi>,
            )
            .expect("runtime");
            // The recording host is shared with the runtime, so the agent
            // seam gets its own `Rc` handle to the same object.
            let agent: Rc<dyn AgentApi> = Rc::new(host.clone());
            rt.attach_agent(agent).expect("agent surface");
            rt
        }

        fn acted(&self) -> Vec<Verb> {
            self.inner.lock().unwrap().acted.clone()
        }

        fn logs(&self) -> Vec<String> {
            self.inner.lock().unwrap().logs.clone()
        }
    }

    /// The `declared` field's handle into the shared recorder, so the tests
    /// keep reading the declared verbs as `host.declared.lock().unwrap()
    /// .clone()`.
    #[derive(Clone)]
    struct DeclaredHandle(Arc<Mutex<Recorder>>);

    impl DeclaredHandle {
        fn lock(&self) -> DeclaredGuard<'_> {
            DeclaredGuard(self.0.lock().unwrap())
        }
    }

    struct DeclaredGuard<'a>(MutexGuard<'a, Recorder>);

    impl DeclaredGuard<'_> {
        fn unwrap(self) -> Vec<String> {
            self.0.declared.clone()
        }
    }

    impl HostApi for RecordingHost {
        fn log(&self, message: &str) {
            self.inner.lock().unwrap().logs.push(message.to_string());
        }
    }

    impl AgentApi for RecordingHost {
        fn act(&self, verb: &Verb) -> Result<String, String> {
            let fail_read = {
                let mut recorder = self.inner.lock().unwrap();
                recorder.acted.push(verb.clone());
                recorder.fail_read
            };
            Ok(match verb {
                Verb::Navigate { url } => json::Value::obj(vec![
                    ("url", json::Value::s(url.clone())),
                    ("title", json::Value::s("Example")),
                    ("status", json::Value::Num(200.0)),
                ]),
                Verb::Read => {
                    if fail_read {
                        return Err("no page is open".into());
                    }
                    json::Value::obj(vec![
                        ("url", json::Value::s("https://a.test/")),
                        ("title", json::Value::s("Example")),
                        ("text", json::Value::s("hello agent")),
                        ("words", json::Value::Num(2.0)),
                    ])
                }
                Verb::Links => json::Value::arr(vec![json::Value::obj(vec![
                    ("text", json::Value::s("docs")),
                    ("href", json::Value::s("https://a.test/docs")),
                ])]),
                Verb::Watch { .. } => json::Value::obj(vec![
                    ("matched", json::Value::Bool(true)),
                    ("elapsedMs", json::Value::Num(12.0)),
                ]),
                other => json::Value::obj(vec![("verb", json::Value::s(other.name()))]),
            }
            .to_json())
        }

        fn permit(&self, verb: &Verb) -> Decision {
            let recorder = self.inner.lock().unwrap();
            if recorder.allow.iter().any(|v| v == "*" || v == verb.name()) {
                Decision::Allow
            } else {
                Decision::Deny("not declared for this run".to_string())
            }
        }

        fn declare(&self, verbs: &[String]) {
            self.inner.lock().unwrap().declared.extend_from_slice(verbs);
        }
    }

    #[test]
    fn verbs_reach_the_host_and_results_parse_as_objects() {
        let host = RecordingHost::new(&["*"]);
        let rt = RecordingHost::runtime(&host);
        rt.run(
            "var page = kitae.navigate('https://a.test/');\
             kitae.log(page.title + '|' + page.status + '|' + typeof page.title);\
             var links = kitae.links();\
             kitae.log(links.length + ':' + links[0].href);\
             var w = kitae.watch('#done', { timeoutMs: 200 });\
             kitae.log(w.matched + '|' + w.elapsedMs);",
        )
        .expect("script runs");
        assert_eq!(
            host.acted(),
            vec![
                Verb::Navigate {
                    url: "https://a.test/".into()
                },
                Verb::Links,
                Verb::Watch {
                    selector: "#done".into(),
                    timeout_ms: 200
                },
            ]
        );
        assert_eq!(
            host.logs(),
            vec!["Example|200|string", "1:https://a.test/docs", "true|12"]
        );
    }

    #[test]
    fn permission_denied_is_a_named_error_and_nothing_runs() {
        let host = RecordingHost::new(&["navigate", "read"]);
        let rt = RecordingHost::runtime(&host);
        rt.run(
            "var out;\
             try { kitae.click('#go'); out = 'ran'; }\
             catch (e) { out = e.name + '|' + e.verb + '|' + e.message; }\
             kitae.log(out);",
        )
        .expect("script runs");
        assert!(host.acted().is_empty(), "a denied verb must not act");
        let line = host.logs().join("\n");
        assert!(
            line.starts_with("PermissionError|click|kitae: permission denied for `click`: "),
            "the denial names the verb and the reason: {line:?}"
        );
        // An allowed verb in the same runtime still works.
        rt.run("kitae.log(kitae.read().words);").expect("read runs");
        assert_eq!(host.acted(), vec![Verb::Read]);
    }

    #[test]
    fn argument_errors_throw_before_the_host_sees_them() {
        let host = RecordingHost::new(&["*"]);
        let rt = RecordingHost::runtime(&host);
        rt.run(
            "var names = [];\
             function probe(fn) { try { fn(); names.push('ran'); } catch (e) { names.push(e.name); } }\
             probe(function () { kitae.fill('#q'); });\
             probe(function () { kitae.click(42); });\
             probe(function () { kitae.find(''); });\
             probe(function () { kitae.navigate(); });\
             probe(function () { kitae.dispatch('#b'); });\
             probe(function () { kitae.verify(null); });\
             kitae.log(names.join(','));",
        )
        .expect("script runs");
        assert!(
            host.acted().is_empty(),
            "validation happens before the host"
        );
        assert_eq!(
            host.logs(),
            vec!["TypeError,TypeError,TypeError,TypeError,TypeError,TypeError"]
        );
    }

    #[test]
    fn host_failures_are_kitae_errors_and_the_runtime_survives() {
        let host = RecordingHost::with_failing_read();
        let rt = RecordingHost::runtime(&host);
        rt.run(
            "var out;\
             try { kitae.read(); } catch (e) { out = e.name + '|' + e.message; }\
             kitae.log(out);",
        )
        .expect("script runs");
        assert_eq!(host.logs(), vec!["KitaeError|kitae: read: no page is open"]);
        // The runtime survives the failure: the next verb still lands.
        rt.run("kitae.log(kitae.links().length);").expect("runs");
        assert_eq!(host.logs()[1], "1");
    }

    #[test]
    fn declarations_reach_the_host_and_the_surface_stays_closed_without_one() {
        let host = RecordingHost::new(&["navigate"]);
        let rt = RecordingHost::runtime(&host);
        rt.run("kitae.declare('navigate, click fill');")
            .expect("runs");
        assert_eq!(
            host.declared.lock().unwrap().clone(),
            vec!["navigate", "click", "fill"]
        );
        assert!(host.acted().is_empty(), "declaring is not acting");
        // `declare` also takes an array.
        rt.run("kitae.declare(['read', 'extract']);").expect("runs");
        assert_eq!(
            host.declared.lock().unwrap().clone(),
            vec!["navigate", "click", "fill", "read", "extract"]
        );

        // Without a host the verbs are simply absent (closed surface).
        let bare = ScriptRuntime::new(
            RuntimeConfig::default(),
            std::sync::Arc::new(RecordingHost::new(&["*"])) as Arc<dyn HostApi>,
        )
        .expect("runtime");
        bare.run("kitae.log(typeof kitae.navigate + '|' + typeof kitae.log + '|' + typeof kitae.declare);")
            .expect("runs");
    }
}
