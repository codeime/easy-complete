//! One QuickJS-NG runtime for extracted Fig generator hooks.
//!
//! Static IR walk stays in Rust. This module is only entered when an argument
//! or node carries a hook id. The host mirrors the old WebView
//! `executeCommand` / `cleanOutput` contract.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rquickjs::{Context, Ctx, Function, Object, Runtime, Value as JsValue};
use serde_json::Value as JsonValue;

use crate::generate::DEFAULT_SCRIPT_TIMEOUT_MS;
use crate::ir::{
    ArgSpec, Builtin, FilterStrategy, GeneratorSpec, GeneratorTrigger, LoadSpec, OptionSpec, ParserDirectives, Spec,
    SuggestionMeta, SuggestionSeed, Template,
};
use crate::process::{self, CommandError};
use crate::runtime::Suggestion;

const MEMORY_LIMIT: usize = 16 * 1024 * 1024;
const STACK_LIMIT: usize = 512 * 1024;
const MAX_JOBS: usize = 10_000;

/// Slack added to a hook's script budget before the interpreter is interrupted.
/// A generator whose final `executeCommand` finishes right on its own timeout
/// still needs a moment of JS time to shape the result rows.
const HOOK_DEADLINE_MARGIN: Duration = Duration::from_secs(2);

thread_local! {
    static ACTIVE: Cell<Option<Active>> = const { Cell::new(None) };
    static INNER: RefCell<Option<Inner>> = const { RefCell::new(None) };
    /// Wall-clock bound for the currently running hook. QuickJS polls this
    /// through the interrupt handler, so a hook that spins in JS (or keeps
    /// scheduling promise jobs) is aborted instead of wedging the completion
    /// attempt until the 30s supervisor watchdog abandons the whole thread.
    static HOOK_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy)]
struct Active {
    host: *const JsHost,
    cwd: *const str,
    shell: *const ShellContext,
}

/// The subset of `Fig.ShellContext` that generators actually read. The WebView
/// received it from figterm on every keystroke; here it rides along with the
/// completion request and is bound for the duration of one attempt.
#[derive(Debug, Clone, Default)]
pub struct ShellContext {
    pub current_process: String,
    pub environment_variables: Arc<Vec<(String, String)>>,
}

fn empty_shell_context() -> &'static ShellContext {
    static EMPTY: OnceLock<ShellContext> = OnceLock::new();
    EMPTY.get_or_init(ShellContext::default)
}

pub struct JsHost {
    hooks_dir: PathBuf,
    sources: Mutex<HashMap<String, String>>,
    /// `custom` generator results, keyed like Fig's `generatorCache`.
    suggestion_cache: Mutex<HashMap<String, CacheEntry<Vec<Suggestion>>>>,
    /// Script generator stdout. Fig caches the `executeCommand` output and
    /// re-applies `splitOn` / `postProcess` on every hit, so the hook still
    /// sees the current tokens; caching rows here would freeze them.
    script_output_cache: Mutex<HashMap<String, CacheEntry<String>>>,
    spec_cache: Mutex<HashMap<String, Spec>>,
}

struct Inner {
    runtime: Runtime,
    context: Context,
}

#[derive(Clone)]
struct CacheEntry<T> {
    value: T,
    fetched_at: Instant,
}

/// Fig `CacheEntry.entry`: both strategies serve the stored value until its
/// TTL passes, then refetch and hand back the *fresh* result. (`swrCache`
/// only returns the stale value while another fetch is in flight, which a
/// synchronous engine never has.) `None` never expires.
#[derive(Clone, Copy)]
struct CachePolicy {
    ttl: Option<Duration>,
}

impl JsHost {
    pub fn new(hooks_dir: PathBuf) -> Self {
        Self {
            hooks_dir,
            sources: Mutex::new(HashMap::new()),
            suggestion_cache: Mutex::new(HashMap::new()),
            script_output_cache: Mutex::new(HashMap::new()),
            spec_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_specs_dir(specs_dir: &Path) -> Self {
        Self::new(specs_dir.join("hooks"))
    }

    /// Bind this host for the duration of a completion attempt so generators
    /// and walk can reach it without threading an extra argument through every
    /// lookup helper.
    #[cfg(test)]
    pub fn enter<R>(&self, cwd: &str, f: impl FnOnce() -> R) -> R {
        self.enter_with_context(cwd, empty_shell_context(), f)
    }

    /// Same as [`JsHost::enter`], plus the shell facts that `custom` generators
    /// expect on their context argument.
    pub fn enter_with_context<R>(&self, cwd: &str, shell: &ShellContext, f: impl FnOnce() -> R) -> R {
        ACTIVE.with(|cell| {
            let previous = cell.get();
            cell.set(Some(Active {
                host: self as *const JsHost,
                cwd: cwd as *const str,
                shell: shell as *const ShellContext,
            }));
            let result = f();
            cell.set(previous);
            result
        })
    }
}

/// Run `f` with no JS host bound, so every hook site takes its static path.
/// Fig parsed history with `exec` swapped for a function that throws, which
/// made any hook that shells out fail and skip that line; not binding the
/// host at all is the same result for the spec walk without the failed
/// attempts.
pub fn without_hooks<R>(f: impl FnOnce() -> R) -> R {
    ACTIVE.with(|cell| {
        let previous = cell.replace(None);
        let result = f();
        cell.set(previous);
        result
    })
}

impl JsHost {
    fn with_inner<R>(f: impl FnOnce(&Inner) -> R) -> Option<R> {
        INNER.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                let runtime = Runtime::new().ok()?;
                runtime.set_memory_limit(MEMORY_LIMIT);
                runtime.set_max_stack_size(STACK_LIMIT);
                runtime.set_interrupt_handler(Some(Box::new(|| {
                    HOOK_DEADLINE.with(|cell| cell.get().is_some_and(|deadline| Instant::now() > deadline))
                })));
                let context = Context::full(&runtime).ok()?;
                if context
                    .with(|ctx| {
                        ctx.eval::<(), _>(
                            r#"
globalThis.console = {
  log: function () {},
  warn: function () {},
  error: function () {},
  info: function () {},
  debug: function () {}
};
"#,
                        )
                    })
                    .is_err()
                {
                    return None;
                }
                *slot = Some(Inner { runtime, context });
            }
            Some(f(slot.as_ref()?))
        })
    }

    fn hook_source(&self, id: &str) -> Option<String> {
        {
            let sources = self.sources.lock().unwrap_or_else(|err| err.into_inner());
            if let Some(source) = sources.get(id) {
                return Some(source.clone());
            }
        }
        let path = hook_path(&self.hooks_dir, id);
        let source = fs::read_to_string(path).ok()?;
        let mut sources = self.sources.lock().unwrap_or_else(|err| err.into_inner());
        sources.insert(id.to_string(), source.clone());
        Some(source)
    }

    pub fn post_process(&self, hook_id: &str, stdout: &str, tokens: &[String]) -> Option<Vec<Suggestion>> {
        let json = self.call_hook(hook_id, default_hook_budget(), |ctx, hook| {
            let tokens = tokens_value(ctx, tokens)?;
            call_hook(ctx, hook, (stdout, tokens))
        })?;
        suggestions_from_json(&json, false)
    }

    pub fn custom(
        &self,
        hook_id: &str,
        tokens: &[String],
        cwd: &str,
        search_term: &str,
        timeout: Duration,
        is_dangerous: bool,
    ) -> Option<Vec<Suggestion>> {
        let json = self.call_hook(hook_id, timeout, |ctx, hook| {
            let tokens_js = tokens_value(ctx, tokens)?;
            let exec = execute_command_fn(ctx, cwd, timeout)?;
            let context = custom_context(ctx, cwd, search_term, is_dangerous)?;
            call_hook(ctx, hook, (tokens_js, exec, context))
        })?;
        suggestions_from_json(&json, is_dangerous)
    }

    pub fn script_command(&self, hook_id: &str, tokens: &[String]) -> Option<ScriptCommand> {
        let json = self.call_hook(hook_id, default_hook_budget(), |ctx, hook| {
            let tokens_js = tokens_value(ctx, tokens)?;
            call_hook(ctx, hook, (tokens_js,))
        })?;
        script_command_from_json(&json)
    }

    pub fn generate_spec(&self, hook_id: &str, tokens: &[String], cwd: &str, timeout: Duration) -> Option<Spec> {
        let json = self.call_hook_rewrite(hook_id, timeout, |ctx, hook| {
            let tokens_js = tokens_value(ctx, tokens)?;
            let exec = execute_command_fn(ctx, cwd, timeout)?;
            call_hook(ctx, hook, (tokens_js, exec))
        })?;
        spec_from_fig_json(&json)
    }

    /// Fig `parserDirectives.alias` as a function: `(token, execute) => string`.
    pub fn alias(&self, hook_id: &str, token: &str, cwd: &str, timeout: Duration) -> Option<String> {
        let json = self.call_hook(hook_id, timeout, |ctx, hook| {
            let exec = execute_command_fn(ctx, cwd, timeout)?;
            call_hook(ctx, hook, (token, exec))
        })?;
        json_as_string(&json)
    }

    /// Function-form `getQueryTerm(searchTerm)`. Failure returns `None` so the
    /// caller can keep the whole search term, matching the WebView try/catch.
    pub fn get_query_term(&self, hook_id: &str, search_term: &str) -> Option<String> {
        let json = self.call_hook(hook_id, default_hook_budget(), |ctx, hook| {
            call_hook(ctx, hook, (search_term,))
        })?;
        json_as_string(&json)
    }

    /// Function-form generator `trigger(searchTerm, previousSearchTerm)`.
    pub fn trigger(&self, hook_id: &str, search_term: &str, previous: &str) -> Option<bool> {
        let json = self.call_hook(hook_id, default_hook_budget(), |ctx, hook| {
            call_hook(ctx, hook, (search_term, previous))
        })?;
        json.as_bool()
    }

    /// Function-form `loadSpec(token, execute) => spec`.
    pub fn load_spec(&self, hook_id: &str, token: &str, cwd: &str, timeout: Duration) -> Option<Spec> {
        let json = self.call_hook_rewrite(hook_id, timeout, |ctx, hook| {
            let exec = execute_command_fn(ctx, cwd, timeout)?;
            call_hook(ctx, hook, (token, exec))
        })?;
        spec_from_fig_json(&json)
    }

    /// `filterTemplateSuggestions(suggestions) => suggestions`.
    pub fn filter_template_suggestions(&self, hook_id: &str, suggestions: &[Suggestion]) -> Option<Vec<Suggestion>> {
        let payload = template_suggestions_json(suggestions);
        let json = self.call_hook(hook_id, default_hook_budget(), |ctx, hook| {
            let value = json_to_js(ctx, &payload)?;
            call_hook(ctx, hook, (value,))
        })?;
        suggestions_from_json(&json, false)
    }

    fn call_hook<F>(&self, hook_id: &str, budget: Duration, invoke: F) -> Option<JsonValue>
    where
        F: for<'js> FnOnce(&Ctx<'js>, Function<'js>) -> rquickjs::Result<JsValue<'js>>,
    {
        self.call_hook_inner(hook_id, budget, false, invoke)
    }

    fn call_hook_rewrite<F>(&self, hook_id: &str, budget: Duration, invoke: F) -> Option<JsonValue>
    where
        F: for<'js> FnOnce(&Ctx<'js>, Function<'js>) -> rquickjs::Result<JsValue<'js>>,
    {
        self.call_hook_inner(hook_id, budget, true, invoke)
    }

    fn call_hook_inner<F>(&self, hook_id: &str, budget: Duration, rewrite_helpers: bool, invoke: F) -> Option<JsonValue>
    where
        F: for<'js> FnOnce(&Ctx<'js>, Function<'js>) -> rquickjs::Result<JsValue<'js>>,
    {
        let source = self.hook_source(hook_id)?;
        let _deadline = HookDeadlineGuard::arm(budget.saturating_add(HOOK_DEADLINE_MARGIN));
        Self::with_inner(|inner| {
            let pending = inner.context.with(|ctx| {
                let hook = eval_hook(&ctx, &source).ok()?;
                let mut value = invoke(&ctx, hook).ok()?;
                if rewrite_helpers {
                    value = rewrite_filepaths_helpers(&ctx, value).ok()?;
                }
                if !is_thenable(&value) {
                    return Some(HookOutcome::Done(js_to_json(&ctx, value).ok()?));
                }
                start_await(&ctx, value).ok()?;
                Some(HookOutcome::Pending)
            })?;
            match pending {
                HookOutcome::Done(json) => Some(json),
                HookOutcome::Pending => {
                    for _ in 0..MAX_JOBS {
                        match inner.runtime.execute_pending_job() {
                            Ok(true) => {},
                            Ok(false) => break,
                            Err(_) => return None,
                        }
                    }
                    inner.context.with(|ctx| finish_await(&ctx).ok())
                },
            }
        })
        .flatten()
    }
}

/// The budget for hooks whose call site has no script timeout of its own
/// (`postProcess` result shaping and `script` command construction).
fn default_hook_budget() -> Duration {
    let ms = crate::generate::configured_script_timeout_ms();
    Duration::from_millis(u64::try_from(ms).unwrap_or(0)).max(Duration::from_millis(
        u64::try_from(DEFAULT_SCRIPT_TIMEOUT_MS).unwrap_or(5_000),
    ))
}

/// Arms [`HOOK_DEADLINE`] for one hook invocation and restores the previous
/// value on drop, so a `generateSpec` hook firing during the walk of another
/// hook's completion keeps the outer deadline intact.
struct HookDeadlineGuard {
    previous: Option<Instant>,
}

impl HookDeadlineGuard {
    fn arm(budget: Duration) -> Self {
        let previous = HOOK_DEADLINE.with(|cell| cell.replace(Some(Instant::now() + budget)));
        Self { previous }
    }
}

impl Drop for HookDeadlineGuard {
    fn drop(&mut self) {
        HOOK_DEADLINE.with(|cell| cell.set(self.previous));
    }
}

/// Time left before the running hook is interrupted. `None` when no hook
/// deadline is armed (e.g. host functions exercised directly by tests).
fn hook_time_remaining() -> Option<Duration> {
    HOOK_DEADLINE.with(|cell| {
        cell.get()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    })
}

enum HookOutcome {
    Done(JsonValue),
    Pending,
}

pub fn current() -> Option<(&'static JsHost, &'static str)> {
    ACTIVE.with(|cell| {
        cell.get().map(|active| {
            // SAFETY: pointers are only set for the `enter` stack frame and
            // callers must not store the references past that frame.
            unsafe { (&*active.host, &*active.cwd) }
        })
    })
}

/// Shell facts bound by the innermost [`JsHost::enter_with_context`] frame.
/// Falls back to an empty context outside a completion attempt.
pub fn current_shell() -> &'static ShellContext {
    ACTIVE.with(|cell| {
        // SAFETY: as in `current`, the pointer only lives for the `enter` frame.
        cell.get()
            .map_or(empty_shell_context(), |active| unsafe { &*active.shell })
    })
}

pub fn hook_path(dir: &Path, id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{safe}.js"))
}

/// Fig `runCachedGenerator`: `[cacheByDirectory ? cwd : undefined, cacheKey ||
/// fallback].toString()`. `fallback` is the joined token array for `custom`
/// generators and the serialized `executeCommand` input for scripts.
pub fn cache_key(cache_by_directory: bool, cwd: &str, cache_key: Option<&str>, fallback: &str) -> String {
    let second = cache_key.filter(|key| !key.is_empty()).unwrap_or(fallback);
    if cache_by_directory {
        format!("{cwd},{second}")
    } else {
        format!(",{second}")
    }
}

/// Fig `getScriptSuggestions` caches on `JSON.stringify(executeCommandInput)`
/// — the resolved command, its args and the cwd — never on the typed tokens,
/// so two generators that compute different commands cannot share an entry
/// and the same command in another directory does not either.
pub fn script_cache_fallback(command: &str, args: &[String], cwd: &str) -> String {
    serde_json::json!({ "command": command, "args": args, "cwd": cwd }).to_string()
}

/// Ceiling for each per-engine hook cache. Directory-keyed generators mint a
/// new entry per cwd, so a long desktop session would otherwise grow these
/// maps without bound. Wholesale clearing at the cap is fine: entries are
/// cheap to regenerate and the cap is far above one session's working set.
const MAX_CACHE_ENTRIES: usize = 512;

fn evict_at_cap<T>(cache: &mut HashMap<String, T>, key: &str) {
    if cache.len() >= MAX_CACHE_ENTRIES && !cache.contains_key(key) {
        cache.clear();
    }
}

pub fn cached_spec(host: &JsHost, cache_key: &str, run: impl FnOnce() -> Option<Spec>) -> Option<Spec> {
    {
        let cache = host.spec_cache.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(spec) = cache.get(cache_key) {
            return Some(spec.clone());
        }
    }
    let value = run()?;
    let mut cache = host.spec_cache.lock().unwrap_or_else(|err| err.into_inner());
    evict_at_cap(&mut cache, cache_key);
    cache.insert(cache_key.to_string(), value.clone());
    Some(value)
}

fn cache_get<T: Clone>(cache: &Mutex<HashMap<String, CacheEntry<T>>>, key: &str, policy: CachePolicy) -> Option<T> {
    let mut cache = cache.lock().unwrap_or_else(|err| err.into_inner());
    let expired = cache
        .get(key)
        .is_some_and(|entry| policy.ttl.is_some_and(|ttl| entry.fetched_at.elapsed() > ttl));
    if expired {
        cache.remove(key);
        return None;
    }
    cache.get(key).map(|entry| entry.value.clone())
}

fn cache_put<T>(cache: &Mutex<HashMap<String, CacheEntry<T>>>, key: String, value: T) {
    let mut cache = cache.lock().unwrap_or_else(|err| err.into_inner());
    evict_at_cap(&mut cache, &key);
    cache.insert(
        key,
        CacheEntry {
            value,
            fetched_at: Instant::now(),
        },
    );
}

/// Avoid leaking cache keys: store the optional key as owned on the policy.
struct OwnedCachePolicy {
    by_directory: bool,
    key: Option<String>,
    ttl: Option<Duration>,
}

/// Fig `runCachedGenerator` + `CacheEntry.entry`. A generator without a
/// `cache` block is uncached unless `beta.autocomplete.auto-cache` is on,
/// which gives it `{ strategy: "stale-while-revalidate", ttl: 1000 }`. With
/// a block, the TTL rules follow the JS arithmetic exactly: `max-age` with
/// no `ttl` compares against `NaN` and never expires, while
/// `stale-while-revalidate` (the default) defaults `maxAge` to 0 and
/// refetches on every turn — `cache: { cacheByDirectory: true }` alone is
/// therefore not a cache at all.
fn owned_cache_policy(arg: &ArgSpec) -> Option<OwnedCachePolicy> {
    let has_explicit = arg.cache_key.is_some()
        || arg.cache_by_directory.is_some()
        || arg.cache_ttl_ms.is_some()
        || arg.cache_strategy.is_some();
    if !has_explicit {
        let auto = fig_settings::settings::get_bool_or("beta.autocomplete.auto-cache", false);
        return auto.then(|| OwnedCachePolicy {
            by_directory: false,
            key: None,
            ttl: Some(Duration::from_millis(1_000)),
        });
    }
    let max_age = arg.cache_strategy.as_deref() == Some("max-age");
    let ttl = match arg.cache_ttl_ms {
        Some(ms) => Some(Duration::from_millis(u64::try_from(ms).unwrap_or(0))),
        None if max_age => None,
        None => Some(Duration::ZERO),
    };
    Some(OwnedCachePolicy {
        by_directory: arg.cache_by_directory.unwrap_or(false),
        key: arg.cache_key.clone(),
        ttl,
    })
}

/// Run `run` through the generator cache. `fallback_key` stands in for a
/// missing `cacheKey`: the token array for `custom` generators
/// ([`custom_cache_fallback`]) and the resolved command for scripts
/// ([`script_cache_fallback`]).
pub fn cached_suggestions(
    host: &JsHost,
    arg: &ArgSpec,
    cwd: &str,
    kind: &str,
    fallback_key: &str,
    run: impl FnOnce() -> Vec<Suggestion>,
) -> Vec<Suggestion> {
    run_cached(&host.suggestion_cache, arg, cwd, kind, fallback_key, run)
}

/// Script stdout through the generator cache; see [`JsHost::script_output_cache`].
pub fn cached_script_output(
    host: &JsHost,
    arg: &ArgSpec,
    cwd: &str,
    fallback_key: &str,
    run: impl FnOnce() -> String,
) -> String {
    run_cached(&host.script_output_cache, arg, cwd, "script", fallback_key, run)
}

fn run_cached<T: Clone>(
    cache: &Mutex<HashMap<String, CacheEntry<T>>>,
    arg: &ArgSpec,
    cwd: &str,
    kind: &str,
    fallback_key: &str,
    run: impl FnOnce() -> T,
) -> T {
    let Some(policy) = owned_cache_policy(arg) else {
        return run();
    };
    let key = format!(
        "{kind}:{}",
        cache_key(policy.by_directory, cwd, policy.key.as_deref(), fallback_key)
    );
    let lookup = CachePolicy { ttl: policy.ttl };
    if let Some(hit) = cache_get(cache, &key, lookup) {
        return hit;
    }
    let value = run();
    cache_put(cache, key, value.clone());
    value
}

/// Fig `runCachedGenerator` falls back to `tokenArray.join(" ")` for
/// generators that do not run a script.
pub fn custom_cache_fallback(tokens: &[String]) -> String {
    tokens.join(" ")
}

/// Drop every cached generator result and generated spec — the WebView's
/// `clear-cache` event (`generatorCache.clear()` + `clearSpecIndex()`).
pub fn clear_caches(host: &JsHost) {
    host.suggestion_cache
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clear();
    host.script_output_cache
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clear();
    host.spec_cache.lock().unwrap_or_else(|err| err.into_inner()).clear();
    host.sources.lock().unwrap_or_else(|err| err.into_inner()).clear();
}

#[derive(Debug, Clone)]
pub struct ScriptCommand {
    pub command: String,
    pub args: Vec<String>,
    pub timeout_ms: Option<i64>,
}

fn eval_hook<'js>(ctx: &Ctx<'js>, source: &str) -> rquickjs::Result<Function<'js>> {
    let trimmed = source.trim();
    let body = trimmed
        .strip_prefix("export default")
        .unwrap_or(trimmed)
        .trim()
        .trim_end_matches(';')
        .trim();
    ctx.eval::<Function<'_>, _>(format!("({body})"))
}

fn call_hook<'js, A>(ctx: &Ctx<'js>, hook: Function<'js>, args: A) -> rquickjs::Result<JsValue<'js>>
where
    A: rquickjs::function::IntoArgs<'js>,
{
    let _ = ctx;
    hook.call(args)
}

fn rewrite_filepaths_helpers<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> rquickjs::Result<JsValue<'js>> {
    let rewrite: Function<'_> = ctx.eval(include_str!("filepaths_rewrite.js"))?;
    rewrite.call((value,))
}

fn is_thenable(value: &JsValue<'_>) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.get::<_, Function<'_>>("then").is_ok())
}

fn start_await<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> rquickjs::Result<()> {
    let helper: Function<'_> = ctx.eval(
        r#"(function(value) {
  var box = { done: false, ok: true, value: undefined, error: undefined };
  Promise.resolve(value).then(
    function (resolved) { box.done = true; box.ok = true; box.value = resolved; },
    function (err) {
      box.done = true;
      box.ok = false;
      box.error = (err && err.message) ? String(err.message) : String(err);
    }
  );
  return box;
})"#,
    )?;
    let box_value: Object<'_> = helper.call((value,))?;
    ctx.globals().set("__ec_await_box", box_value)?;
    Ok(())
}

fn finish_await(ctx: &Ctx<'_>) -> rquickjs::Result<JsonValue> {
    let box_value: Object<'_> = ctx.globals().get("__ec_await_box")?;
    let done: bool = box_value.get("done")?;
    let ok: bool = box_value.get("ok")?;
    if !done || !ok {
        return Err(rquickjs::Error::Unknown);
    }
    let value: JsValue<'_> = box_value.get("value")?;
    js_to_json(ctx, value)
}

fn tokens_value<'js>(ctx: &Ctx<'js>, tokens: &[String]) -> rquickjs::Result<rquickjs::Array<'js>> {
    let array = rquickjs::Array::new(ctx.clone())?;
    for (index, token) in tokens.iter().enumerate() {
        array.set(index, token.as_str())?;
    }
    Ok(array)
}

fn execute_command_fn<'js>(ctx: &Ctx<'js>, cwd: &str, timeout: Duration) -> rquickjs::Result<Function<'js>> {
    let cwd = cwd.to_string();
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    Function::new(ctx.clone(), {
        move |ctx: Ctx<'js>, input: JsValue<'js>| -> rquickjs::Result<Object<'js>> {
            run_execute_command(&ctx, input, &cwd, timeout_ms)
        }
    })
}

fn custom_context<'js>(
    ctx: &Ctx<'js>,
    cwd: &str,
    search_term: &str,
    is_dangerous: bool,
) -> rquickjs::Result<Object<'js>> {
    let shell = current_shell();
    let object = Object::new(ctx.clone())?;
    object.set("currentWorkingDirectory", cwd)?;
    object.set("currentProcess", shell.current_process.as_str())?;
    object.set("sshPrefix", "")?;
    object.set("searchTerm", search_term)?;
    object.set("isDangerous", is_dangerous)?;
    let env = Object::new(ctx.clone())?;
    for (key, value) in shell.environment_variables.iter() {
        env.set(key.as_str(), value.as_str())?;
    }
    object.set("environmentVariables", env)?;
    Ok(object)
}

fn run_execute_command<'js>(
    ctx: &Ctx<'js>,
    input: JsValue<'js>,
    default_cwd: &str,
    default_timeout_ms: u64,
) -> rquickjs::Result<Object<'js>> {
    let parsed =
        parse_execute_input(&input, default_cwd, default_timeout_ms).map_err(|_parse| rquickjs::Error::Unknown)?;
    let mut timeout = Duration::from_millis(parsed.timeout_ms);
    // The interrupt handler cannot fire while this blocking call runs, so a
    // hook chaining subprocess calls has to hand each one only the time the
    // hook itself has left. Without this, one more 5s command scheduled right
    // before the deadline stretches the attempt well past its budget.
    if let Some(remaining) = hook_time_remaining() {
        if remaining.is_zero() {
            return Err(rquickjs::Error::Unknown);
        }
        timeout = timeout.min(remaining);
    }
    let result = process::execute_full(&parsed.command, &parsed.args, &parsed.cwd, &parsed.env, timeout);
    let output = Object::new(ctx.clone())?;
    match result {
        Ok(result) => {
            output.set("status", result.status)?;
            output.set("stdout", clean_output(&result.stdout))?;
            output.set("stderr", clean_output(&result.stderr))?;
        },
        Err(CommandError::TimedOut | CommandError::Failed) => return Err(rquickjs::Error::Unknown),
    }
    Ok(output)
}

struct ParsedCommand {
    command: String,
    args: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    timeout_ms: u64,
}

fn parse_execute_input(input: &JsValue<'_>, default_cwd: &str, default_timeout_ms: u64) -> Result<ParsedCommand, ()> {
    if let Some(command) = input.as_string().and_then(|value| value.to_string().ok()) {
        return Ok(ParsedCommand {
            command: "sh".into(),
            args: vec!["-c".into(), command],
            cwd: default_cwd.to_string(),
            env: Vec::new(),
            timeout_ms: default_timeout_ms.max(u64::try_from(DEFAULT_SCRIPT_TIMEOUT_MS).unwrap_or(5_000)),
        });
    }
    let object = input.as_object().ok_or(())?;
    let command: String = object.get("command").map_err(|_get| ())?;
    let args = match object.get::<_, JsValue<'_>>("args") {
        Ok(value) if value.is_array() => js_string_array(&value).unwrap_or_default(),
        _ => Vec::new(),
    };
    let cwd = match object.get::<_, String>("cwd") {
        Ok(cwd) if !cwd.is_empty() => cwd,
        _ => default_cwd.to_string(),
    };
    let timeout_ms = match object.get::<_, f64>("timeout") {
        Ok(timeout) if timeout.is_finite() && timeout > 0.0 => default_timeout_ms.max(timeout as u64),
        _ => default_timeout_ms,
    };
    let env = match object.get::<_, Object<'_>>("env") {
        Ok(env) => env.props::<String, String>().filter_map(|entry| entry.ok()).collect(),
        Err(_) => Vec::new(),
    };
    Ok(ParsedCommand {
        command,
        args,
        cwd,
        env,
        timeout_ms,
    })
}

fn js_string_array(value: &JsValue<'_>) -> Option<Vec<String>> {
    let array = value.as_array()?;
    let mut out = Vec::new();
    for item in array.iter::<String>() {
        out.push(item.ok()?);
    }
    Some(out)
}

pub fn clean_output(output: &str) -> String {
    output
        .replace("\r\n", "\n")
        .replace("\x1b[?25h", "")
        .trim_start_matches('\n')
        .trim_end_matches('\n')
        .to_string()
}

fn js_to_json<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> rquickjs::Result<JsonValue> {
    let stringify: Function<'_> = ctx.eval(
        r#"(function(value) {
  return JSON.stringify(value === undefined ? null : value);
})"#,
    )?;
    let text: rquickjs::String<'_> = stringify.call((value,))?;
    let text = text.to_string()?;
    serde_json::from_str(&text).map_err(|_json| rquickjs::Error::Unknown)
}

fn json_as_string(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(text) => Some(text.clone()),
        JsonValue::Number(number) => Some(number.to_string()),
        JsonValue::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn json_to_js<'js>(ctx: &Ctx<'js>, value: &JsonValue) -> rquickjs::Result<JsValue<'js>> {
    let text = serde_json::to_string(value).map_err(|_json| rquickjs::Error::Unknown)?;
    let parse: Function<'_> = ctx.eval("(function(text) { return JSON.parse(text); })")?;
    parse.call((text,))
}

fn template_suggestions_json(suggestions: &[Suggestion]) -> JsonValue {
    JsonValue::Array(
        suggestions
            .iter()
            .map(|suggestion| {
                let mut object = serde_json::Map::new();
                object.insert("name".into(), JsonValue::String(suggestion.name.clone()));
                object.insert("type".into(), JsonValue::String(suggestion.kind.clone()));
                if !suggestion.description.is_empty() {
                    object.insert("description".into(), JsonValue::String(suggestion.description.clone()));
                }
                if let Some(insert) = suggestion.insert_value.clone() {
                    object.insert("insertValue".into(), JsonValue::String(insert));
                }
                let mut context = serde_json::Map::new();
                context.insert(
                    "templateType".into(),
                    JsonValue::String(if suggestion.kind == "folder" {
                        "folders".into()
                    } else if suggestion.kind == "file" {
                        "filepaths".into()
                    } else if suggestion.kind == "special" {
                        "help".into()
                    } else {
                        "history".into()
                    }),
                );
                object.insert("context".into(), JsonValue::Object(context));
                JsonValue::Object(object)
            })
            .collect(),
    )
}

fn suggestions_from_json(value: &JsonValue, is_dangerous: bool) -> Option<Vec<Suggestion>> {
    let items = value.as_array()?;
    let mut out = Vec::new();
    for item in items {
        if let Some(suggestion) = suggestion_from_json(item, is_dangerous) {
            out.push(suggestion);
        }
    }
    Some(out)
}

fn suggestion_from_json(item: &JsonValue, is_dangerous: bool) -> Option<Suggestion> {
    if let Some(name) = item.as_str() {
        if name.is_empty() {
            return None;
        }
        return Some(
            Suggestion::new(name, "", "arg")
                .with_insert_value(name)
                .with_dangerous(is_dangerous),
        );
    }
    let object = item.as_object()?;
    let name = json_name(object.get("name")?)?;
    if name.is_empty() {
        return None;
    }
    let description = object
        .get("description")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_string();
    let kind = object
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("arg")
        .to_string();
    let insert = object
        .get("insertValue")
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned);
    let display = object
        .get("displayName")
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned);
    let icon = object.get("icon").and_then(JsonValue::as_str).map(ToOwned::to_owned);
    let hidden = object.get("hidden").and_then(JsonValue::as_bool).unwrap_or(false);
    let dangerous = is_dangerous || object.get("isDangerous").and_then(JsonValue::as_bool).unwrap_or(false);
    let priority = object.get("priority").and_then(JsonValue::as_i64);
    let mut suggestion = Suggestion::new(name.clone(), description, kind)
        .with_dangerous(dangerous)
        .with_meta(
            insert,
            display,
            None,
            object
                .get("shouldAddSpace")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
            hidden,
            priority,
            icon,
        );
    if suggestion.insert_value.is_none() {
        suggestion.insert_value = Some(name);
    }
    Some(suggestion)
}

fn json_name(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(name) if !name.is_empty() => Some(name.clone()),
        JsonValue::Array(names) => names.iter().find_map(|item| item.as_str().map(ToOwned::to_owned)),
        _ => None,
    }
}

fn script_command_from_json(value: &JsonValue) -> Option<ScriptCommand> {
    if let Some(command) = value.as_str() {
        if command.trim().is_empty() {
            return None;
        }
        return Some(ScriptCommand {
            command: "sh".into(),
            args: vec!["-c".into(), command.to_string()],
            timeout_ms: None,
        });
    }
    if let Some(parts) = value.as_array() {
        let strings: Vec<String> = parts
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect();
        let (command, args) = strings.split_first()?;
        if command.is_empty() {
            return None;
        }
        return Some(ScriptCommand {
            command: command.clone(),
            args: args.to_vec(),
            timeout_ms: None,
        });
    }
    let object = value.as_object()?;
    let command = object.get("command")?.as_str()?.to_string();
    if command.is_empty() {
        return None;
    }
    let args = object
        .get("args")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let timeout_ms = object.get("timeout").and_then(JsonValue::as_i64);
    Some(ScriptCommand {
        command,
        args,
        timeout_ms,
    })
}

pub fn spec_from_fig_json(value: &JsonValue) -> Option<Spec> {
    let object = value.as_object()?;
    let names = json_names(object.get("name")).or_else(|| json_names(object.get("names")))?;
    if names.is_empty() {
        return None;
    }
    Some(Spec {
        names,
        description: object
            .get("description")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string(),
        subcommands: json_array(object.get("subcommands"))
            .iter()
            .filter_map(spec_from_fig_json)
            .collect(),
        options: json_array(object.get("options"))
            .iter()
            .filter_map(option_from_fig_json)
            .collect(),
        persistent_options: json_array(object.get("persistentOptions"))
            .iter()
            .filter_map(option_from_fig_json)
            .collect(),
        args: args_from_fig(object.get("args")),
        additional_suggestions: json_array(object.get("additionalSuggestions"))
            .iter()
            .filter_map(seed_from_fig_json)
            .collect(),
        meta: meta_from_fig(object),
        load_spec: object
            .get("loadSpec")
            .and_then(JsonValue::as_str)
            .map(|path| LoadSpec::Path(path.to_string())),
        requires_subcommand: object.get("requiresSubcommand").and_then(JsonValue::as_bool),
        filter_strategy: filter_strategy_from_fig(object.get("filterStrategy")),
        parser_directives: parser_directives_from_fig(object.get("parserDirectives")),
        js_generate_spec: None,
        generate_spec_cache_key: object
            .get("generateSpecCacheKey")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        js_load_spec: object
            .get("jsLoadSpec")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
    })
}

pub fn merge_generated_spec(wrapper: &Spec, generated: Spec) -> Spec {
    let mut merged = generated;
    merged.names = if wrapper.names.is_empty() {
        merged.names
    } else {
        wrapper.names.clone()
    };
    if !wrapper.subcommands.is_empty() {
        merged.subcommands = merge_specs(wrapper.subcommands.clone(), merged.subcommands);
    }
    if !wrapper.options.is_empty() {
        merged.options = merge_options(wrapper.options.clone(), merged.options);
    }
    if !wrapper.persistent_options.is_empty() {
        merged.persistent_options = merge_options(wrapper.persistent_options.clone(), merged.persistent_options);
    }
    if !wrapper.args.is_empty() {
        merged.args = wrapper.args.clone();
    }
    if merged.description.is_empty() {
        merged.description = wrapper.description.clone();
    }
    merged
}

fn merge_specs(mut dest: Vec<Spec>, incoming: Vec<Spec>) -> Vec<Spec> {
    for spec in incoming {
        if let Some(existing) = dest
            .iter_mut()
            .find(|existing| spec.names.iter().any(|name| existing.has_name(name)))
        {
            *existing = spec;
        } else {
            dest.push(spec);
        }
    }
    dest
}

fn merge_options(mut dest: Vec<OptionSpec>, incoming: Vec<OptionSpec>) -> Vec<OptionSpec> {
    for option in incoming {
        if let Some(existing) = dest.iter_mut().find(|existing| {
            option
                .names
                .iter()
                .any(|name| existing.names.iter().any(|candidate| candidate == name))
        }) {
            *existing = option;
        } else {
            dest.push(option);
        }
    }
    dest
}

fn option_from_fig_json(value: &JsonValue) -> Option<OptionSpec> {
    let object = value.as_object()?;
    let names = json_names(object.get("name")).or_else(|| json_names(object.get("names")))?;
    if names.is_empty() {
        return None;
    }
    Some(OptionSpec {
        names,
        description: object
            .get("description")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string(),
        args: args_from_fig(object.get("args")),
        meta: meta_from_fig(object),
        load_spec: object
            .get("loadSpec")
            .and_then(JsonValue::as_str)
            .map(|path| LoadSpec::Path(path.to_string())),
        ..OptionSpec::default()
    })
}

fn args_from_fig(value: Option<&JsonValue>) -> Vec<ArgSpec> {
    match value {
        Some(JsonValue::Array(items)) => items.iter().filter_map(arg_from_fig_json).collect(),
        Some(item) => arg_from_fig_json(item).into_iter().collect(),
        None => Vec::new(),
    }
}

fn arg_from_fig_json(value: &JsonValue) -> Option<ArgSpec> {
    let object = value.as_object()?;
    let templates = object
        .get("template")
        .or_else(|| object.get("templates"))
        .map(templates_from_fig)
        .unwrap_or_default();
    let cache = cache_fields_from_fig(object);
    Some(ArgSpec {
        name: object
            .get("name")
            .and_then(|name| json_name(name).or_else(|| name.as_str().map(ToOwned::to_owned)))
            .unwrap_or_default(),
        description: object
            .get("description")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string(),
        templates,
        script: script_from_fig(object.get("script")),
        split_on: object.get("splitOn").and_then(JsonValue::as_str).map(ToOwned::to_owned),
        js_post_process: object
            .get("jsPostProcess")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        js_custom: object
            .get("jsCustom")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        js_script: object
            .get("jsScript")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        cache_key: cache.0,
        cache_by_directory: cache.1,
        cache_ttl_ms: cache.2,
        cache_strategy: cache.3,
        script_timeout_ms: object.get("scriptTimeout").and_then(JsonValue::as_i64),
        builtin: object.get("builtin").and_then(builtin_from_fig),
        builtins: json_array(object.get("builtins"))
            .iter()
            .filter_map(builtin_from_fig)
            .collect(),
        suggestions: json_array(object.get("suggestions"))
            .iter()
            .filter_map(seed_from_fig_json)
            .collect(),
        is_optional: object.get("isOptional").and_then(JsonValue::as_bool).unwrap_or(false),
        is_variadic: object.get("isVariadic").and_then(JsonValue::as_bool).unwrap_or(false),
        is_command: object.get("isCommand").and_then(JsonValue::as_bool).unwrap_or(false),
        is_script: object.get("isScript").and_then(JsonValue::as_bool).unwrap_or(false),
        is_module: object
            .get("isModule")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        load_spec: object.get("loadSpec").and_then(load_spec_from_fig),
        js_load_spec: object
            .get("jsLoadSpec")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        js_get_query_term: object
            .get("jsGetQueryTerm")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        debounce_ms: debounce_ms_from_fig(object),
        parser_directives: parser_directives_from_fig(object.get("parserDirectives")),
        generators: generators_from_fig(object),
        meta: meta_from_fig(object),
        ..ArgSpec::default()
    })
}

fn script_from_fig(value: Option<&JsonValue>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if let Some(script) = value.as_str() {
        let script = script.trim();
        if script.is_empty() {
            return Vec::new();
        }
        return vec!["sh".into(), "-c".into(), script.to_string()];
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect();
    }
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    let Some(command) = object.get("command").and_then(JsonValue::as_str) else {
        return Vec::new();
    };
    if command.trim().is_empty() {
        return Vec::new();
    }
    let mut script = vec![command.to_string()];
    if let Some(args) = object.get("args").and_then(JsonValue::as_array) {
        script.extend(args.iter().filter_map(|item| item.as_str().map(ToOwned::to_owned)));
    }
    script
}

fn load_spec_from_fig(value: &JsonValue) -> Option<LoadSpec> {
    if let Some(path) = value.as_str() {
        return Some(LoadSpec::Path(path.to_string()));
    }
    spec_from_fig_json(value).map(|spec| LoadSpec::Inline(Box::new(spec)))
}

fn builtin_from_fig(value: &JsonValue) -> Option<Builtin> {
    match value.as_str()? {
        "git-refs" => Some(Builtin::GitRefs),
        "git-branches" => Some(Builtin::GitBranches),
        "git-tags" => Some(Builtin::GitTags),
        "git-commits" => Some(Builtin::GitCommits),
        "git-remotes" => Some(Builtin::GitRemotes),
        "git-changed-files" => Some(Builtin::GitChangedFiles),
        "git-stashes" => Some(Builtin::GitStashes),
        "git-aliases" => Some(Builtin::GitAliases),
        "npm-scripts" => Some(Builtin::NpmScripts),
        "npm-deps" => Some(Builtin::NpmDeps),
        "cobra" => Some(Builtin::Cobra),
        _ => None,
    }
}

fn generator_from_fig(value: &JsonValue) -> Option<GeneratorSpec> {
    let object = value.as_object()?;
    let cache = cache_fields_from_fig(object);
    Some(GeneratorSpec {
        templates: object
            .get("template")
            .or_else(|| object.get("templates"))
            .map(templates_from_fig)
            .unwrap_or_default(),
        script: script_from_fig(object.get("script")),
        split_on: object.get("splitOn").and_then(JsonValue::as_str).map(ToOwned::to_owned),
        js_post_process: object
            .get("jsPostProcess")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        js_custom: object
            .get("jsCustom")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        js_script: object
            .get("jsScript")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        cache_key: cache.0,
        cache_by_directory: cache.1,
        cache_ttl_ms: cache.2,
        cache_strategy: cache.3,
        script_timeout_ms: object.get("scriptTimeout").and_then(JsonValue::as_i64),
        builtin: object.get("builtin").and_then(builtin_from_fig),
        get_query_term: object
            .get("getQueryTerm")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        js_get_query_term: object
            .get("jsGetQueryTerm")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        js_filter_template_suggestions: object
            .get("jsFilterTemplateSuggestions")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        trigger: object.get("trigger").and_then(trigger_from_fig),
        extensions: json_string_list(object.get("extensions")),
        equals: json_string_list(object.get("equals")),
        show_folders: object
            .get("showFolders")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        filter_folders: object.get("filterFolders").and_then(JsonValue::as_bool),
        file_priority: object.get("filePriority").and_then(JsonValue::as_i64),
        folder_priority: object.get("folderPriority").and_then(JsonValue::as_i64),
        root_directory: object
            .get("rootDirectory")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        matches: object.get("matches").and_then(JsonValue::as_str).map(ToOwned::to_owned),
        matches_flags: object
            .get("matchesFlags")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
    })
}

fn generators_from_fig(object: &serde_json::Map<String, JsonValue>) -> Vec<GeneratorSpec> {
    let mut out = Vec::new();
    if let Some(one) = object.get("generator")
        && let Some(generator) = generator_from_fig(one)
    {
        out.push(generator);
    }
    // Fig accepts either `generators: { … }` or `generators: [{ … }]`.
    match object.get("generators") {
        Some(JsonValue::Array(items)) => {
            for item in items {
                if let Some(generator) = generator_from_fig(item) {
                    out.push(generator);
                }
            }
        },
        Some(item) => {
            if let Some(generator) = generator_from_fig(item) {
                out.push(generator);
            }
        },
        None => {},
    }
    out
}

fn debounce_ms_from_fig(object: &serde_json::Map<String, JsonValue>) -> Option<i64> {
    if let Some(ms) = object.get("debounceMs").and_then(JsonValue::as_i64) {
        return (ms > 0).then_some(ms);
    }
    match object.get("debounce") {
        Some(JsonValue::Bool(true)) => Some(200),
        Some(JsonValue::Number(number)) => number.as_i64().filter(|ms| *ms > 0),
        _ => None,
    }
}

fn cache_fields_from_fig(
    object: &serde_json::Map<String, JsonValue>,
) -> (Option<String>, Option<bool>, Option<i64>, Option<String>) {
    let nested = object.get("cache").and_then(JsonValue::as_object);
    let cache_key = object
        .get("cacheKey")
        .and_then(JsonValue::as_str)
        .or_else(|| nested.and_then(|cache| cache.get("cacheKey").or_else(|| cache.get("key"))?.as_str()))
        .map(ToOwned::to_owned);
    let cache_by_directory = object
        .get("cacheByDirectory")
        .and_then(JsonValue::as_bool)
        .or_else(|| nested.and_then(|cache| cache.get("cacheByDirectory")?.as_bool()));
    let cache_ttl_ms = object
        .get("cacheTtl")
        .and_then(JsonValue::as_i64)
        .or_else(|| nested.and_then(|cache| cache.get("ttl")?.as_i64()));
    let cache_strategy = object
        .get("cacheStrategy")
        .and_then(JsonValue::as_str)
        .or_else(|| nested.and_then(|cache| cache.get("strategy")?.as_str()))
        .map(ToOwned::to_owned);
    (cache_key, cache_by_directory, cache_ttl_ms, cache_strategy)
}

fn trigger_from_fig(value: &JsonValue) -> Option<GeneratorTrigger> {
    if let Some(string) = value.as_str() {
        return Some(GeneratorTrigger {
            on: "string".into(),
            string: Some(JsonValue::String(string.to_string())),
            length: None,
            js_trigger: None,
        });
    }
    let object = value.as_object()?;
    Some(GeneratorTrigger {
        on: object
            .get("on")
            .and_then(JsonValue::as_str)
            .unwrap_or("change")
            .to_string(),
        string: object.get("string").cloned(),
        length: object.get("length").and_then(JsonValue::as_i64),
        js_trigger: object
            .get("jsTrigger")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
    })
}

fn seed_from_fig_json(value: &JsonValue) -> Option<SuggestionSeed> {
    if let Some(name) = value.as_str() {
        return Some(SuggestionSeed {
            names: vec![name.to_string()],
            ..SuggestionSeed::default()
        });
    }
    let object = value.as_object()?;
    let names = json_names(object.get("name")).or_else(|| json_names(object.get("names")))?;
    Some(SuggestionSeed {
        names,
        description: object
            .get("description")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string(),
        meta: meta_from_fig(object),
        ..SuggestionSeed::default()
    })
}

fn meta_from_fig(object: &serde_json::Map<String, JsonValue>) -> SuggestionMeta {
    SuggestionMeta {
        suggestion_type: object.get("type").and_then(JsonValue::as_str).map(ToOwned::to_owned),
        original_type: object
            .get("originalType")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        get_query_term: object
            .get("getQueryTerm")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        js_get_query_term: object
            .get("jsGetQueryTerm")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        insert_value: object
            .get("insertValue")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        display_name: object
            .get("displayName")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        separator_to_add: object
            .get("separatorToAdd")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        should_add_space: object.get("shouldAddSpace").and_then(JsonValue::as_bool),
        hidden: object.get("hidden").and_then(JsonValue::as_bool).unwrap_or(false),
        priority: object.get("priority").and_then(JsonValue::as_i64),
        icon: object.get("icon").and_then(JsonValue::as_str).map(ToOwned::to_owned),
        is_dangerous: object.get("isDangerous").and_then(JsonValue::as_bool).unwrap_or(false),
    }
}

fn templates_from_fig(value: &JsonValue) -> Vec<Template> {
    let items: Vec<&str> = match value {
        JsonValue::String(item) => vec![item.as_str()],
        JsonValue::Array(items) => items.iter().filter_map(JsonValue::as_str).collect(),
        _ => return Vec::new(),
    };
    items
        .into_iter()
        .filter_map(|item| match item {
            "filepaths" => Some(Template::Filepaths),
            "folders" => Some(Template::Folders),
            "history" => Some(Template::History),
            "help" => Some(Template::Help),
            _ => None,
        })
        .collect()
}

fn filter_strategy_from_fig(value: Option<&JsonValue>) -> Option<FilterStrategy> {
    match value.and_then(JsonValue::as_str) {
        Some("prefix") => Some(FilterStrategy::Prefix),
        Some("fuzzy") => Some(FilterStrategy::Fuzzy),
        Some("default") => Some(FilterStrategy::Default),
        _ => None,
    }
}

fn parser_directives_from_fig(value: Option<&JsonValue>) -> Option<ParserDirectives> {
    let object = value.and_then(JsonValue::as_object)?;
    Some(ParserDirectives {
        options_must_precede_arguments: object.get("optionsMustPrecedeArguments").and_then(JsonValue::as_bool),
        flags_are_posix_noncompliant: object.get("flagsArePosixNoncompliant").and_then(JsonValue::as_bool),
        option_arg_separators: object
            .get("optionArgSeparators")
            .and_then(JsonValue::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                    .collect()
            }),
        alias: object.get("alias").and_then(JsonValue::as_str).map(ToOwned::to_owned),
        js_alias: object.get("jsAlias").and_then(JsonValue::as_str).map(ToOwned::to_owned),
    })
}

fn json_names(value: Option<&JsonValue>) -> Option<Vec<String>> {
    let value = value?;
    if let Some(name) = value.as_str() {
        return Some(vec![name.to_string()]);
    }
    let items = value.as_array()?;
    let names: Vec<String> = items
        .iter()
        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

fn json_array(value: Option<&JsonValue>) -> &[JsonValue] {
    value.and_then(JsonValue::as_array).map_or(&[], Vec::as_slice)
}

fn json_string_list(value: Option<&JsonValue>) -> Vec<String> {
    match value {
        Some(JsonValue::String(item)) if !item.is_empty() => vec![item.clone()],
        Some(JsonValue::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(ToOwned::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    #[test]
    fn quickjs_can_eval_and_run_a_hook() {
        let runtime = Runtime::new().expect("runtime");
        let context = Context::full(&runtime).expect("context");
        context.with(|ctx| {
            let value: i32 = ctx.eval("1 + 2").expect("eval");
            assert_eq!(value, 3);
        });
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo_postProcess_0.js"),
            "export default function(out) { return [{ name: 'ok-' + out }]; }\n",
        )
        .unwrap();
        let host = JsHost::new(dir.path().to_path_buf());
        let rows = host
            .post_process("demo#postProcess#0", "row", &["demo".into()])
            .expect("hook");
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["ok-row"]
        );
    }

    #[test]
    fn post_process_can_call_console_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo_postProcess_0.js"),
            "export default function(out) { try { JSON.parse('not-json'); } catch (err) { console.error(err); } return [{ name: out }]; }\n",
        )
        .unwrap();
        let host = JsHost::new(dir.path().to_path_buf());
        let rows = host
            .post_process("demo#postProcess#0", "kept", &["demo".into()])
            .expect("hook");
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["kept"]
        );
    }

    #[test]
    fn clean_output_matches_webview() {
        assert_eq!(clean_output("\n\r\nhello\x1b[?25h\n\n"), "hello");
        assert_eq!(clean_output("a\r\nb"), "a\nb");
    }

    #[test]
    fn fig_json_keeps_singular_generator_string_trigger_and_debounce() {
        let spec = spec_from_fig_json(&serde_json::json!({
            "name": "demo",
            "args": {
                "generator": { "script": "printf hi", "trigger": "/" },
                "debounce": true,
                "cache": { "ttl": 1000, "strategy": "stale-while-revalidate", "key": "k" }
            }
        }))
        .expect("spec");
        let arg = &spec.args[0];
        assert_eq!(arg.debounce_ms, Some(200));
        assert_eq!(arg.cache_ttl_ms, Some(1000));
        assert_eq!(arg.cache_strategy.as_deref(), Some("stale-while-revalidate"));
        assert_eq!(arg.cache_key.as_deref(), Some("k"));
        assert_eq!(arg.generators.len(), 1);
        assert_eq!(arg.generators[0].script, vec!["sh", "-c", "printf hi"]);
        let trigger = arg.generators[0].trigger.as_ref().expect("trigger");
        assert_eq!(trigger.on, "string");
        assert_eq!(trigger.string.as_ref().and_then(JsonValue::as_str), Some("/"));
    }

    #[test]
    fn execute_command_reads_only_the_webview_fields() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo_custom_0.js"),
            "export default async function(tokens, exec, ctx) {\n  const extra = await exec({\n    command: 'printf',\n    args: ['ok'],\n    cwd: ctx.currentWorkingDirectory,\n    env: { EC_HOOK: '1' },\n    timeout: 1,\n    stdin: 'ignored',\n    shell: true\n  });\n  return [{ name: extra.stdout }];\n}\n",
        )
        .unwrap();
        let host = JsHost::new(dir.path().to_path_buf());
        let rows = host
            .custom(
                "demo#custom#0",
                &["demo".into()],
                "/",
                "",
                Duration::from_millis(5_000),
                false,
            )
            .expect("hook");
        assert_eq!(rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(), vec!["ok"]);
    }

    #[test]
    fn custom_hooks_see_the_shell_process_and_environment() {
        // The WebView handed generators a real `Fig.ShellContext`. Several
        // bundled specs branch on `context.environmentVariables`, so an empty
        // object silently changed their suggestions.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo_custom_0.js"),
            "export default function(tokens, exec, ctx) {\n  return [{ name: `${ctx.currentProcess}:${ctx.environmentVariables.EC_TEST}` }];\n}\n",
        )
        .unwrap();
        let host = JsHost::new(dir.path().to_path_buf());
        let shell = ShellContext {
            current_process: "/bin/zsh".into(),
            environment_variables: Arc::new(vec![("EC_TEST".into(), "on".into())]),
        };
        let rows = host
            .enter_with_context("/", &shell, || {
                host.custom(
                    "demo#custom#0",
                    &["demo".into()],
                    "/",
                    "",
                    Duration::from_millis(5_000),
                    false,
                )
            })
            .expect("hook");
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["/bin/zsh:on"]
        );
    }

    #[test]
    fn shell_context_is_empty_outside_a_completion_attempt() {
        assert!(current_shell().current_process.is_empty());
        assert!(current_shell().environment_variables.is_empty());
    }

    #[test]
    fn an_infinite_js_loop_is_interrupted_at_the_hook_deadline() {
        // Without the interrupt handler this hook wedged the completion
        // attempt until the 30s supervisor watchdog abandoned the thread —
        // and every retype of the same buffer wedged the next attempt too,
        // which the user experienced as completions never coming back.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo_custom_0.js"),
            "export default function() { for (;;) {} }\n",
        )
        .unwrap();
        let host = JsHost::new(dir.path().to_path_buf());
        let started = Instant::now();
        let rows = host.custom(
            "demo#custom#0",
            &["demo".into()],
            "/",
            "",
            Duration::from_millis(100),
            false,
        );
        assert!(rows.is_none(), "the spinning hook must fail, not hang");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "interrupted after {:?}, expected roughly budget + margin",
            started.elapsed()
        );
    }

    #[test]
    fn the_runtime_survives_an_interrupted_hook() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("spin_custom_0.js"),
            "export default function() { for (;;) {} }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("ok_custom_0.js"),
            "export default function() { return [{ name: 'alive' }]; }\n",
        )
        .unwrap();
        let host = JsHost::new(dir.path().to_path_buf());
        assert!(
            host.custom(
                "spin#custom#0",
                &["spin".into()],
                "/",
                "",
                Duration::from_millis(100),
                false
            )
            .is_none()
        );
        let rows = host
            .custom(
                "ok#custom#0",
                &["ok".into()],
                "/",
                "",
                Duration::from_millis(5_000),
                false,
            )
            .expect("the runtime must stay usable after an interrupt");
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            vec!["alive"]
        );
    }

    #[test]
    fn exec_calls_are_clamped_to_the_hook_deadline() {
        // The interrupt handler cannot fire inside a blocking subprocess
        // call, so a hook chaining `exec` invocations must hand each one only
        // its remaining budget. Two 5s sleeps under a ~100ms budget would
        // otherwise run for the full 10 seconds.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("slow_custom_0.js"),
            "export default async function(tokens, exec) {\n  try { await exec({ command: 'sleep', args: ['5'], timeout: 5000 }); } catch (e) {}\n  try { await exec({ command: 'sleep', args: ['5'], timeout: 5000 }); } catch (e) {}\n  return [{ name: 'done' }];\n}\n",
        )
        .unwrap();
        let host = JsHost::new(dir.path().to_path_buf());
        let started = Instant::now();
        let _rows = host.custom(
            "slow#custom#0",
            &["slow".into()],
            "/",
            "",
            Duration::from_millis(100),
            false,
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "exec calls ran for {:?}, expected them clamped to the budget",
            started.elapsed()
        );
    }

    #[test]
    fn cache_key_matches_js_tostring() {
        assert_eq!(cache_key(false, "/tmp", None, "env"), ",env");
        assert_eq!(cache_key(true, "/tmp", Some("env"), "ignored"), "/tmp,env");
        assert_eq!(cache_key(false, "/tmp", Some(""), "fallback"), ",fallback");
        assert_eq!(custom_cache_fallback(&["git".into(), "add".into()]), "git add");
    }

    #[test]
    fn script_cache_fallback_separates_commands_and_directories() {
        let pods = script_cache_fallback("kubectl", &["get".into(), "pods".into()], "/repo");
        let nodes = script_cache_fallback("kubectl", &["get".into(), "nodes".into()], "/repo");
        let elsewhere = script_cache_fallback("kubectl", &["get".into(), "pods".into()], "/other");
        assert_ne!(pods, nodes);
        assert_ne!(pods, elsewhere);
        assert_eq!(pods, r#"{"args":["get","pods"],"command":"kubectl","cwd":"/repo"}"#);
    }

    #[test]
    fn cache_policy_follows_fig_ttl_arithmetic() {
        let explicit = |ttl: Option<i64>, strategy: Option<&str>, by_dir: Option<bool>| ArgSpec {
            cache_ttl_ms: ttl,
            cache_strategy: strategy.map(str::to_owned),
            cache_by_directory: by_dir,
            ..ArgSpec::default()
        };
        // `stale-while-revalidate` with a TTL: served until it passes.
        let policy = owned_cache_policy(&explicit(Some(3_600_000), Some("stale-while-revalidate"), None)).unwrap();
        assert_eq!(policy.ttl, Some(Duration::from_millis(3_600_000)));
        // `max-age` with no TTL: `Date.now() > NaN` is false forever.
        let policy = owned_cache_policy(&explicit(None, Some("max-age"), None)).unwrap();
        assert_eq!(policy.ttl, None);
        // Default strategy with no TTL: `maxAge = 0`, refetched every turn.
        let policy = owned_cache_policy(&explicit(None, None, Some(true))).unwrap();
        assert_eq!(policy.ttl, Some(Duration::ZERO));
        assert!(policy.by_directory);
        let policy = owned_cache_policy(&explicit(None, Some("stale-while-revalidate"), None)).unwrap();
        assert_eq!(policy.ttl, Some(Duration::ZERO));
    }

    #[test]
    fn clear_caches_rereads_hook_source() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        fs::write(
            hooks.join("demo_custom_0.js"),
            "export default function() { return [{ name: 'before' }]; }\n",
        )
        .unwrap();
        let host = JsHost::new(hooks.clone());
        let first = host
            .enter("/", || {
                host.custom("demo#custom#0", &[], "/", "", Duration::from_secs(1), false)
            })
            .expect("first");
        assert_eq!(first[0].name, "before");
        fs::write(
            hooks.join("demo_custom_0.js"),
            "export default function() { return [{ name: 'after' }]; }\n",
        )
        .unwrap();
        let stale = host
            .enter("/", || {
                host.custom("demo#custom#0", &[], "/", "", Duration::from_secs(1), false)
            })
            .expect("stale");
        assert_eq!(stale[0].name, "before");
        clear_caches(&host);
        let fresh = host
            .enter("/", || {
                host.custom("demo#custom#0", &[], "/", "", Duration::from_secs(1), false)
            })
            .expect("fresh");
        assert_eq!(fresh[0].name, "after");
    }

    #[test]
    fn expired_entries_refetch_instead_of_serving_stale_rows() {
        let host = JsHost::new(std::env::temp_dir());
        let short = ArgSpec {
            cache_ttl_ms: Some(1),
            cache_strategy: Some("stale-while-revalidate".into()),
            ..ArgSpec::default()
        };
        // Same key, generous TTL: reads the entry the short policy wrote.
        let long = ArgSpec {
            cache_ttl_ms: Some(600_000),
            ..short.clone()
        };
        let first = cached_suggestions(&host, &short, "/tmp", "custom", "k", || {
            vec![Suggestion::new("first", "", "arg")]
        });
        assert_eq!(first[0].name, "first");
        std::thread::sleep(Duration::from_millis(5));
        let second = cached_suggestions(&host, &short, "/tmp", "custom", "k", || {
            vec![Suggestion::new("second", "", "arg")]
        });
        assert_eq!(
            second[0].name, "second",
            "an expired entry must hand back the fresh value"
        );
        let third = cached_suggestions(&host, &long, "/tmp", "custom", "k", || {
            vec![Suggestion::new("third", "", "arg")]
        });
        assert_eq!(third[0].name, "second", "the refetched value is cached again");
        clear_caches(&host);
        let fourth = cached_suggestions(&host, &long, "/tmp", "custom", "k", || {
            vec![Suggestion::new("fourth", "", "arg")]
        });
        assert_eq!(fourth[0].name, "fourth");
    }

    #[test]
    fn generate_spec_rewrites_live_filepaths_helpers_to_native_templates() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("demo_generateSpec_0.js"),
            r#"export default function () {
  const gen = {
    custom: async function (_tokens, exec) {
      const skip = [".DS_Store"];
      const { stdout } = await exec({ command: "ls", args: ["-1ApL"], cwd: "/x" });
      return String(stdout)
        .split("\n")
        .filter(Boolean)
        .filter((name) => skip.indexOf(name) === -1)
        .filter((name) => name.endsWith(".py") || name.endsWith("/"))
        .map((name) => ({
          name,
          type: name.endsWith("/") ? "folder" : "file",
          priority: name.endsWith(".py") ? 76 : undefined,
        }));
    },
  };
  return { name: "dyn", args: { name: "file", generators: gen } };
}
"#,
        )
        .unwrap();
        let host = JsHost::new(dir.path().to_path_buf());
        let spec = host
            .generate_spec(
                "demo#generateSpec#0",
                &["demo".into()],
                "/",
                Duration::from_millis(5_000),
            )
            .expect("spec");
        let generator = &spec.args[0].generators[0];
        assert_eq!(generator.templates, vec![Template::Filepaths]);
        assert_eq!(generator.extensions, vec!["py".to_string()]);
        assert_eq!(generator.file_priority, Some(76));
        assert_eq!(generator.get_query_term.as_deref(), Some("/"));
        assert!(generator.js_custom.is_none());
    }

    #[test]
    fn merge_keeps_wrapper_names_and_existing_args() {
        let wrapper = Spec {
            names: vec!["php".into()],
            args: vec![ArgSpec {
                name: "file".into(),
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let generated = Spec {
            names: vec!["generated".into()],
            subcommands: vec![Spec {
                names: vec!["artisan".into()],
                ..Spec::default()
            }],
            args: vec![ArgSpec {
                name: "other".into(),
                ..ArgSpec::default()
            }],
            ..Spec::default()
        };
        let merged = merge_generated_spec(&wrapper, generated);
        assert_eq!(merged.names, vec!["php"]);
        assert_eq!(merged.args[0].name, "file");
        assert!(merged.find_subcommand("artisan").is_some());
    }
}
