//! kitae_script — the closed-API QuickJS runtime for Kitae automation.
//!
//! Scope (`docs/SCRIPTING_DECISION.md`): this is the one script engine
//! Kitae ships, and it executes *automation scripts* — user scripts, agent
//! scripts, server-side `kitae_ui` logic — against **Kitae's own closed
//! API surface**. It never runs web JavaScript: nothing here touches the
//! page pipeline, and fetched pages are never executed. The display list
//! remains the engine's endpoint.
//!
//! Why this shape converges: the API surface is an explicit inventory we
//! own — growing it "bit by bit" is a bounded project with a finish line,
//! unlike a web-DOM emulator racing Chromium's unbounded tail.
//!
//! Non-negotiable runtime guarantees (each guarded by a test):
//! - every context carries a hard memory budget (`RuntimeConfig`);
//! - every run carries a wall-clock deadline enforced by an interrupt;
//! - scripts surface *errors as values* (`ScriptError`), never panics —
//!   the runtime outlives every script, including broken ones;
//! - the surface is closed: scripts see `kitae.*` and the standard
//!   library, nothing else — no `fetch`, no timers, no I/O the inventory
//!   did not grant.

pub mod agent;
// The codec itself lives in the zero-dependency `kitae_json` crate (below
// this one in the graph) so `kitae_core::extract` can shape agent-facing
// JSON with `page-js` off. Re-exported here as `kitae_script::json` so the
// script surface still reads as one module tree.
pub use kitae_json as json;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rquickjs::function::Func;
use rquickjs::loader::{Loader, Resolver};
use rquickjs::{Context, Module, Runtime as QjsRuntime};

/// Re-export so embedders build against the *same* QuickJS/rquickjs
/// instance this crate links (no version skew, one C runtime).
pub use rquickjs;

/// What the host grants this runtime. The automation surface grows bit by
/// bit against the inventory in `docs/SCRIPTING_DECISION.md`; every method
/// here is one reviewed row, not an escape hatch.
pub trait HostApi: Send + Sync {
    /// Structured logging from scripts (`kitae.log`). The JS surface
    /// stringifies whatever the script logs (JSON for objects); the sink
    /// receives the finished line.
    fn log(&self, message: &str);
}

/// The host-side ES-module fetcher. QuickJS hands every `import` (static
/// and dynamic) to the resolver for canonicalization, then to the loader
/// for source. The host owns both policies — URL joining, budgets, origin
/// gates, caching — this crate only carries the errors into JS.
pub trait ModuleFetcher: Send + Sync {
    /// Canonicalize `(canonical name of the importing module, specifier)`
    /// to a stable module name. An error rejects the whole import chain
    /// with `Could not find module`.
    fn resolve(&self, base: &str, name: &str) -> Result<String, String>;
    /// Fetch the resolved module's source synchronously. May touch the
    /// network; callers on the engine side budget that.
    fn load(&self, url: &str) -> Result<String, String>;
}

struct ResolverAdapter(Arc<dyn ModuleFetcher>);

impl Resolver for ResolverAdapter {
    fn resolve<'js>(
        &mut self,
        _ctx: &rquickjs::Ctx<'js>,
        base: &str,
        name: &str,
    ) -> rquickjs::Result<String> {
        self.0
            .resolve(base, name)
            .map_err(|_| rquickjs::Error::new_loading(name))
    }
}

struct LoaderAdapter(Arc<dyn ModuleFetcher>);

impl Loader for LoaderAdapter {
    fn load<'js>(
        &mut self,
        ctx: &rquickjs::Ctx<'js>,
        name: &str,
    ) -> rquickjs::Result<Module<'js, rquickjs::module::Declared>> {
        let source = self
            .0
            .load(name)
            .map_err(|_| rquickjs::Error::new_loading(name))?;
        Module::declare(ctx.clone(), name, source)
    }
}

/// Runtime budgets. Conservative defaults: automation scripts are small
/// and must never degrade the host — 8 MiB of heap, 1 MiB of interpreter
/// stack, five seconds of wall clock.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub memory_bytes: usize,
    pub max_stack_bytes: usize,
    pub deadline_ms: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            memory_bytes: 8 * 1024 * 1024,
            max_stack_bytes: 1024 * 1024,
            deadline_ms: 5_000,
        }
    }
}

/// Everything a script run can end with. Errors are values: a hostile or
/// merely broken script must be diagnosable and disposable, never able to
/// unwind the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptError {
    /// The script exceeded its wall-clock deadline (runaway loop).
    DeadlineExceeded,
    /// The script exceeded its memory budget.
    MemoryExceeded,
    /// The script did not parse.
    Syntax(String),
    /// The script threw (including host-API rejections).
    Thrown(String),
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScriptError::DeadlineExceeded => write!(f, "script exceeded its deadline"),
            ScriptError::MemoryExceeded => write!(f, "script exceeded its memory budget"),
            ScriptError::Syntax(s) => write!(f, "script did not parse: {s}"),
            ScriptError::Thrown(s) => write!(f, "script threw: {s}"),
        }
    }
}

impl std::error::Error for ScriptError {}

/// One QuickJS runtime with its closed `kitae` API installed. Accepts many
/// sequential runs, broken ones included.
pub struct ScriptRuntime {
    rt: QjsRuntime,
    ctx: Context,
    /// Millisecond clock at which the current run began; 0 when idle (the
    /// interrupt handler keeps running scripts when idle).
    started_ms: Arc<AtomicU64>,
    deadline_ms: u64,
}

impl ScriptRuntime {
    /// Create a runtime backed by `api` under `config`'s budgets.
    pub fn new(config: RuntimeConfig, api: Arc<dyn HostApi>) -> Result<Self, ScriptError> {
        let rt =
            QjsRuntime::new().map_err(|e| ScriptError::Thrown(format!("runtime init: {e}")))?;
        rt.set_memory_limit(config.memory_bytes);
        rt.set_max_stack_size(config.max_stack_bytes);
        let ctx =
            Context::full(&rt).map_err(|e| ScriptError::Thrown(format!("context init: {e}")))?;

        // The interrupt handler IS the time budget: QuickJS polls it while
        // executing, and returning `true` raises an uncatchable exception
        // that aborts the script. Runs stamp `started_ms`; an idle runtime
        // (stamp 0) never aborts.
        let started = Arc::new(AtomicU64::new(0));
        let deadline_ms = config.deadline_ms;
        let handler_started = started.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            let begin = handler_started.load(Ordering::Relaxed);
            begin != 0 && now_ms().saturating_sub(begin) >= deadline_ms
        })));

        let runtime = Self {
            rt,
            ctx,
            started_ms: started,
            deadline_ms: config.deadline_ms,
        };
        runtime.install_kitae_api(api)?;
        Ok(runtime)
    }

    /// The configured wall-clock deadline (inspection).
    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }

    /// Attach the act surface (tier B1): the `kitae.navigate`/`click`/
    /// `fill`/… verbs, gated by `api`'s permission decisions. Call before
    /// the first `run`; without a host the verbs stay undefined — a
    /// runtime that cannot act does not pretend it can.
    pub fn attach_agent(&self, api: std::rc::Rc<dyn agent::AgentApi>) -> Result<(), ScriptError> {
        self.ctx
            .with(|ctx| -> Result<(), rquickjs::Error> {
                let globals = ctx.globals();
                let kitae: rquickjs::Object = globals.get("kitae")?;
                agent::install_agent_api(&ctx, &kitae, api)
            })
            .map_err(|e| ScriptError::Thrown(format!("agent surface install failed: {e}")))
    }

    /// Run one pending promise/microtask job, if any. `Ok(false)` when
    /// the job queue is empty — call in a loop after evaluation (and
    /// after timer/callback invocations) so settled promises resolve
    /// without an externally-driven event loop.
    pub fn pump_job(&self) -> Result<bool, ScriptError> {
        self.rt
            .execute_pending_job()
            .map_err(|_| ScriptError::Thrown("microtask threw".into()))
    }

    /// Drain all pending microtask jobs (bounded defensively).
    pub fn pump_jobs(&self) -> Result<(), ScriptError> {
        for _ in 0..10_000 {
            if !self.pump_job()? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Enter the JS context (advanced): registry plumbing that must touch
    /// globals between runs — callbacks stored JS-side, promise settling —
    /// goes through here. Callers must not evaluate unbounded scripts.
    pub fn with_context<R>(&self, f: impl FnOnce(&rquickjs::Ctx<'_>) -> R) -> R {
        self.ctx.with(|ctx| f(&ctx))
    }

    /// Parse and execute one script, top-level. Each run resets the time
    /// budget; the memory budget is shared by the runtime. `Ok(())` means
    /// the script completed without throwing.
    pub fn run(&self, source: &str) -> Result<(), ScriptError> {
        self.started_ms.store(now_ms(), Ordering::Relaxed);
        // The web's default is sloppy mode: implicit globals are legal,
        // strictness arrives only via the script's own "use strict".
        // EvalOptions is non_exhaustive: mutate the default instead.
        let mut options = rquickjs::context::EvalOptions::default();
        options.strict = false;
        let outcome: Result<(), ScriptError> = self.ctx.with(|ctx| {
            match ctx.eval_with_options::<rquickjs::Value, _>(source, options) {
                Ok(_) => Ok(()),
                Err(rquickjs::Error::Exception) => {
                    // The pending exception is the script's answer: fetch
                    // and type it before the context borrow ends.
                    let value = ctx.catch();
                    Err(exception_script_error(&value))
                }
                Err(e) => Err(qjs_error(&e)),
            }
        });
        self.started_ms.store(0, Ordering::Relaxed);
        outcome
    }

    /// Install the ES-module fetcher. Call once after construction; every
    /// `import` in any script or module then routes through it.
    pub fn set_module_fetcher(&self, fetcher: Arc<dyn ModuleFetcher>) {
        self.rt
            .set_loader(ResolverAdapter(fetcher.clone()), LoaderAdapter(fetcher));
    }

    /// Compile and evaluate one ES module (`name` is the module's
    /// canonical URL; imports resolve through the installed fetcher).
    /// Strict-mode always — the spec mandates it for modules. The wall
    /// clock covers import loading too, so an import chain cannot outrun
    /// the deadline by re-entering the fetcher.
    pub fn run_module(&self, name: &str, source: &str) -> Result<(), ScriptError> {
        self.started_ms.store(now_ms(), Ordering::Relaxed);
        let outcome = self.ctx.with(|ctx| -> Result<(), ScriptError> {
            // quickjs-ng also loads static imports during the compile step,
            // so fetcher failures surface here as a pending exception.
            let module = match Module::declare(ctx.clone(), name, source) {
                Ok(m) => m,
                Err(rquickjs::Error::Exception) => {
                    let value = ctx.catch();
                    return Err(exception_script_error(&value));
                }
                Err(e) => return Err(qjs_error(&e)),
            };
            match module.eval() {
                // Module evaluation failures are delivered through the
                // returned promise's rejection, not the eval result.
                Ok((_, promise)) => match promise.result::<rquickjs::Value>() {
                    Some(Ok(_)) | None => Ok(()),
                    Some(Err(rquickjs::Error::Exception)) => {
                        let value = ctx.catch();
                        Err(exception_script_error(&value))
                    }
                    Some(Err(e)) => Err(qjs_error(&e)),
                },
                Err(rquickjs::Error::Exception) => {
                    let value = ctx.catch();
                    Err(exception_script_error(&value))
                }
                Err(e) => Err(qjs_error(&e)),
            }
        });
        self.started_ms.store(0, Ordering::Relaxed);
        outcome
    }

    /// Install the closed `kitae` surface onto the global object.
    fn install_kitae_api(&self, api: Arc<dyn HostApi>) -> Result<(), ScriptError> {
        self.ctx
            .with(|ctx| -> Result<(), rquickjs::Error> {
                let globals = ctx.globals();
                let kitae = rquickjs::Object::new(ctx.clone())?;
                kitae.set(
                    "log",
                    Func::from(move |args: rquickjs::function::Rest<String>| {
                        let line = args.0.join(" ");
                        api.log(&line);
                    }),
                )?;
                globals.set("kitae", kitae)?;
                // `kitae.log` follows `console.log`: any value logs, with
                // objects and arrays carrying JSON (circular ones fall back
                // to `String`). The Rust sink stays string-only.
                ctx.eval::<(), _>(LOG_SHAPE)?;
                Ok(())
            })
            .map_err(|e| ScriptError::Thrown(format!("api install failed: {e}")))
    }
}

/// The `kitae.log` wrapper (see `install_kitae_api`).
const LOG_SHAPE: &str = r#"
kitae.__log = kitae.log;
kitae.log = function () {
  var parts = [];
  for (var i = 0; i < arguments.length; i++) {
    var a = arguments[i];
    if (typeof a === 'string') parts.push(a);
    else if (a === undefined) parts.push('undefined');
    else if (a === null) parts.push('null');
    else {
      try { parts.push(JSON.stringify(a)); } catch (e) { parts.push(String(a)); }
    }
  }
  kitae.__log(parts.join(' '));
};
"#;

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A fetched JS exception value → typed error. Error objects carry a
/// constructor class (`SyntaxError`, `TypeError`, …) and a `message`;
/// budget aborts (deadline / memory) arrive as `InternalError`s with
/// QuickJS's canonical wording; thrown primitives stringify.
fn exception_script_error(value: &rquickjs::Value<'_>) -> ScriptError {
    let message = value
        .as_object()
        .and_then(|obj| obj.get::<_, String>("message").ok())
        .unwrap_or_else(|| "exception".to_string());
    if message.contains("interrupted") {
        return ScriptError::DeadlineExceeded;
    }
    if message.contains("out of memory") {
        return ScriptError::MemoryExceeded;
    }
    let class = value
        .as_object()
        .and_then(|obj| obj.get::<_, rquickjs::Object>("constructor").ok())
        .and_then(|ctor| ctor.get::<_, String>("name").ok());
    if class.as_deref() == Some("SyntaxError") {
        ScriptError::Syntax(message)
    } else if let Some(name) = class {
        ScriptError::Thrown(format!("{name}: {message}"))
    } else if let Some(primitive) = value.as_string().and_then(|s| s.to_string().ok()) {
        ScriptError::Thrown(primitive)
    } else {
        ScriptError::Thrown(message)
    }
}

/// A transport-level QuickJS error → typed error. Interrupts (the
/// deadline) and the memory cap come through here; matched on QuickJS's
/// canonical wording once, documented here once.
fn qjs_error(e: &rquickjs::Error) -> ScriptError {
    let text = e.to_string();
    if text.contains("interrupted") {
        ScriptError::DeadlineExceeded
    } else if text.contains("out of memory") || text.contains("Kernel panic") {
        ScriptError::MemoryExceeded
    } else if text.starts_with("SyntaxError") {
        ScriptError::Syntax(text)
    } else {
        ScriptError::Thrown(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockHost {
        lines: Mutex<Vec<String>>,
    }

    impl HostApi for MockHost {
        fn log(&self, message: &str) {
            self.lines.lock().unwrap().push(message.to_string());
        }
    }

    fn runtime() -> (ScriptRuntime, Arc<MockHost>) {
        let host = Arc::new(MockHost::default());
        let rt =
            ScriptRuntime::new(RuntimeConfig::default(), host.clone()).expect("runtime builds");
        (rt, host)
    }

    #[test]
    fn runs_a_script_and_reaches_the_host_api() {
        let (rt, host) = runtime();
        rt.run("kitae.log('hello from', 'quickjs');").expect("runs");
        assert_eq!(
            host.lines.lock().unwrap().as_slice(),
            ["hello from quickjs"]
        );
        // Many runs on one runtime.
        rt.run("kitae.log('second');").expect("runs again");
        assert_eq!(host.lines.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_surface_is_closed() {
        let (rt, _host) = runtime();
        // The kitae object and the standard library exist; nothing
        // web-shaped leaks in. No fetch, no XHR, no timers — the
        // automation surface only.
        rt.run(
            "if (typeof kitae !== 'object') throw new Error('kitae missing');\
             if (typeof fetch !== 'undefined') throw new Error('fetch must not exist');\
             if (typeof XMLHttpRequest !== 'undefined') throw new Error('XHR must not exist');\
             if (typeof setTimeout !== 'undefined') throw new Error('timers must not exist (v0)');\
             if (typeof Math !== 'object') throw new Error('standard lib must exist');",
        )
        .expect("closed-surface assertions hold");
    }

    #[test]
    fn syntax_errors_are_values_and_the_runtime_survives() {
        let (rt, _host) = runtime();
        let err = rt.run("this is not javascript at all {").unwrap_err();
        assert!(matches!(err, ScriptError::Syntax(_)), "got {err:?}");
        // The same runtime accepts a good script right after.
        rt.run("kitae.log('still alive');").expect("recovers");
    }

    #[test]
    fn thrown_errors_carry_the_message() {
        let (rt, _host) = runtime();
        let err = rt.run("throw new TypeError('bogus input');").unwrap_err();
        match err {
            ScriptError::Thrown(text) => {
                assert!(text.contains("bogus input"), "message carried: {text}");
            }
            other => panic!("expected Thrown, got {other:?}"),
        }
        // And the runtime took no damage from the throw.
        rt.run("kitae.log('post-throw');").expect("recovers");
    }

    #[test]
    fn runaway_loops_hit_the_deadline() {
        let host = Arc::new(MockHost::default());
        let config = RuntimeConfig {
            deadline_ms: 120,
            ..RuntimeConfig::default()
        };
        let rt = ScriptRuntime::new(config, host).expect("runtime builds");
        let err = rt.run("let x = 0; for (;;) { x = x + 1; }").unwrap_err();
        assert_eq!(err, ScriptError::DeadlineExceeded);
        // The interrupted runtime still accepts the next script.
        rt.run("kitae.log('post-interrupt');").expect("recovers");
    }

    #[test]
    fn runaway_loops_finish_before_the_deadline_when_given_time() {
        let host = Arc::new(MockHost::default());
        let config = RuntimeConfig {
            deadline_ms: 5_000,
            ..RuntimeConfig::default()
        };
        let rt = ScriptRuntime::new(config, host).expect("runtime builds");
        rt.run("let s = 0; for (let i = 0; i < 2_000_000; i++) { s = (s + i) % 7; }")
            .expect("an honest loop finishes");
    }

    #[test]
    fn memory_hogs_hit_the_budget() {
        let host = Arc::new(MockHost::default());
        let config = RuntimeConfig {
            memory_bytes: 512 * 1024,
            ..RuntimeConfig::default()
        };
        let rt = ScriptRuntime::new(config, host).expect("runtime builds");
        let err = rt
            .run("let a = []; for (let i = 0; i < 1000000; i++) { a.push({i}); }")
            .unwrap_err();
        assert_eq!(err, ScriptError::MemoryExceeded);
        // Budget-tripped runtime recovers for the next, honest script.
        rt.run("kitae.log('post-budget');").expect("recovers");
    }

    // ── ES modules ──

    /// A tiny in-memory fetcher: module names are absolute "/…" paths and
    /// relative imports join against the importing module's directory.
    struct MapFetcher {
        modules: Mutex<HashMap<String, String>>,
    }

    /// `/`-rooted path join with `.` / `..` segment folding.
    fn toy_join(base_dir: &str, spec: &str) -> String {
        let mut segs: Vec<&str> = base_dir
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        for part in spec.split('/') {
            match part {
                "" | "." | "/" => {}
                ".." => {
                    segs.pop();
                }
                other => segs.push(other),
            }
        }
        format!("/{}", segs.join("/"))
    }

    impl ModuleFetcher for MapFetcher {
        fn resolve(&self, base: &str, name: &str) -> Result<String, String> {
            let dir = base.rsplit_once('/').map(|(d, _)| d).unwrap_or_default();
            let joined = toy_join(dir, name);
            if self.modules.lock().unwrap().contains_key(&joined) {
                Ok(joined)
            } else {
                Err(format!("unknown module {name}"))
            }
        }

        fn load(&self, url: &str) -> Result<String, String> {
            self.modules
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| format!("no source for {url}"))
        }
    }

    fn module_runtime(modules: &[(&str, &str)]) -> (ScriptRuntime, Arc<MockHost>) {
        let map: HashMap<String, String> = modules
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let fetcher: Arc<dyn ModuleFetcher> = Arc::new(MapFetcher {
            modules: Mutex::new(map),
        });
        let host = Arc::new(MockHost::default());
        let rt =
            ScriptRuntime::new(RuntimeConfig::default(), host.clone()).expect("runtime builds");
        rt.set_module_fetcher(fetcher);
        (rt, host)
    }

    #[test]
    fn static_import_chains_load_through_the_fetcher() {
        let (rt, host) = module_runtime(&[
            (
                "/app/main.mjs",
                "import { v } from \"./dep.mjs\";\nkitae.log(\"value \" + v);",
            ),
            ("/app/dep.mjs", "export const v = 7;"),
        ]);
        rt.run_module(
            "/app/main.mjs",
            "import { v } from \"./dep.mjs\";\nkitae.log(\"value \" + v);",
        )
        .expect("module chain runs");
        assert_eq!(host.lines.lock().unwrap().as_slice(), ["value 7"]);
    }

    #[test]
    fn dynamic_import_settles_through_the_job_queue() {
        // A classic (sloppy) script has no module URL, so the fetcher sees
        // an empty base — the specifier lands at the root namespace.
        let (rt, host) = module_runtime(&[("/m.mjs", "export const v = 3;")]);
        rt.run("import('./m.mjs').then(function (m) { kitae.log('dynamic ' + m.v); });")
            .expect("import() starts");
        let _ = rt.pump_jobs();
        assert_eq!(host.lines.lock().unwrap().as_slice(), ["dynamic 3"]);
    }

    #[test]
    fn module_errors_are_values_and_the_runtime_survives() {
        let (rt, _host) = module_runtime(&[("/app/main.mjs", "export const v = 1;")]);
        // A bare specifier has no fetcher answer: the whole chain fails.
        let err = rt
            .run_module("/app/main.mjs", "import \"react\";")
            .unwrap_err();
        assert!(
            matches!(err, ScriptError::Thrown(ref s) if s.contains("module")),
            "bare specifier surfaces as a thrown error, got {err:?}"
        );
        // The same runtime still runs a good module afterwards.
        rt.run_module("/app/main.mjs", "kitae.log('alive');")
            .expect("recovers");
    }

    #[test]
    fn module_throws_face_the_deadline_handler() {
        let (rt, _host) = module_runtime(&[("/app/main.mjs", "export const boom = missing_var;")]);
        let err = rt
            .run_module("/app/main.mjs", "export const boom = missing_var;")
            .unwrap_err();
        assert!(
            matches!(err, ScriptError::Thrown(ref s) if s.contains("missing_var")),
            "module throw carries the message, got {err:?}"
        );
    }
}
