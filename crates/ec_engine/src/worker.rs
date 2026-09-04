use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::anyhow;
// Not `tokio::sync::oneshot`. The desktop awaits the reply on GPUI's foreground
// executor, which used to run nested inside one `tokio::Runtime::block_on` poll
// that hosted `NSApplication::run` and never returned. `block_on` fixes tokio's
// cooperative budget (128 units) for the life of that poll, and every tokio
// channel that resolves on that thread spends one. Once they were gone the
// 129th reply never resolved: `poll_proceed` reported Pending and woke the task
// at once, GPUI re-queued it onto the same main-queue drain, and the desktop sat
// at 100% CPU with the overlay frozen. `fig_desktop` now starts the UI loop
// outside `block_on`; this channel has no budget accounting at all, so the reply
// path stays correct even if a caller polls it from inside one again.
use futures::channel::oneshot;

use crate::ir::Registry;
use crate::rank::AcceptanceIndex;
use crate::runtime::{CompleteRequest, CompleteResult, Engine};

/// Thread-safe handle around the completion [`Engine`].
#[derive(Clone)]
pub struct EngineClient {
    tx: mpsc::Sender<Job>,
    acceptance: Arc<Mutex<AcceptanceIndex>>,
}

struct Job {
    kind: JobKind,
}

enum JobKind {
    Complete {
        request: CompleteRequest,
        reply: oneshot::Sender<anyhow::Result<CompleteResult>>,
    },
    RecordAcceptance {
        root_command: String,
        accepted_name: String,
        timestamp: u64,
    },
    /// `ec hook clear-autocomplete-cache`: drop every cached spec and
    /// generator result before the next completion runs.
    ClearCaches,
}

// A completion attempt can legitimately spend the legacy 5s script timeout
// inside a native generator. Keep the supervisor watchdog above that default
// (and above the 15s scriptTimeout used by several bundled specs) so the
// worker does not reset the engine while the generator is still within its
// configured budget.
const MIN_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Slack added to the user's script budget before the UI stops waiting, so a
/// generator that finishes right on its own deadline still gets rendered.
const UI_DEADLINE_MARGIN: Duration = Duration::from_secs(1);
/// Floor for the UI deadline, for the case where `scriptTimeout` is set to
/// something tiny. Below this the overlay would give up on completions the
/// engine was about to return anyway.
const MIN_UI_DEADLINE: Duration = Duration::from_secs(2);

/// Watchdog used by the normal engine client. A user script timeout above the
/// bundled-spec baseline extends the watchdog instead of being silently
/// truncated by it. Read this for every job so changing the setting does not
/// require restarting the desktop process.
pub fn engine_attempt_timeout() -> Duration {
    let configured_ms = fig_settings::settings::get_int("autocomplete.scriptTimeout")
        .ok()
        .flatten()
        .unwrap_or(crate::generate::DEFAULT_SCRIPT_TIMEOUT_MS);
    engine_attempt_timeout_for(configured_ms)
}

fn engine_attempt_timeout_for(configured_ms: i64) -> Duration {
    let configured = Duration::from_millis(u64::try_from(configured_ms).unwrap_or(0));
    MIN_ATTEMPT_TIMEOUT.max(configured.saturating_add(Duration::from_secs(5)))
}

/// How long the overlay waits for a completion before giving up on it.
///
/// Deliberately not [`engine_attempt_timeout`]. That one is the supervisor's
/// "has this worker thread wedged" floor and sits at 30s so a spec with a long
/// `scriptTimeout` is never killed mid-run. Reusing it for the UI meant a
/// single stuck generator pinned the `···` marker on screen for half a minute.
/// What the user is actually waiting on is their own script budget.
pub fn ui_completion_deadline() -> Duration {
    ui_completion_deadline_for(crate::generate::configured_script_timeout_ms())
}

fn ui_completion_deadline_for(configured_ms: i64) -> Duration {
    let configured = Duration::from_millis(u64::try_from(configured_ms).unwrap_or(0));
    MIN_UI_DEADLINE.max(configured.saturating_add(UI_DEADLINE_MARGIN))
}

#[derive(Debug)]
enum AttemptFailure {
    TimedOut,
    Panicked,
}

type AttemptResult = Result<(Engine, anyhow::Result<CompleteResult>), AttemptFailure>;

impl EngineClient {
    pub fn spawn(specs_dir: PathBuf) -> anyhow::Result<Self> {
        Self::spawn_supervised(specs_dir, None)
    }

    #[cfg(test)]
    fn spawn_with_timeout(specs_dir: PathBuf, attempt_timeout: Duration) -> anyhow::Result<Self> {
        Self::spawn_supervised(specs_dir, Some(attempt_timeout))
    }

    fn spawn_supervised(specs_dir: PathBuf, fixed_attempt_timeout: Option<Duration>) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Job>();
        let supervisor_specs_dir = specs_dir.clone();
        let acceptance = Arc::new(Mutex::new(AcceptanceIndex::load()));
        let worker_acceptance = acceptance.clone();

        thread::Builder::new()
            .name("ec-engine".into())
            .spawn(move || {
                // Initialize lazily for the first real request.  A malformed
                // or temporarily unavailable specs directory must not poison
                // this worker forever: the next request gets another chance
                // to build the registry after the caller repairs the input.
                let mut engine = None;
                // Pristine index kept aside so recovering from a timed-out
                // attempt does not re-walk the specs directory. It is cloned,
                // never handed out, so a poisoned attempt cannot corrupt it.
                let mut registry_template: Option<Registry> = None;
                while let Ok(first) = rx.recv() {
                    let first = match first.kind {
                        JobKind::RecordAcceptance {
                            root_command,
                            accepted_name,
                            timestamp,
                        } => {
                            record_acceptance(
                                &mut engine,
                                &worker_acceptance,
                                &root_command,
                                &accepted_name,
                                timestamp,
                            );
                            continue;
                        },
                        JobKind::ClearCaches => {
                            clear_caches(&mut engine, &mut registry_template);
                            continue;
                        },
                        JobKind::Complete { request, reply } => Job {
                            kind: JobKind::Complete { request, reply },
                        },
                    };
                    // Rapid typing queues many jobs; only the latest buffer matters.
                    // Acceptance records and cache clears are independent
                    // events and are applied while draining instead of being
                    // mistaken for completion work or dropped by latest-job
                    // coalescing.
                    let latest = drain_to_latest(&rx, first, |side_effect| match side_effect {
                        SideEffect::RecordAcceptance {
                            root_command,
                            accepted_name,
                            timestamp,
                        } => record_acceptance(
                            &mut engine,
                            &worker_acceptance,
                            &root_command,
                            &accepted_name,
                            timestamp,
                        ),
                        SideEffect::ClearCaches => clear_caches(&mut engine, &mut registry_template),
                    });
                    let JobKind::Complete { request, reply } = latest.kind else {
                        unreachable!("drain_to_latest returns a completion job");
                    };
                    // A caller can cancel its future while the job is waiting
                    // behind an in-flight attempt.  Do not initialize an
                    // engine or run a completion for a request nobody is
                    // waiting for anymore.
                    if reply.is_canceled() {
                        continue;
                    }
                    let current_engine = match engine.take() {
                        Some(engine) => engine,
                        None => {
                            match rebuild_engine(&supervisor_specs_dir, &mut registry_template, &worker_acceptance) {
                                Ok(engine) => engine,
                                Err(err) => {
                                    let _ = reply.send(Err(anyhow!("completion engine initialization failed: {err}")));
                                    continue;
                                },
                            }
                        },
                    };
                    let attempt_timeout = fixed_attempt_timeout.unwrap_or_else(engine_attempt_timeout);
                    let attempt_context = attempt_log_context(&request);
                    match run_engine_attempt(current_engine, request, attempt_timeout) {
                        Ok((next_engine, result)) => {
                            engine = Some(next_engine);
                            let _ = reply.send(result);
                        },
                        Err(failure) => {
                            // The default log filter is ERROR, so this is the
                            // only durable evidence of a wedged or crashed
                            // generator. Keep it at that level.
                            tracing::error!(
                                timeout_ms = attempt_timeout.as_millis() as u64,
                                context = %attempt_context,
                                failure = ?failure,
                                "completion attempt abandoned; engine reset"
                            );
                            let error = failure.error(attempt_timeout);
                            let _ = reply.send(Err(error));
                            // The timed-out/panicked attempt owns the old
                            // engine and may still be unwinding in its
                            // detached thread.  Drop it from the supervisor;
                            // the next request will retry initialization and
                            // can recover if the specs directory was repaired.
                            engine = None;
                        },
                    }
                }
            })
            .map_err(|err| anyhow!("spawn engine thread: {err}"))?;

        Ok(Self { tx, acceptance })
    }

    pub async fn complete(&self, request: CompleteRequest) -> anyhow::Result<CompleteResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job {
                kind: JobKind::Complete { request, reply },
            })
            .map_err(|_err| anyhow!("engine thread is gone"))?;
        rx.await.map_err(|_err| anyhow!("engine dropped the reply"))?
    }

    /// Block the current thread until completion returns. Use from CLI / tests only —
    /// the desktop overlay must call [`Self::complete`] so generators cannot freeze AppKit.
    pub fn complete_blocking(&self, request: CompleteRequest) -> anyhow::Result<CompleteResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job {
                kind: JobKind::Complete { request, reply },
            })
            .map_err(|_err| anyhow!("engine thread is gone"))?;
        futures::executor::block_on(rx).map_err(|_err| anyhow!("engine dropped the reply"))?
    }

    /// Queue a successful acceptance without participating in completion
    /// latest-job cancellation. Sending is unbounded and returns immediately;
    /// the worker applies the record between completion attempts and persists
    /// it best-effort. This keeps shell/UI acceptance independent of a slow or
    /// timed-out generator.
    pub fn record_acceptance(
        &self,
        root_command: impl Into<String>,
        accepted_name: impl Into<String>,
    ) -> anyhow::Result<()> {
        let root_command = root_command.into();
        let accepted_name = accepted_name.into();
        let timestamp = AcceptanceIndex::now_millis();
        {
            let mut acceptance = self.acceptance.lock().unwrap_or_else(|err| err.into_inner());
            let _ = acceptance.record_at(&root_command, &accepted_name, timestamp);
        }
        self.tx
            .send(Job {
                kind: JobKind::RecordAcceptance {
                    root_command,
                    accepted_name,
                    timestamp,
                },
            })
            .map_err(|_err| {
                self.acceptance.lock().unwrap_or_else(|err| err.into_inner()).persist();
                anyhow!("engine thread is gone")
            })
    }

    /// Forget every cached spec and generator result. Applied on the worker
    /// between completions, like an acceptance record, so it is never
    /// coalesced away by a newer completion request.
    pub fn clear_caches(&self) -> anyhow::Result<()> {
        self.tx
            .send(Job {
                kind: JobKind::ClearCaches,
            })
            .map_err(|_err| anyhow!("engine thread is gone"))
    }
}

/// A queued job that is not a completion: applied in order while draining.
enum SideEffect {
    RecordAcceptance {
        root_command: String,
        accepted_name: String,
        timestamp: u64,
    },
    ClearCaches,
}

fn clear_caches(engine: &mut Option<Engine>, registry_template: &mut Option<Registry>) {
    if let Some(engine) = engine.as_mut() {
        engine.clear_caches();
    }
    // The pristine index a reset engine is rebuilt from would otherwise keep
    // serving the spec files as they were at startup.
    *registry_template = None;
}

impl AttemptFailure {
    fn error(self, timeout: Duration) -> anyhow::Error {
        match self {
            Self::TimedOut => anyhow!(
                "completion attempt timed out after {}ms; engine reset",
                timeout.as_millis()
            ),
            Self::Panicked => anyhow!("completion attempt panicked; engine reset"),
        }
    }
}

/// Run one completion in an isolated thread.  The engine is returned with the
/// result so successful attempts retain the registry/frecency caches.  If the
/// attempt gets stuck, its thread (and the engine it owns) is intentionally
/// abandoned; the supervisor can then rebuild a fresh engine and accept the
/// next request.
fn run_engine_attempt(engine: Engine, request: CompleteRequest, timeout: Duration) -> AttemptResult {
    run_attempt(engine, request, timeout, |mut engine, request| {
        let result = engine.complete(request);
        (engine, result)
    })
}

fn run_attempt<F>(engine: Engine, request: CompleteRequest, timeout: Duration, complete: F) -> AttemptResult
where
    F: FnOnce(Engine, CompleteRequest) -> (Engine, anyhow::Result<CompleteResult>) + Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(1);
    let spawn_result = thread::Builder::new().name("ec-engine-attempt".into()).spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| complete(engine, request)));
        let message = match outcome {
            Ok(result) => Ok(result),
            Err(_) => Err(()),
        };
        let _ = tx.send(message);
    });
    if spawn_result.is_err() {
        return Err(AttemptFailure::Panicked);
    }

    match rx.recv_timeout(timeout) {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(())) | Err(mpsc::RecvTimeoutError::Disconnected) => Err(AttemptFailure::Panicked),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(AttemptFailure::TimedOut),
    }
}

/// Root command plus cwd, enough to identify which spec's generator wedged
/// without copying the whole edit buffer into the log.
fn attempt_log_context(request: &CompleteRequest) -> String {
    let root = request.buffer.split_whitespace().next().unwrap_or("");
    format!("root_command={root:?} cwd={:?}", request.cwd)
}

fn record_acceptance(
    engine: &mut Option<Engine>,
    acceptance: &Arc<Mutex<AcceptanceIndex>>,
    root_command: &str,
    accepted_name: &str,
    timestamp: u64,
) {
    if let Some(engine) = engine.as_mut() {
        engine.record_acceptance_at(root_command, accepted_name, timestamp);
    } else {
        // As in `Engine::record_acceptance_at`: persist a snapshot outside
        // the lock so a slow SQLite write cannot stall threads cloning the
        // index for ranking. This runs on the supervisor thread, where any
        // stall also delays every queued completion.
        let snapshot = {
            let mut index = acceptance.lock().unwrap_or_else(|err| err.into_inner());
            index
                .record_at(root_command, accepted_name, timestamp)
                .then(|| index.clone())
        };
        if let Some(snapshot) = snapshot {
            snapshot.persist();
        }
    }
}

/// Build an engine, indexing the specs directory only the first time.
///
/// A timed-out attempt keeps the engine it was given, so the supervisor has to
/// construct a fresh one. Re-reading the index on every reset made the first
/// completion after a wedged generator slow enough to show the loading marker
/// again, which reads to the user as the overlay never recovering.
fn rebuild_engine(
    specs_dir: &Path,
    template: &mut Option<Registry>,
    acceptance: &Arc<Mutex<AcceptanceIndex>>,
) -> anyhow::Result<Engine> {
    let registry = match template {
        Some(registry) => registry.clone(),
        None => {
            let registry = Engine::load_registry(specs_dir)?;
            template.get_or_insert(registry).clone()
        },
    };
    Ok(Engine::from_registry(specs_dir, registry, acceptance.clone()))
}

fn drain_to_latest<F>(rx: &mpsc::Receiver<Job>, first: Job, mut on_side_effect: F) -> Job
where
    F: FnMut(SideEffect),
{
    let mut job = first;
    while let Ok(next) = rx.try_recv() {
        match next.kind {
            JobKind::RecordAcceptance {
                root_command,
                accepted_name,
                timestamp,
            } => on_side_effect(SideEffect::RecordAcceptance {
                root_command,
                accepted_name,
                timestamp,
            }),
            JobKind::ClearCaches => on_side_effect(SideEffect::ClearCaches),
            JobKind::Complete { request, reply } => {
                // A newer request makes the current one irrelevant, but its
                // caller is still waiting on the reply channel. Finish it
                // explicitly instead of dropping the sender and leaving that
                // caller suspended indefinitely.
                let previous = std::mem::replace(&mut job.kind, JobKind::Complete { request, reply });
                if let JobKind::Complete { reply, .. } = previous {
                    let _ = reply.send(Err(anyhow!("completion request superseded by a newer request")));
                }
            },
        }
    }
    job
}

pub fn default_specs_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("EC_SPECS_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = bundled_specs_ir_dir() {
        return dir;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bundle/specs-ir")
}

fn bundled_specs_ir_dir() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    match exe.parent().filter(|dir| dir.ends_with("MacOS")) {
        Some(macos) => Ok(specs_ir_in_bundle(macos)),
        None => anyhow::bail!("not running from an app bundle"),
    }
}

/// `Easy Complete.app/Contents/MacOS` → `Contents/Resources/specs-ir`. Only the
/// compiled IR is bundled; the JS specs it was built from never enter the `.app`,
/// so pointing at `Resources/specs` would silently yield an empty registry.
fn specs_ir_in_bundle(macos_dir: &Path) -> PathBuf {
    macos_dir.join("../Resources/specs-ir")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion_job(request: CompleteRequest, reply: oneshot::Sender<anyhow::Result<CompleteResult>>) -> Job {
        Job {
            kind: JobKind::Complete { request, reply },
        }
    }

    #[test]
    fn the_ui_deadline_tracks_the_script_budget_not_the_supervisor_floor() {
        // The supervisor waits 30s before declaring a worker wedged. Showing
        // `···` for that long reads as a hang, so the overlay follows the
        // user's own script budget instead.
        assert_eq!(ui_completion_deadline_for(5_000), Duration::from_secs(6));
        assert!(ui_completion_deadline_for(5_000) < engine_attempt_timeout_for(5_000));
    }

    #[test]
    fn a_long_script_budget_extends_the_ui_deadline() {
        assert_eq!(ui_completion_deadline_for(15_000), Duration::from_secs(16));
    }

    #[test]
    fn a_tiny_script_budget_still_leaves_the_engine_time_to_answer() {
        assert_eq!(ui_completion_deadline_for(0), MIN_UI_DEADLINE);
        assert_eq!(ui_completion_deadline_for(-1), MIN_UI_DEADLINE);
    }

    #[test]
    fn rebuilding_the_engine_indexes_the_specs_directory_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("git.json"), r#"{"names":["git"]}"#).unwrap();
        std::fs::write(dir.path().join("index.json"), r#"{"files":{"git":"git.json"}}"#).unwrap();
        let acceptance = Arc::new(Mutex::new(AcceptanceIndex::default()));
        let mut template = None;

        rebuild_engine(dir.path(), &mut template, &acceptance).expect("first build");
        assert!(template.is_some(), "the index should be retained for reuse");

        // Deleting the index proves the rebuild came from the cached template
        // rather than the disk, which is what keeps the first completion after
        // a timed-out attempt fast.
        std::fs::remove_file(dir.path().join("index.json")).unwrap();
        let engine = rebuild_engine(dir.path(), &mut template, &acceptance).expect("rebuild without disk");
        assert!(!engine.registry().is_empty());
    }

    #[test]
    fn drain_keeps_the_newest_job() {
        let (tx, rx) = mpsc::channel::<Job>();
        let (reply_a, rx_a) = oneshot::channel();
        let (reply_b, rx_b) = oneshot::channel();
        let (reply_c, _rx_c) = oneshot::channel();
        tx.send(completion_job(
            CompleteRequest {
                buffer: "gi".into(),
                ..CompleteRequest::default()
            },
            reply_a,
        ))
        .unwrap();
        tx.send(completion_job(
            CompleteRequest {
                buffer: "git".into(),
                ..CompleteRequest::default()
            },
            reply_b,
        ))
        .unwrap();
        tx.send(completion_job(
            CompleteRequest {
                buffer: "git ".into(),
                ..CompleteRequest::default()
            },
            reply_c,
        ))
        .unwrap();
        let first = rx.recv().unwrap();
        let latest = drain_to_latest(&rx, first, |_| unreachable!("no side effects in this test"));
        let JobKind::Complete { request, .. } = latest.kind else {
            unreachable!("latest job should be a completion")
        };
        assert_eq!(request.buffer, "git ");
        assert!(rx.try_recv().is_err());

        let error_a = futures::executor::block_on(rx_a)
            .expect("superseded request should receive a reply")
            .expect_err("superseded request should fail explicitly");
        assert_eq!(error_a.to_string(), "completion request superseded by a newer request");
        let error_b = futures::executor::block_on(rx_b)
            .expect("superseded request should receive a reply")
            .expect_err("superseded request should fail explicitly");
        assert_eq!(error_b.to_string(), "completion request superseded by a newer request");
    }

    #[test]
    fn acceptance_jobs_survive_completion_coalescing() {
        let (tx, rx) = mpsc::channel::<Job>();
        let (reply_a, _rx_a) = oneshot::channel();
        let (reply_b, _rx_b) = oneshot::channel();
        tx.send(completion_job(CompleteRequest::default(), reply_a)).unwrap();
        tx.send(Job {
            kind: JobKind::RecordAcceptance {
                root_command: "git".into(),
                accepted_name: "status".into(),
                timestamp: 123,
            },
        })
        .unwrap();
        tx.send(completion_job(
            CompleteRequest {
                buffer: "git ".into(),
                ..CompleteRequest::default()
            },
            reply_b,
        ))
        .unwrap();

        let mut records = Vec::new();
        let latest = drain_to_latest(&rx, rx.recv().unwrap(), |side_effect| match side_effect {
            SideEffect::RecordAcceptance {
                root_command,
                accepted_name,
                ..
            } => records.push((root_command, accepted_name)),
            SideEffect::ClearCaches => unreachable!("no cache clear in this test"),
        });
        assert_eq!(records, vec![("git".into(), "status".into())]);
        let JobKind::Complete { request, .. } = latest.kind else {
            unreachable!("latest job should be a completion");
        };
        assert_eq!(request.buffer, "git ");
    }

    #[test]
    fn spawn_completes_on_a_plain_thread() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let engine = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let result = engine
            .complete_blocking(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("complete");
        assert!(
            result.suggestions.iter().any(|s| s.name == "status"),
            "{:?}",
            result.suggestions
        );
    }

    fn counting_script(count: &std::path::Path) -> String {
        format!("printf 'src\\n'; printf x >> '{}'", count.display())
    }

    /// Watchdog for the reset tests. The request after a reset runs on a
    /// fresh engine that still has to load shell history, so this needs
    /// real headroom over that on a loaded test host; 100ms did not have it.
    const WATCHDOG_UNDER_TEST: Duration = Duration::from_secs(1);

    #[test]
    fn generator_session_survives_attempt_threads() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let spec = serde_json::json!({
            "names": ["demo"],
            "args": [{
                "name": "slot",
                "script": ["sh", "-c", counting_script(&count)],
                "splitOn": "\n",
                "debounceMs": 200
            }]
        });
        std::fs::write(dir.path().join("demo.json"), spec.to_string()).unwrap();
        let engine = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let cwd = dir.path().display().to_string();
        let request = CompleteRequest {
            buffer: "demo ".into(),
            cwd,
            include_history: false,
            ..CompleteRequest::default()
        };
        let first = engine.complete_blocking(request.clone()).expect("first");
        assert!(
            first.pending_generators,
            "debounce should delay the first run: {first:?}"
        );
        assert!(!count.exists() || std::fs::read_to_string(&count).unwrap().is_empty());

        let second = engine.complete_blocking(request).expect("follow-up");
        assert!(
            second.suggestions.iter().any(|row| row.name == "src"),
            "{:?}",
            second.suggestions
        );
        assert_eq!(std::fs::read_to_string(&count).unwrap().matches('x').count(), 1);
        assert!(!second.pending_generators);
    }

    #[test]
    fn clear_caches_rereads_specs_and_forgets_generator_results() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let spec = |names: &[&str]| {
            serde_json::json!({
                "names": ["demo"],
                "subcommands": names.iter().map(|name| serde_json::json!({"names": [name]})).collect::<Vec<_>>(),
                "args": [{
                    "name": "slot",
                    "script": ["sh", "-c", counting_script(&count)],
                    "splitOn": "\n",
                    "cacheTtl": 60000
                }]
            })
        };
        std::fs::write(dir.path().join("demo.json"), spec(&["before"]).to_string()).unwrap();
        let engine = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let request = CompleteRequest {
            buffer: "demo ".into(),
            cwd: dir.path().display().to_string(),
            include_history: false,
            ..CompleteRequest::default()
        };
        let first = engine.complete_blocking(request.clone()).expect("first");
        assert!(first.suggestions.iter().any(|row| row.name == "before"), "{first:?}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().matches('x').count(), 1);

        // Edit the spec on disk; a cached spec and a cached script result
        // would both hide the change.
        std::fs::write(dir.path().join("demo.json"), spec(&["after"]).to_string()).unwrap();
        let stale = engine.complete_blocking(request.clone()).expect("stale");
        assert!(stale.suggestions.iter().any(|row| row.name == "before"), "{stale:?}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().matches('x').count(), 1);

        engine.clear_caches().expect("clear");
        let fresh = engine.complete_blocking(request).expect("fresh");
        assert!(fresh.suggestions.iter().any(|row| row.name == "after"), "{fresh:?}");
        assert!(fresh.suggestions.iter().all(|row| row.name != "before"), "{fresh:?}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().matches('x').count(), 2);
    }

    #[test]
    fn trailing_space_and_typed_token_share_generator_session_across_threads() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let script = counting_script(&count);
        let spec = serde_json::json!({
            "names": ["git"],
            "subcommands": [
                {"names": ["add"], "args": [{"name": "pathspec", "script": ["sh", "-c", script.clone()], "splitOn": "\n"}]},
                {"names": ["rm"], "args": [{"name": "pathspec", "script": ["sh", "-c", script], "splitOn": "\n"}]}
            ]
        });
        std::fs::write(dir.path().join("git.json"), spec.to_string()).unwrap();
        let engine = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let cwd = dir.path().display().to_string();
        let after_space = engine
            .complete_blocking(CompleteRequest {
                buffer: "git add ".into(),
                cwd: cwd.clone(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("git add ");
        let while_typing = engine
            .complete_blocking(CompleteRequest {
                buffer: "git add s".into(),
                cwd: cwd.clone(),
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("git add s");
        let other_command = engine
            .complete_blocking(CompleteRequest {
                buffer: "git rm ".into(),
                cwd,
                include_history: false,
                ..CompleteRequest::default()
            })
            .expect("git rm ");
        assert!(
            after_space.suggestions.iter().any(|row| row.name == "src"),
            "{after_space:?}"
        );
        assert!(
            while_typing.suggestions.iter().any(|row| row.name == "src"),
            "{while_typing:?}"
        );
        assert!(
            other_command.suggestions.iter().any(|row| row.name == "src"),
            "{other_command:?}"
        );
        assert_eq!(std::fs::read_to_string(&count).unwrap().matches('x').count(), 2);
    }

    #[test]
    fn supervisor_retries_engine_initialization_after_specs_are_repaired() {
        let dir = tempfile::tempdir().unwrap();
        // Registry::load parses index.json during Engine::new, so an invalid
        // index gives us a deterministic initialization failure before any
        // completion attempt starts.
        std::fs::write(dir.path().join("index.json"), b"{").unwrap();
        let client = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");

        let first = client.complete_blocking(CompleteRequest {
            buffer: "git ".into(),
            cwd: dir.path().display().to_string(),
            ..CompleteRequest::default()
        });
        let error = first.expect_err("the malformed registry should fail initialization");
        assert!(
            error.to_string().contains("completion engine initialization failed"),
            "{error}"
        );

        // Repair the directory after the first request.  A permanently
        // failed supervisor would return the old initialization error again;
        // a retrying supervisor should build the registry and complete.
        std::fs::write(dir.path().join("index.json"), r#"{"files":{"git":"git.json"}}"#).unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let second = client
            .complete_blocking(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("the repaired registry should be retried");
        assert!(second.suggestions.iter().any(|suggestion| suggestion.name == "status"));
    }

    #[test]
    fn cancelled_latest_job_is_detected_before_running_an_attempt() {
        let (tx, rx) = mpsc::channel::<Job>();
        let (reply, caller) = oneshot::channel();
        tx.send(completion_job(CompleteRequest::default(), reply)).unwrap();
        drop(caller);

        let job = drain_to_latest(&rx, rx.recv().unwrap(), |_| {
            unreachable!("no side effects in this test")
        });
        let JobKind::Complete { reply, .. } = job.kind else {
            unreachable!("job should be a completion")
        };
        assert!(
            reply.is_canceled(),
            "a cancelled caller must be observable before execution"
        );
    }

    /// Reproduces the desktop's shape exactly: a tokio `block_on` whose poll
    /// never returns, and inside it a foreign executor driving `complete()`
    /// futures to completion by hand. tokio's budget for that poll is 128 and
    /// is never refilled, so a `tokio::sync::oneshot` reply stops resolving on
    /// the 129th round and wakes itself forever instead — the livelock that
    /// pinned the desktop at 100% CPU after a few hours of typing.
    #[test]
    fn replies_keep_resolving_after_tokio_exhausts_its_budget_on_the_polling_thread() {
        use std::task::{Context, Poll, Waker};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let client = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let request = CompleteRequest {
            buffer: "git ".into(),
            cwd: dir.path().display().to_string(),
            ..CompleteRequest::default()
        };

        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            // Well past tokio's 128-unit budget. Each round polls the reply
            // future directly, never yielding back to `block_on`.
            for round in 0..200u32 {
                let mut future = Box::pin(client.complete(request.clone()));
                // A livelocked reply never resolves, so a wall-clock bound
                // catches it; a spin count did not survive a loaded test
                // host, where the first round also pays for the engine's
                // history load.
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                let result = loop {
                    match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
                        Poll::Ready(result) => break result,
                        Poll::Pending => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "round {round}: the reply future stopped making progress on this thread"
                            );
                            thread::yield_now();
                        },
                    }
                };
                let result = result.expect("completion");
                assert!(result.suggestions.iter().any(|s| s.name == "status"), "round {round}");
            }
        });
    }

    #[test]
    fn supervisor_resets_after_a_timeout_before_serving_the_next_request() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hang.json"),
            r#"{"names":["hang"],"args":[{"script":["sleep","5"]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();

        let client = EngineClient::spawn_with_timeout(dir.path().to_path_buf(), WATCHDOG_UNDER_TEST).expect("spawn");
        let first = client.complete_blocking(CompleteRequest {
            buffer: "hang ".into(),
            cwd: dir.path().display().to_string(),
            ..CompleteRequest::default()
        });
        let error = first.expect_err("the hanging attempt should time out");
        assert!(error.to_string().contains("timed out"), "{error}");

        let second = client
            .complete_blocking(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("the reset engine should serve the next request");
        assert!(second.suggestions.iter().any(|suggestion| suggestion.name == "status"));
    }

    #[test]
    fn a_reset_after_a_timeout_survives_the_specs_directory_going_bad() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hang.json"),
            r#"{"names":["hang"],"args":[{"script":["sleep","5"]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("index.json"),
            r#"{"files":{"git":"git.json","hang":"hang.json"}}"#,
        )
        .unwrap();

        let client = EngineClient::spawn_with_timeout(dir.path().to_path_buf(), WATCHDOG_UNDER_TEST).expect("spawn");
        let first = client.complete_blocking(CompleteRequest {
            buffer: "hang ".into(),
            cwd: dir.path().display().to_string(),
            ..CompleteRequest::default()
        });
        assert!(
            first
                .expect_err("the hanging attempt should time out")
                .to_string()
                .contains("timed out")
        );

        // The reset rebuilds from the index cached at startup, so corrupting
        // the directory afterwards cannot take completions down with it. A
        // directory that never loaded still retries — see
        // `supervisor_retries_engine_initialization_after_specs_are_repaired`.
        std::fs::write(dir.path().join("index.json"), b"{").unwrap();
        let second = client
            .complete_blocking(CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("the cached index should carry the reset engine");
        assert!(second.suggestions.iter().any(|suggestion| suggestion.name == "status"));
    }

    #[test]
    fn timed_out_attempt_can_be_followed_by_a_fresh_engine_attempt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let timed_out = run_attempt(
            engine,
            CompleteRequest::default(),
            Duration::from_millis(10),
            |engine, _request| {
                thread::sleep(Duration::from_millis(80));
                (engine, Ok(CompleteResult::default()))
            },
        );
        assert!(matches!(timed_out, Err(AttemptFailure::TimedOut)));

        // The timed-out attempt owns the old engine, so the supervisor's reset
        // path creates a new one before accepting the next request.
        let fresh_engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let (_, result) = run_engine_attempt(
            fresh_engine,
            CompleteRequest {
                buffer: "git ".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            },
            Duration::from_secs(1),
        )
        .expect("fresh attempt should not be blocked by the old one");
        let result = result.expect("fresh completion should succeed");
        assert!(result.suggestions.iter().any(|suggestion| suggestion.name == "status"));
    }

    #[test]
    fn default_watchdog_does_not_cut_off_the_legacy_five_second_generator_budget() {
        assert!(engine_attempt_timeout() >= Duration::from_secs(5));
        assert_eq!(engine_attempt_timeout_for(5_000), Duration::from_secs(30));
        assert_eq!(engine_attempt_timeout_for(60_000), Duration::from_secs(65));
        assert_eq!(engine_attempt_timeout_for(-1), Duration::from_secs(30));
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let result = run_attempt(
            engine,
            CompleteRequest::default(),
            engine_attempt_timeout(),
            |engine, _request| {
                // This is deliberately just beyond the old three-second
                // watchdog while remaining below the legacy 5s default.
                thread::sleep(Duration::from_millis(3_200));
                (engine, Ok(CompleteResult::default()))
            },
        )
        .expect("the attempt should remain within the default watchdog");
        assert!(result.1.is_ok());
    }

    #[test]
    fn panicked_attempt_returns_an_error_and_does_not_block_reset() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let panicked = run_attempt(
            engine,
            CompleteRequest::default(),
            Duration::from_secs(1),
            |_engine, _request| panic!("simulated completion panic"),
        );
        assert!(matches!(panicked, Err(AttemptFailure::Panicked)));

        let fresh_engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let (_, result) = run_engine_attempt(fresh_engine, CompleteRequest::default(), Duration::from_secs(1))
            .expect("fresh attempt should run after a panic");
        assert!(result.is_ok());
    }

    #[test]
    fn app_bundle_uses_specs_ir_not_js_specs() {
        let macos = Path::new("/Applications/easy-complete.app/Contents/MacOS");
        let dir = specs_ir_in_bundle(macos);
        assert_eq!(dir.file_name().unwrap(), "specs-ir");
        assert_eq!(
            dir,
            PathBuf::from("/Applications/easy-complete.app/Contents/MacOS/../Resources/specs-ir")
        );
    }
}
