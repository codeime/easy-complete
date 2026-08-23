use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::anyhow;
use tokio::sync::oneshot;

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
    ClearCaches {
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
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
                // One attempt thread, created on the first complete. QuickJS
                // lives in thread-local storage, so spawning a thread per
                // keystroke rebuilt the runtime on every overlay key. Timeout
                // and panic drop this sender so the wedged thread (and its
                // engine) is abandoned; the next request spawns a replacement.
                let mut attempt_worker = None;
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
                        JobKind::ClearCaches { reply } => {
                            if let Some(engine) = engine.as_mut() {
                                engine.clear_caches();
                            }
                            let _ = reply.send(Ok(()));
                            continue;
                        },
                        JobKind::Complete { request, reply } => Job {
                            kind: JobKind::Complete { request, reply },
                        },
                    };
                    // Rapid typing queues many jobs; only the latest buffer matters.
                    // Acceptance records and cache clears are independent events
                    // and are applied while draining instead of being mistaken
                    // for completion work or dropped by latest-job coalescing.
                    let mut pending_clears = Vec::new();
                    let latest = drain_to_latest(
                        &rx,
                        first,
                        |root_command, accepted_name, timestamp| {
                            record_acceptance(
                                &mut engine,
                                &worker_acceptance,
                                &root_command,
                                &accepted_name,
                                timestamp,
                            );
                        },
                        |reply| pending_clears.push(reply),
                    );
                    if !pending_clears.is_empty() {
                        if let Some(engine) = engine.as_mut() {
                            engine.clear_caches();
                        }
                        for reply in pending_clears {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    let JobKind::Complete { request, reply } = latest.kind else {
                        unreachable!("drain_to_latest returns a completion job");
                    };
                    // A caller can cancel its future while the job is waiting
                    // behind an in-flight attempt.  Do not initialize an
                    // engine or run a completion for a request nobody is
                    // waiting for anymore.
                    if reply.is_closed() {
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
                    match dispatch_engine_attempt(&mut attempt_worker, current_engine, request, attempt_timeout) {
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
        rx.blocking_recv().map_err(|_err| anyhow!("engine dropped the reply"))?
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

    /// Drop generateSpec / generator caches on the supervisor. A complete that
    /// is already on the attempt thread finishes against the old cache; the
    /// next job sees an empty one. Does not rebuild the spec IR index.
    pub async fn clear_caches(&self) -> anyhow::Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job {
                kind: JobKind::ClearCaches { reply },
            })
            .map_err(|_err| anyhow!("engine thread is gone"))?;
        rx.await.map_err(|_err| anyhow!("engine dropped the reply"))?
    }

    /// Block the current thread until caches are cleared. CLI / tests only.
    pub fn clear_caches_blocking(&self) -> anyhow::Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job {
                kind: JobKind::ClearCaches { reply },
            })
            .map_err(|_err| anyhow!("engine thread is gone"))?;
        rx.blocking_recv().map_err(|_err| anyhow!("engine dropped the reply"))?
    }
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

/// One job on the long-lived attempt thread. The engine travels with the
/// request so a timeout can drop this sender and abandon both.
struct AttemptJob {
    engine: Engine,
    request: CompleteRequest,
    reply: mpsc::SyncSender<Result<(Engine, anyhow::Result<CompleteResult>), ()>>,
}

fn spawn_attempt_worker() -> Option<mpsc::Sender<AttemptJob>> {
    let (tx, rx) = mpsc::channel::<AttemptJob>();
    thread::Builder::new()
        .name("ec-engine-attempt".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut engine = job.engine;
                    let result = engine.complete(job.request);
                    (engine, result)
                }));
                match outcome {
                    Ok(result) => {
                        let _ = job.reply.send(Ok(result));
                    },
                    Err(_) => {
                        let _ = job.reply.send(Err(()));
                        // Leave the QuickJS thread-local behind. A panicked
                        // runtime is not reused for the next keystroke.
                        break;
                    },
                }
            }
        })
        .ok()?;
    Some(tx)
}

/// Send one completion to the persistent attempt thread. Timeout or panic
/// drops `worker` so the wedged thread cannot block the next request.
fn dispatch_engine_attempt(
    worker: &mut Option<mpsc::Sender<AttemptJob>>,
    engine: Engine,
    request: CompleteRequest,
    timeout: Duration,
) -> AttemptResult {
    if worker.is_none() {
        *worker = spawn_attempt_worker();
    }
    let sender = worker.clone().ok_or(AttemptFailure::Panicked)?;
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    let job = AttemptJob {
        engine,
        request,
        reply: reply_tx,
    };
    if sender.send(job).is_err() {
        *worker = None;
        return Err(AttemptFailure::Panicked);
    }
    match reply_rx.recv_timeout(timeout) {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(())) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            *worker = None;
            Err(AttemptFailure::Panicked)
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Dropping the sender is what abandons the thread: the worker is
            // still inside `complete`, and when it next `recv`s the channel
            // is gone. Do not queue another job on this sender.
            *worker = None;
            Err(AttemptFailure::TimedOut)
        },
    }
}

/// Run one completion in an isolated thread.  Tests use this to pin watchdog
/// timeout/panic isolation without going through the supervisor. Production
/// completions go through [`dispatch_engine_attempt`] so the attempt thread
/// stays warm across keystrokes.
#[cfg(test)]
fn run_engine_attempt(engine: Engine, request: CompleteRequest, timeout: Duration) -> AttemptResult {
    run_attempt(engine, request, timeout, |mut engine, request| {
        let result = engine.complete(request);
        (engine, result)
    })
}

#[cfg(test)]
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

fn drain_to_latest<A, C>(rx: &mpsc::Receiver<Job>, first: Job, mut on_acceptance: A, mut on_clear: C) -> Job
where
    A: FnMut(String, String, u64),
    C: FnMut(oneshot::Sender<anyhow::Result<()>>),
{
    let mut job = first;
    while let Ok(next) = rx.try_recv() {
        match next.kind {
            JobKind::RecordAcceptance {
                root_command,
                accepted_name,
                timestamp,
            } => on_acceptance(root_command, accepted_name, timestamp),
            JobKind::ClearCaches { reply } => on_clear(reply),
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
    for dir in installed_specs_ir_candidates() {
        if dir.is_dir() {
            return dir;
        }
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

/// Prefix layout for a Linux install. Must stay equal to
/// `fig_util::consts::linux::PACKAGE_NAME` (`easy-complete`); this crate
/// does not depend on `fig_util`.
#[cfg_attr(not(unix), allow(dead_code))]
fn linux_share_specs_ir() -> PathBuf {
    PathBuf::from("/usr/share/easy-complete/specs-ir")
}

fn specs_ir_beside_prefix_bin(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    Some(bin.join("../share/easy-complete/specs-ir"))
}

/// Windows zip (F6) and a portable Linux tree: `specs-ir/` next to the binary.
fn specs_ir_beside_exe(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.join("specs-ir"))
}

fn installed_specs_ir_candidates() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = specs_ir_beside_exe(&exe) {
            dirs.push(dir);
        }
        if let Some(dir) = specs_ir_beside_prefix_bin(&exe) {
            dirs.push(dir);
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        if !local.is_empty() {
            dirs.push(PathBuf::from(local).join("easy-complete/specs-ir"));
        }
    }
    #[cfg(unix)]
    {
        if let Ok(xdg_home) = std::env::var("XDG_DATA_HOME") {
            if !xdg_home.is_empty() {
                dirs.push(PathBuf::from(xdg_home).join("easy-complete/specs-ir"));
            }
        } else if let Ok(home) = std::env::var("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/easy-complete/specs-ir"));
        }
        let xdg_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        for dir in xdg_dirs.split(':').filter(|dir| !dir.is_empty()) {
            dirs.push(PathBuf::from(dir).join("easy-complete/specs-ir"));
        }
        dirs.push(linux_share_specs_ir());
    }
    dirs
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
        let latest = drain_to_latest(
            &rx,
            first,
            |_, _, _| unreachable!("no acceptance in this test"),
            |_| unreachable!("no cache clear in this test"),
        );
        let JobKind::Complete { request, .. } = latest.kind else {
            unreachable!("latest job should be a completion")
        };
        assert_eq!(request.buffer, "git ");
        assert!(rx.try_recv().is_err());

        let error_a = rx_a
            .blocking_recv()
            .expect("superseded request should receive a reply")
            .expect_err("superseded request should fail explicitly");
        assert_eq!(error_a.to_string(), "completion request superseded by a newer request");
        let error_b = rx_b
            .blocking_recv()
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
        let latest = drain_to_latest(
            &rx,
            rx.recv().unwrap(),
            |command, name, _| {
                records.push((command, name));
            },
            |_| unreachable!("no cache clear in this test"),
        );
        assert_eq!(records, vec![("git".into(), "status".into())]);
        let JobKind::Complete { request, .. } = latest.kind else {
            unreachable!("latest job should be a completion");
        };
        assert_eq!(request.buffer, "git ");
    }

    #[test]
    fn cache_clear_jobs_survive_completion_coalescing() {
        let (tx, rx) = mpsc::channel::<Job>();
        let (reply_a, _rx_a) = oneshot::channel();
        let (reply_clear, rx_clear) = oneshot::channel();
        let (reply_b, _rx_b) = oneshot::channel();
        tx.send(completion_job(CompleteRequest::default(), reply_a)).unwrap();
        tx.send(Job {
            kind: JobKind::ClearCaches { reply: reply_clear },
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

        let mut cleared = 0usize;
        let latest = drain_to_latest(
            &rx,
            rx.recv().unwrap(),
            |_, _, _| unreachable!("no acceptance in this test"),
            |reply| {
                cleared += 1;
                let _ = reply.send(Ok(()));
            },
        );
        assert_eq!(cleared, 1);
        rx_clear.blocking_recv().expect("clear reply").expect("clear ok");
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
        let again = engine
            .complete_blocking(CompleteRequest {
                buffer: "git sta".into(),
                cwd: dir.path().display().to_string(),
                ..CompleteRequest::default()
            })
            .expect("second complete on the same client");
        assert!(
            again.suggestions.iter().any(|s| s.name == "status"),
            "{:?}",
            again.suggestions
        );
    }

    #[cfg(unix)]
    #[test]
    fn client_clear_caches_drops_generate_spec_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("hooks")).unwrap();
        std::fs::write(
            dir.path().join("hooks/demo_generateSpec_0.js"),
            "export default async(e,t)=>{await t({command:\"sh\",args:[\"-c\",\"printf x >> runs\"]});return {name:\"demo\",subcommands:[{name:\"alpha\"}]};}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("demo.json"),
            r#"{"names":["demo"],"jsGenerateSpec":"demo#generateSpec#0"}"#,
        )
        .unwrap();
        let client = EngineClient::spawn(dir.path().to_path_buf()).expect("spawn");
        let cwd = dir.path().display().to_string();
        let request = CompleteRequest {
            buffer: "demo a".into(),
            cwd,
            ..CompleteRequest::default()
        };
        client.complete_blocking(request.clone()).expect("first complete");
        client.complete_blocking(request.clone()).expect("cached complete");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("runs"))
                .unwrap_or_default()
                .matches('x')
                .count(),
            1,
            "the second complete must hit the generateSpec cache"
        );
        client.clear_caches_blocking().expect("clear");
        client.complete_blocking(request).expect("complete after clear");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("runs"))
                .unwrap_or_default()
                .matches('x')
                .count(),
            2,
            "ClearCaches must drop generateSpec so the next complete re-runs the hook"
        );
    }

    #[test]
    fn dispatch_keeps_the_attempt_sender_across_two_completes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();
        let cwd = dir.path().display().to_string();
        let mut worker = None;
        let first_engine = Engine::new(dir.path().to_path_buf()).unwrap();
        let (engine, first) = dispatch_engine_attempt(
            &mut worker,
            first_engine,
            CompleteRequest {
                buffer: "git ".into(),
                cwd: cwd.clone(),
                ..CompleteRequest::default()
            },
            Duration::from_secs(5),
        )
        .expect("first dispatch");
        assert!(
            worker.is_some(),
            "a successful complete must keep the attempt thread for the next keystroke"
        );
        assert!(
            first
                .expect("first complete")
                .suggestions
                .iter()
                .any(|s| s.name == "status")
        );
        let (_engine, second) = dispatch_engine_attempt(
            &mut worker,
            engine,
            CompleteRequest {
                buffer: "git sta".into(),
                cwd,
                ..CompleteRequest::default()
            },
            Duration::from_secs(5),
        )
        .expect("second dispatch");
        assert!(worker.is_some(), "the second complete must reuse the same sender");
        assert!(
            second
                .expect("second complete")
                .suggestions
                .iter()
                .any(|s| s.name == "status")
        );
    }

    #[test]
    fn supervisor_reuses_the_attempt_thread_on_the_happy_path() {
        let src = include_str!("worker.rs");
        let start = src.find("fn spawn_supervised").expect("spawn_supervised");
        let rest = &src[start..];
        let end = rest
            .find("pub async fn complete(")
            .expect("complete method follows spawn_supervised");
        let body = &rest[..end];
        assert!(
            body.contains("attempt_worker") && body.contains("dispatch_engine_attempt"),
            "the supervisor must keep one attempt thread and dispatch to it"
        );
        assert!(
            !body.contains("run_engine_attempt"),
            "run_engine_attempt spawns a one-shot thread; the overlay path must not use it"
        );
        let dispatch = {
            let start = src.find("fn dispatch_engine_attempt").expect("dispatch_engine_attempt");
            let rest = &src[start..];
            let brace = rest.find('{').expect("fn body");
            let bytes = rest.as_bytes();
            let mut depth = 0i32;
            let mut end = rest.len();
            for (i, &b) in bytes.iter().enumerate().skip(brace) {
                match b {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = i;
                            break;
                        }
                    },
                    _ => {},
                }
            }
            &rest[..=end]
        };
        assert!(
            dispatch.contains("*worker = None") && dispatch.contains("RecvTimeoutError::Timeout"),
            "a timed-out dispatch must drop the attempt sender so the wedged thread cannot take the next job"
        );
        assert!(
            !dispatch.contains("thread::Builder"),
            "dispatch must not spawn a thread per completion"
        );
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

        let job = drain_to_latest(
            &rx,
            rx.recv().unwrap(),
            |_, _, _| unreachable!("no acceptance in this test"),
            |_| unreachable!("no cache clear in this test"),
        );
        let JobKind::Complete { reply, .. } = job.kind else {
            unreachable!("job should be a completion")
        };
        assert!(
            reply.is_closed(),
            "a cancelled caller must be observable before execution"
        );
    }

    #[test]
    fn supervisor_resets_after_a_timeout_before_serving_the_next_request() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hang.json"),
            r#"{"names":["hang"],"args":[{"script":["sleep","1"]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("git.json"),
            r#"{"names":["git"],"subcommands":[{"names":["status"]}]}"#,
        )
        .unwrap();

        let client =
            EngineClient::spawn_with_timeout(dir.path().to_path_buf(), Duration::from_millis(100)).expect("spawn");
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
            r#"{"names":["hang"],"args":[{"script":["sleep","1"]}]}"#,
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

        let client =
            EngineClient::spawn_with_timeout(dir.path().to_path_buf(), Duration::from_millis(100)).expect("spawn");
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

    #[test]
    fn linux_share_prefix_matches_package_name() {
        assert_eq!(
            linux_share_specs_ir(),
            PathBuf::from("/usr/share/easy-complete/specs-ir")
        );
    }

    #[test]
    fn linux_prefix_bin_looks_beside_share() {
        assert_eq!(
            specs_ir_beside_prefix_bin(Path::new("/usr/local/bin/easy-complete")).as_deref(),
            Some(Path::new("/usr/local/bin/../share/easy-complete/specs-ir"))
        );
    }

    #[test]
    fn windows_zip_layout_looks_beside_the_exe() {
        // Do not parse `C:\...` on Linux: backslashes are not separators.
        let mut exe = PathBuf::new();
        exe.push("easy-complete");
        exe.push("ec.exe");
        assert_eq!(
            specs_ir_beside_exe(&exe),
            Some(PathBuf::from("easy-complete").join("specs-ir"))
        );
        assert_eq!(
            specs_ir_beside_exe(Path::new("/opt/easy-complete/ec")),
            Some(PathBuf::from("/opt/easy-complete/specs-ir"))
        );
    }
}
