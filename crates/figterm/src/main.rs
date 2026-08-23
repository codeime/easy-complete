#[cfg(target_os = "linux")]
mod cleanup;
pub mod cli;
mod event_handler;
pub mod history;
pub mod input;
pub mod interceptor;
pub mod ipc;
pub mod logger;
mod message;
pub mod pty;
pub mod term;
pub mod update;

use std::env;
#[cfg(unix)]
use std::ffi::{CString, OsStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use alacritty_terminal::Term;
use alacritty_terminal::ansi::Processor;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::{ShellState, SizeInfo};
use anyhow::{Context as _, Result, anyhow};
use bytes::BytesMut;
use cfg_if::cfg_if;
use clap::Parser;
use cli::Cli;
use fig_log::{LogArgs, initialize_logging};
use fig_os_shim::{Context, Env};
use fig_proto::local::{self, EnvironmentVariable, TerminalCursorCoordinates};
use fig_proto::remote::Hostbound;
use fig_proto::remote_hooks::{hook_to_message, new_edit_buffer_hook};
use fig_settings::state;
use fig_util::env_var::{Q_LOG_LEVEL, Q_SHELL, Q_TERM, QTERM_SESSION_ID};
use fig_util::process_info::{Pid, PidExt};
use fig_util::{PRODUCT_NAME, PTY_BINARY_NAME, directories, terminal::current_terminal};
use flume::{Receiver, Sender};
#[cfg(unix)]
use nix::unistd::execvp;
use portable_pty::PtySize;
use tokio::io::{self, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::{runtime, select};
use tracing::{debug, error, info, trace, warn};

use crate::event_handler::EventHandler;
use crate::input::{InputEvent, KeyCode, KeyCodeEncodeModes, KeyboardEncoding, Modifiers};
use crate::interceptor::KeyInterceptor;
use crate::ipc::{spawn_figterm_ipc, spawn_remote_ipc};
use crate::message::{process_figterm_message, process_remote_message};
#[cfg(unix)]
use crate::pty::unix::open_pty;
#[cfg(windows)]
use crate::pty::win::open_pty;
use crate::pty::{AsyncMasterPtyExt, CommandBuilder};
use crate::term::{SystemTerminal, Terminal};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const BUFFER_SIZE: usize = 16384;
/// Must stay 1. See the comment at the `Term::new` call in `figterm_main`.
const TERM_SCROLLBACK_LINES: usize = 1;

static INSERT_ON_NEW_CMD: Mutex<Option<(String, bool, bool)>> = Mutex::new(None);
static INSERTION_LOCKED_AT: RwLock<Option<SystemTime>> = RwLock::new(None);
static EXPECTED_BUFFER: Mutex<String> = Mutex::new(String::new());

static SHELL_ENVIRONMENT_VARIABLES: Mutex<Vec<EnvironmentVariable>> = Mutex::new(Vec::new());
static SHELL_ALIAS: Mutex<Option<String>> = Mutex::new(None);

pub(crate) fn recover_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

pub(crate) fn recover_rwlock_read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|err| err.into_inner())
}

pub(crate) fn recover_rwlock_write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|err| err.into_inner())
}
/// Bumped by `UpdateShellContext` (`ec _ pre-cmd` at prompt). Edit-buffer
/// frames send env/alias only when this changes, so the desktop session
/// learns about a just-finished `export` without cloning env on every key.
static SHELL_CONTEXT_EPOCH: AtomicU64 = AtomicU64::new(0);
static LAST_SENT_SHELL_CONTEXT_EPOCH: AtomicU64 = AtomicU64::new(u64::MAX);

pub(crate) fn note_shell_context_updated() {
    SHELL_CONTEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
}

static USER_ENABLED_SHELLS: LazyLock<Vec<String>> = LazyLock::new(|| {
    fig_settings::state::get("user.enabled-shells")
        .ok()
        .flatten()
        .unwrap_or_default()
});

static HOSTNAME: LazyLock<Option<String>> = LazyLock::new(hostname);

fn hostname() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { nix::libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc != 0 {
            return None;
        }
        let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        std::str::from_utf8(&buf[..nul]).ok().map(str::to_owned)
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").ok()
    }
}

pub enum MainLoopEvent {
    Insert {
        insert: Vec<u8>,
        unlock: bool,
        bracketed: bool,
        execute: bool,
    },
    UnlockInterception,
    SetImmediateMode(bool),
    PromptSSH {
        uuid: String,
        remote_host: String,
    },
    SetCsiU,
    UnsetCsiU,
}

/// Prompt / preexec / postexec / intercepted-key frames carry the full
/// context. Edit-buffer ticks use [`edit_buffer_context`] so they do not
/// clone env, aliases, or the parent-terminal lookup on every keystroke.
fn shell_state_to_context(shell_state: &ShellState) -> local::ShellContext {
    local::ShellContext {
        pid: shell_state.local_context.pid,
        ttys: shell_state.local_context.tty.clone(),
        process_name: shell_state.local_context.shell.clone(),
        shell_path: shell_state
            .local_context
            .shell_path
            .clone()
            .map(|path| path.display().to_string()),
        wsl_distro: shell_state.local_context.wsl_distro.clone(),
        current_working_directory: cwd_string(shell_state),
        session_id: shell_state.local_context.session_id.clone(),
        terminal: cached_parent_terminal(),
        hostname: cached_host_label(shell_state.local_context.username.as_deref()),
        environment_variables: recover_mutex(&SHELL_ENVIRONMENT_VARIABLES).clone(),
        qterm_version: Some(env!("CARGO_PKG_VERSION").into()),
        preexec: Some(shell_state.preexec),
        osc_lock: Some(shell_state.osc_lock),
        alias: recover_mutex(&SHELL_ALIAS).clone(),
    }
}

/// OSC 7 can move cwd between prompts. Env/alias ride along only after
/// `UpdateShellContext`; `process_name` / `shell_path` stay on every frame
/// so a keystroke before the first Prompt still selects the right history.
fn edit_buffer_context(shell_state: &ShellState, include_environment: bool) -> local::ShellContext {
    local::ShellContext {
        current_working_directory: cwd_string(shell_state),
        process_name: shell_state.local_context.shell.clone(),
        shell_path: shell_state
            .local_context
            .shell_path
            .as_ref()
            .map(|path| path.display().to_string()),
        environment_variables: if include_environment {
            recover_mutex(&SHELL_ENVIRONMENT_VARIABLES).clone()
        } else {
            Vec::new()
        },
        alias: if include_environment {
            recover_mutex(&SHELL_ALIAS).clone()
        } else {
            None
        },
        ..Default::default()
    }
}

fn pending_shell_context_epoch() -> Option<u64> {
    let epoch = SHELL_CONTEXT_EPOCH.load(Ordering::Relaxed);
    (LAST_SENT_SHELL_CONTEXT_EPOCH.load(Ordering::Relaxed) != epoch).then_some(epoch)
}

fn mark_shell_context_sent(epoch: u64) {
    LAST_SENT_SHELL_CONTEXT_EPOCH.store(epoch, Ordering::Relaxed);
}

fn cwd_string(shell_state: &ShellState) -> Option<String> {
    shell_state
        .local_context
        .current_working_directory
        .as_ref()
        .map(|cwd| cwd.display().to_string())
}

fn cached_parent_terminal() -> Option<String> {
    current_terminal().map(|terminal| terminal.to_string())
}

fn cached_host_label(username: Option<&str>) -> Option<String> {
    static LABEL: OnceLock<String> = OnceLock::new();
    if let Some(existing) = LABEL.get() {
        return Some(existing.clone());
    }
    let label = username.and_then(|user| HOSTNAME.as_deref().map(|host| format!("{user}@{host}")))?;
    Some(LABEL.get_or_init(|| label).clone())
}

#[allow(clippy::needless_return)]
fn get_cursor_coordinates(terminal: &dyn Terminal) -> Option<TerminalCursorCoordinates> {
    cfg_if! {
        if #[cfg(target_os = "windows")] {
            use term::cast;

            let coordinate = terminal.get_cursor_coordinate().ok()?;
            let screen_size = terminal.get_screen_size().ok()?;
            return Some(TerminalCursorCoordinates {
                x: cast(coordinate.cols).ok()?,
                y: cast(coordinate.rows).ok()?,
                xpixel: cast(screen_size.xpixel).ok()?,
                ypixel: cast(screen_size.ypixel).ok()?,
            });
        } else {
            let _terminal = terminal;
            return None;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn _should_install_remote_ssh_integration(
    uuid: String,
    remote_host: String,
    main_loop_tx: Sender<MainLoopEvent>,
    remote_receiver: Receiver<fig_proto::remote::Clientbound>,
    remote_sender: Sender<Hostbound>,
    term: &Term<EventHandler>,
    pty_master: &mut Box<dyn crate::pty::AsyncMasterPty + Send + Sync>,
    key_interceptor: &mut KeyInterceptor,
) -> Option<bool> {
    use fig_proto::remote::clientbound;

    let remote_install_setting = fig_settings::settings::get_string_or("ssh.remote-prompt", "ask".into());
    if remote_install_setting == "never" {
        return Some(false);
    }

    let key = format!("ssh.remote-prompt.disable-host.{remote_host}");
    let disable_host = fig_settings::state::get_bool_or(key, false);
    if disable_host {
        return Some(false);
    }

    let prompt_timeout: u64 = fig_settings::settings::get_int_or("ssh.remote-prompt.timeout", 2000)
        .try_into()
        .unwrap_or(2000);

    // Wait for child ssh session to connect to local desktop instance.
    let got_child_connection = tokio::time::timeout(tokio::time::Duration::from_millis(prompt_timeout), async {
        loop {
            if let Ok(msg) = remote_receiver.recv_async().await {
                if let Some(clientbound::Packet::NotifyChildSessionStarted(clientbound::NotifyChildSessionStarted {
                    parent_id,
                })) = msg.packet
                {
                    if parent_id == uuid {
                        return true;
                    }
                } else {
                    process_remote_message(
                        msg,
                        main_loop_tx.clone(),
                        remote_sender.clone(),
                        term,
                        pty_master,
                        key_interceptor,
                    )
                    .await
                    .ok();
                }
            }
        }
    })
    .await
    .is_ok();

    if got_child_connection {
        return Some(false);
    }

    if remote_install_setting == "always" {
        return Some(true);
    }

    None
}

fn can_send_edit_buffer<T>(term: &Term<T>) -> bool
where
    T: EventListener,
{
    let shell_enabled = ["bash", "zsh", "fish", "nu", "dash"]
        .into_iter()
        .chain(USER_ENABLED_SHELLS.iter().map(|s| s.as_str()))
        .any(|s| {
            let shell_raw = term.shell_state().get_context().shell.as_deref();
            // we actually want to work with a nested figterm :)
            let shell = match shell_raw.and_then(|s| s.strip_suffix(" (figterm)")) {
                Some(s) => Some(s),
                None => shell_raw,
            };

            shell == Some(s)
        });
    let preexec = term.shell_state().preexec;

    let insertion_locked = insertion_lock_blocks(term);

    trace!(%shell_enabled, %preexec, %insertion_locked, "can_send_edit_buffer");

    shell_enabled && !insertion_locked && !preexec
}

/// Most keystrokes have no insertion lock. Take a write lock only when one is
/// armed — `ecterm` runs this on every edit-buffer tick.
fn insertion_lock_blocks<T>(term: &Term<T>) -> bool
where
    T: EventListener,
{
    if recover_rwlock_read(&INSERTION_LOCKED_AT).is_none() {
        return false;
    }
    let mut handle = recover_rwlock_write(&INSERTION_LOCKED_AT);
    match handle.as_ref() {
        Some(at) => {
            let lock_expired = at.elapsed().unwrap_or(Duration::ZERO) > Duration::from_millis(16);
            let should_unlock = lock_expired
                || term
                    .get_current_buffer()
                    .is_none_or(|buff| buff.buffer == *recover_mutex(&EXPECTED_BUFFER));
            if should_unlock {
                handle.take();
                if lock_expired {
                    trace!("insertion lock released because lock expired");
                } else {
                    trace!("insertion lock released because buffer looks like how we expect");
                }
                false
            } else {
                true
            }
        },
        None => false,
    }
}

const Q_DISABLE_AUTOCOMPLETE: &str = "Q_DISABLE_AUTOCOMPLETE";

fn autocomplete_enabled(env: &Env) -> bool {
    env.get_os(Q_DISABLE_AUTOCOMPLETE).is_none_or(|s| s.is_empty())
}

static AUTOCOMPLETE_ENABLED: LazyLock<bool> = LazyLock::new(|| autocomplete_enabled(&Env::new()));

/// Last edit-buffer frame that went on the wire. PTY highlighting can emit
/// many chunks that do not change the captured prompt text; skip those so
/// we do not protobuf+socket the same buffer 60 times a second.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SentEditBuffer {
    text: String,
    cursor: i64,
    cwd: Option<PathBuf>,
    coords: Option<TerminalCursorCoordinates>,
}

fn edit_buffer_frame_is_duplicate(
    last: Option<&SentEditBuffer>,
    text: &str,
    cursor: i64,
    cwd: Option<&Path>,
    coords: Option<&TerminalCursorCoordinates>,
    env_pending: bool,
) -> bool {
    if env_pending {
        return false;
    }
    last.is_some_and(|last| {
        last.text == text && last.cursor == cursor && last.cwd.as_deref() == cwd && last.coords.as_ref() == coords
    })
}

async fn send_edit_buffer<T>(
    term: &Term<T>,
    sender: &Sender<Hostbound>,
    cursor_coordinates: Option<TerminalCursorCoordinates>,
    last_sent: &mut Option<SentEditBuffer>,
) -> Result<()>
where
    T: EventListener,
{
    if !*AUTOCOMPLETE_ENABLED {
        return Ok(());
    }

    match term.get_current_buffer() {
        Some(edit_buffer) => {
            if let Some(cursor_idx) = edit_buffer.cursor_idx.and_then(|i| i.try_into().ok()) {
                debug!("edit_buffer: {edit_buffer:?}");
                trace!("buffer bytes: {:02X?}", edit_buffer.buffer.as_bytes());
                trace!("buffer chars: {:?}", edit_buffer.buffer.chars().collect::<Vec<_>>());

                let sent_epoch = pending_shell_context_epoch();
                if edit_buffer_frame_is_duplicate(
                    last_sent.as_ref(),
                    &edit_buffer.buffer,
                    cursor_idx,
                    term.shell_state().local_context.current_working_directory.as_deref(),
                    cursor_coordinates.as_ref(),
                    sent_epoch.is_some(),
                ) {
                    return Ok(());
                }
                let context = edit_buffer_context(term.shell_state(), sent_epoch.is_some());

                let edit_buffer_hook = new_edit_buffer_hook(
                    Some(context),
                    edit_buffer.buffer.clone(),
                    cursor_idx,
                    0,
                    cursor_coordinates,
                );
                let message = hook_to_message(edit_buffer_hook);

                trace!("Sending: {message:?}");

                sender.send_async(message).await?;
                if let Some(epoch) = sent_epoch {
                    mark_shell_context_sent(epoch);
                }
                *last_sent = Some(SentEditBuffer {
                    text: edit_buffer.buffer,
                    cursor: cursor_idx,
                    cwd: term.shell_state().local_context.current_working_directory.clone(),
                    coords: cursor_coordinates,
                });
            }
            Ok(())
        },
        None => Err(anyhow!("No edit buffer to send")),
    }
}

fn get_parent_shell() -> Result<String> {
    match env::var(Q_SHELL).ok().filter(|s| !s.is_empty()) {
        Some(v) => Ok(v),
        None => match env::var("SHELL").ok().filter(|s| !s.is_empty()) {
            Some(shell) => Ok(shell),
            None => {
                anyhow::bail!("No Q_SHELL or SHELL found");
            },
        },
    }
}

fn build_shell_command(command: Option<&[String]>) -> Result<CommandBuilder> {
    let mut builder = match command {
        Some(command) => {
            let mut iter = command.iter().map(|s| s.as_str());
            let Some(prog) = iter.next() else {
                anyhow::bail!("empty command");
            };
            let mut builder = CommandBuilder::new(prog);
            for arg in iter {
                builder.arg(arg);
            }
            builder
        },
        None => {
            let parent_shell = get_parent_shell()?;
            let mut builder = CommandBuilder::new(parent_shell);

            if env::var("Q_IS_LOGIN_SHELL").ok().as_deref() == Some("1") {
                builder.arg("--login");
            }

            if let Some(execution_string) = env::var("Q_EXECUTION_STRING").ok().filter(|s| !s.is_empty()) {
                builder.args(["-c", &execution_string]);
            }

            if let Some(extra_args) = env::var("Q_SHELL_EXTRA_ARGS").ok().filter(|s| !s.is_empty()) {
                builder.args(extra_args.split_whitespace().filter(|arg| arg != &"--login"));
            }

            builder
        },
    };

    builder.env(Q_TERM, env!("CARGO_PKG_VERSION"));
    if env::var_os("TMUX").is_some() {
        builder.env("Q_TERM_TMUX", env!("CARGO_PKG_VERSION"));
    }

    // Clean up environment and launch shell.
    builder.env_remove(Q_SHELL);
    builder.env_remove("Q_IS_LOGIN_SHELL");
    builder.env_remove("Q_START_TEXT");
    builder.env_remove("Q_SHELL_EXTRA_ARGS");
    builder.env_remove("Q_EXECUTION_STRING");

    if let Ok(dir) = std::env::current_dir() {
        builder.cwd(dir);
    }

    Ok(builder)
}

#[cfg(unix)]
fn launch_shell(command: Option<&[String]>) -> Result<()> {
    let cmd = build_shell_command(command)?.as_command()?;
    let mut args: Vec<&OsStr> = std::vec![cmd.get_program()];
    args.extend(cmd.get_args());

    let cargs = args.into_iter().map(cstring_from_arg).collect::<Result<Vec<_>>>()?;
    if cargs.is_empty() {
        anyhow::bail!("empty command");
    }
    for (key, val) in cmd.get_envs() {
        unsafe {
            match val {
                Some(value) => env::set_var(key, value),
                None => {
                    env::remove_var(key);
                },
            }
        }
    }

    execvp(&cargs[0], &cargs).map_err(|err| anyhow!("Failed to execvp: {err}"))?;
    unreachable!()
}

#[cfg(unix)]
fn cstring_from_arg(arg: &OsStr) -> Result<CString> {
    CString::new(arg.to_string_lossy().as_ref()).map_err(|err| anyhow!("command argument contains interior NUL: {err}"))
}

fn figterm_main(command: Option<&[String]>) -> Result<()> {
    fig_settings::settings::init_global().ok();

    let context = Context::new();

    let session_id = match std::env::var("MOCK_QTERM_SESSION_ID") {
        Ok(id) => id,
        Err(_) => uuid::Uuid::new_v4().simple().to_string(),
    };

    unsafe {
        std::env::set_var(QTERM_SESSION_ID, &session_id);
    }

    let parent_id = fig_os_shim::Env::new().q_parent().ok();

    let mut terminal = SystemTerminal::new_from_stdio()?;
    let screen_size = terminal.get_screen_size()?;

    let pty_size = PtySize {
        rows: screen_size.rows as u16,
        cols: screen_size.cols as u16,
        pixel_width: screen_size.xpixel as u16,
        pixel_height: screen_size.ypixel as u16,
    };

    let pty = open_pty(&pty_size).context("Failed to open pty")?;
    let command = build_shell_command(command)?;

    let pty_name = pty.slave.get_name().unwrap_or_else(|| session_id.clone());

    // A file appender starts a worker thread. Default filter is ERROR, so the
    // per-tab log is empty unless someone raised `Q_LOG_LEVEL` — skip the file
    // (and that thread) until they have.
    let log_file_path = std::env::var_os(Q_LOG_LEVEL)
        .map(|_| directories::logs_dir().map(|dir| dir.join(format!("{PTY_BINARY_NAME}{pty_name}.log"))))
        .transpose()?;
    let _log_guard = match initialize_logging(LogArgs {
        log_level: None,
        log_to_stdout: false,
        log_file_path,
        delete_old_log_file: true,
    }) {
        Ok(logger_guard) => Some(logger_guard),
        Err(err) => {
            if !fig_settings::state::get_bool_or("pty.suppress_log_error", false) {
                // let id = capture_anyhow(&err);
                eprintln!("{PRODUCT_NAME} failed to init logger: {err:?}");
            }
            None
        },
    };

    logger::stdio_debug_log(format!("pty name: {pty_name}"));
    logger::stdio_debug_log("Forking child shell process");

    #[cfg(unix)]
    {
        let pid = nix::unistd::getpid();
        logger::stdio_debug_log(format!("Parent pid: {pid}"));
    }

    let mut child = pty.slave.spawn_command(command)?;
    info!("Shell: {:?}", child.process_id());
    if let Some(pid) = child.process_id() {
        logger::stdio_debug_log(format!("Child pid: {pid}"));
    }

    let (child_tx, mut child_rx) = oneshot::channel();
    std::thread::spawn(move || child_tx.send(child.wait()));

    info!("Pid: {}", Pid::current());
    info!("Pty name: {pty_name}");

    // Two workers is enough for this process: the main loop is one `block_on`,
    // and the rest is I/O (stdin, the figterm listener, remote IPC, history).
    // The default pool is one thread per core, and `ecterm` multiplies by tab,
    // so that was paying for stacks the grid never used. Cap blocking at 8
    // like the desktop so a tab cannot grow an unbounded blocking pool.
    let runtime = runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .thread_name_fn(|| {
            static ATOMIC_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = ATOMIC_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            format!("{PTY_BINARY_NAME}-runtime-worker-{id}")
        })
        .build()?;

    let runtime_result = runtime.block_on(async {
        update::check_for_update(&context);

        terminal.set_raw_mode()?;

        let (main_loop_tx, main_loop_rx) = flume::bounded::<MainLoopEvent>(16);

        let history_sender = history::spawn_history_task();

        // Spawn thread to handle figterm ipc
        let incoming_receiver = spawn_figterm_ipc(&session_id).await?;

        // Spawn thread to handle remote ipc
        let (remote_sender, remote_receiver, stop_ipc_tx) = spawn_remote_ipc(
            session_id.clone(),
            parent_id,
            main_loop_tx.clone()
        ).await?;

        let mut stdout = io::stdout();
        let mut master = pty.master.get_async_master_pty()?;

        let mut processor = Processor::new();
        let size = SizeInfo::new(pty_size.rows as usize, pty_size.cols as usize);
        let event_sender = EventHandler::new(remote_sender.clone(), history_sender.clone(), main_loop_tx.clone());
        // One line of history is load-bearing. `get_current_buffer` bails when
        // `topmost_line() > cmd_cursor.line`, and `topmost_line()` is
        // `Line(-history_size)`. Zero would drop a prompt that just scrolled
        // off the viewport and lose the edit buffer for the rest of the command.
        let mut term = alacritty_terminal::Term::new(size, event_sender, TERM_SCROLLBACK_LINES, session_id.clone());

        #[cfg(target_os = "windows")]
        term.set_windows_delay_end_prompt(true);

        let mut write_buffer: Vec<u8> = vec![0; BUFFER_SIZE];

        let mut key_interceptor = KeyInterceptor::new();
        key_interceptor.load_key_intercepts()?;

        let mut edit_buffer_interval = tokio::time::interval(Duration::from_millis(16));
        let mut last_sent_edit_buffer = None;

        let mut first_time = true;

        let input_rx = terminal.read_input()?;

        let key_code_encode_mode = KeyCodeEncodeModes {
            #[cfg(unix)]
            encoding: KeyboardEncoding::Xterm,
            #[cfg(windows)]
            encoding: KeyboardEncoding::Win32,
            application_cursor_keys: false,
            newline_mode: false,
        };

        if let Ok(shell) = get_parent_shell() {
            let path = std::path::Path::new(&shell);
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or(shell.as_str());
            let title_osc = format!("\x1b]0;{name}\x07");
            if let Err(err) = stdout.write(title_osc.as_bytes()).await {
                error!("Failed to write title osc: {err}");
            }
        }

        let mut csi_u_set = false;

        let result: Result<()> = 'select_loop: loop {
            if first_time && term.shell_state().has_seen_prompt {
                trace!("Has seen prompt and first time");
                let initial_command = env::var("Q_START_TEXT").ok().filter(|s| !s.is_empty());
                if let Some(mut initial_command) = initial_command {
                    debug!("Sending initial text: {initial_command}");
                    initial_command.push('\n');
                    if let Err(err) = master.write_all(initial_command.as_bytes()).await {
                        error!("Failed to write initial command: {err}");
                    }
                }
                first_time = false;
            }

            let select_result: Result<()> = select! {
                biased;
                res = main_loop_rx.recv_async() => {
                    match res {
                        Ok(event) => {
                            match event {
                                MainLoopEvent::Insert { insert, unlock, bracketed, execute } => {
                                    use bstr::ByteSlice;
                                    if bracketed {
                                        if term.mode().contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE) {
                                            master.write_all(b"\x1b[200~").await?;
                                            master.write_all(&insert.replace(b"\x1b", "")).await?;
                                            master.write_all(b"\x1b[201~").await?;
                                        } else {
                                            master.write_all(&insert.replace("\r\n", "\r").replace("\n", "\r")).await?;
                                        }
                                    } else {
                                        master.write_all(&insert).await?;
                                    }

                                    if execute {
                                        master.write_all(b"\r").await?; 
                                    }

                                    if unlock {
                                        key_interceptor.reset();
                                    }
                                },
                                MainLoopEvent::UnlockInterception => {
                                    key_interceptor.reset();
                                },
                                MainLoopEvent::SetImmediateMode(mode) => {
                                    if let Err(err) = terminal.set_immediate_mode(mode) {
                                        error!(%err, "Failed to set immediate mode");
                                    }
                                },
                                MainLoopEvent::SetCsiU => {
                                    // Send CSI > 1 u
                                    stdout.write_all(b"\x1b[>1u").await?;
                                    stdout.flush().await?;
                                    csi_u_set = true;
                                },
                                MainLoopEvent::UnsetCsiU => {
                                    // Send CSI < u
                                    stdout.write_all(b"\x1b[<u").await?;
                                    stdout.flush().await?;
                                    csi_u_set = false;
                                },
                                MainLoopEvent::PromptSSH { uuid: _, remote_host: _ } => {
                                    // let should_install = should_install_remote_ssh_integration(
                                    //     uuid,
                                    //     remote_host.clone(),
                                    //     main_loop_tx.clone(),
                                    //     remote_receiver.clone(),
                                    //     remote_sender.clone(),
                                    //     &term,
                                    //     &mut master,
                                    //     &mut key_interceptor,
                                    // ).await;

                                    // let should_install = match should_install {
                                    //     Some(val) => val,
                                    //     None => {
                                    //         prompt_remote_integration_install(
                                    //             remote_host,
                                    //             console_term.clone(),
                                    //             console_term_key_tx.clone(),
                                    //             &mut terminal,
                                    //             input_rx.clone(),
                                    //         ).await.unwrap_or(false)
                                    //     }
                                    // };

                                    // if should_install {
                                    //     let installation_command = "curl -fSsL https://fig.io/install-minimal.sh | bash; exec $SHELL\n";
                                    //     master.write_all(installation_command.as_bytes()).await?;
                                    // }
                                }
                            }
                        }
                        Err(err) => warn!("Failed to recv: {err}"),
                    };
                    Ok(())
                }
                res = input_rx.recv_async() => {
                    let mut input_res = Ok(());
                    match res {
                        Ok(events) => {
                            let mut write_buffer = BytesMut::new();
                            for event in events {
                                match event {
                                    Ok((raw, InputEvent::Key(event))) => {
                                        // Do not do most stuff during not preexec since that means a command is running
                                        let preexec = term.shell_state().preexec;

                                        debug!(?event, ?raw, %preexec,  "Got key event");

                                        // if we are in CSI u mode we try to encode first, otherwise we try to send the raw bytes first
                                        let raw = if csi_u_set {
                                            event.key.encode(event.modifiers, key_code_encode_mode, true)
                                                .ok()
                                                .map(|s| s.into_bytes().into()).or(raw)
                                        } else {
                                            raw.or_else(|| {
                                                event.key.encode(event.modifiers, key_code_encode_mode, true)
                                                    .ok()
                                                    .map(|s| s.into_bytes().into())
                                            })
                                        };

                                        let handled_action = if !preexec {
                                            if let Some(action) = key_interceptor.intercept_key(&event) {
                                                debug!(?action, "Intercepted action");
                                                let s = raw.clone()
                                                    .and_then(|b| String::from_utf8(b.to_vec()).ok())
                                                    .unwrap_or_default();
                                                let context = shell_state_to_context(term.shell_state());
                                                let hook = fig_proto::remote_hooks::new_intercepted_key_hook(context, action, s);
                                                if let Err(err) = remote_sender.send(hook_to_message(hook)) {
                                                    error!(%err, "Sender error");
                                                }

                                                if event.key == KeyCode::Escape {
                                                    key_interceptor.reset();
                                                }
                                                true
                                            } else {
                                                false
                                            }
                                        } else {
                                            false
                                        };

                                        if !handled_action {
                                            if let Some(bytes) = raw {
                                                if (event.key == KeyCode::Char('c') || event.key == KeyCode::Char('d'))
                                                    && event.modifiers == Modifiers::CTRL {
                                                    key_interceptor.reset();
                                                }
                                                write_buffer.extend(&bytes);
                                            }
                                        }
                                    }
                                    Ok((_, InputEvent::Resized)) => {
                                        terminal.flush()?;

                                        let size = terminal.get_screen_size()?;
                                        let pty_size = PtySize {
                                            rows: size.rows as u16,
                                            cols: size.cols as u16,
                                            pixel_width: size.xpixel as u16,
                                            pixel_height: size.ypixel as u16,
                                        };

                                        master.resize(pty_size)?;
                                        let window_size = SizeInfo::new(size.rows, size.cols);
                                        debug!("Window size changed: {window_size:?}");
                                        term.resize(window_size);
                                    }
                                    Ok((None, InputEvent::Paste(string))) => {
                                        // Pass through bracketed pastes.
                                        if term.mode().contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE) {
                                            write_buffer.extend(b"\x1b[200~");
                                            write_buffer.extend(string.replace('\x1b', "").as_bytes());
                                            write_buffer.extend(b"\x1b[201~");
                                        } else {
                                            write_buffer.extend(string.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
                                        }
                                    }
                                    Ok((raw, _)) => {
                                        if let Some(raw) = raw {
                                            info!("Fallback write");
                                            write_buffer.extend(&raw);
                                        } else {
                                            info!("Unhandled input event with no raw pass-through data");
                                        }
                                    }
                                    Err(err) => {
                                        error!("Failed receiving input from stdin: {err}");
                                        input_res = Err(err);
                                        break;
                                    }
                                };
                            }
                            master.write_all(&write_buffer).await?;
                        }
                        Err(err) => {
                            warn!("Failed recv: {err}");
                        }
                    };
                    input_res
                }
                res = master.read(&mut write_buffer) => {
                    #[cfg(feature = "profiling_early_exit")]
                    break 'select_loop Ok(());
                    match res {
                        Ok(0) => {
                            trace!("EOF from master");
                            break 'select_loop Ok(());
                        },
                        Ok(size) => {
                            trace!("Read {size} bytes from master");

                            let old_delayed_count = term.get_delayed_events_count();
                            for byte in &write_buffer[..size] {
                                processor.advance(&mut term, *byte);
                            }

                            let delayed_count = term.get_delayed_events_count();

                            // We have delayed events and did not receive delayed events. Flush all
                            // delayed events now.
                            if delayed_count > 0 && delayed_count == old_delayed_count {
                                term.flush_delayed_events();
                            }

                            stdout.write_all(&write_buffer[..size]).await?;
                            stdout.flush().await?;

                            if can_send_edit_buffer(&term) {
                                let cursor_coordinates = get_cursor_coordinates(&terminal);
                                if let Err(err) = send_edit_buffer(
                                    &term,
                                    &remote_sender,
                                    cursor_coordinates,
                                    &mut last_sent_edit_buffer,
                                )
                                .await
                                {
                                    warn!("Failed to send edit buffer: {err}");
                                }
                            }

                            Ok(())
                        }
                        Err(err) => {
                            error!("Failed to read from master: {err}");
                            break 'select_loop Ok(());
                        }
                    }
                }
                msg = remote_receiver.recv_async() => {
                    match msg {
                        Ok(message) => {
                            trace!("Received message from socket: {message:?}");
                            process_remote_message(
                                message,
                                main_loop_tx.clone(),
                                remote_sender.clone(),
                                &term,
                                &mut master,
                                &mut key_interceptor
                            ).await?;
                        }
                        Err(err) => {
                            error!("Failed to receive message from socket: {err}");
                        }
                    }
                    Ok(())
                }
                msg = incoming_receiver.recv_async() => {
                    match msg {
                        Ok((message, sender)) => {
                            debug!("Received message from figterm listener: {message:?}");
                            process_figterm_message(
                                message,
                                main_loop_tx.clone(),
                                sender.clone(),
                                &term,
                                &history_sender,
                                &mut master,
                                &mut key_interceptor,
                                &session_id,
                            ).await?;
                        }
                        Err(err) => {
                            error!("Failed to receive message from socket: {err}");
                        }
                    }
                    Ok(())
                }
                // Check if to send the edit buffer because of timeout
                _ = edit_buffer_interval.tick() => {
                    let send_eb = recover_rwlock_read(&INSERTION_LOCKED_AT).is_some();
                    if send_eb && can_send_edit_buffer(&term) {
                        let cursor_coordinates = get_cursor_coordinates(&terminal);
                        if let Err(err) = send_edit_buffer(
                            &term,
                            &remote_sender,
                            cursor_coordinates,
                            &mut last_sent_edit_buffer,
                        )
                        .await
                        {
                            warn!(%err, "Failed to send edit buffer");
                        }
                    }
                    Ok(())
                }
                _ = &mut child_rx => {
                    trace!("Shell process exited");
                    break 'select_loop Ok(());
                }
            };

            if let Err(err) = select_result {
                error!("Error in select loop: {err}");
                break 'select_loop Err(err);
            }
        };

        let _ = stop_ipc_tx.send(());

        result
    });

    // Reading from stdin is a blocking task on a separate thread:
    // https://github.com/tokio-rs/tokio/issues/2466
    // We must explicitly shutdown the runtime to exit.
    // This can cause resource leaks if we aren't careful about tasks we spawn.
    runtime.shutdown_background();

    // attempt cleanup
    #[cfg(target_os = "linux")]
    cleanup::cleanup()?;

    runtime_result
}

fn main() {
    let cli = Cli::parse();
    let command = cli.command.as_deref();

    logger::stdio_debug_log(format!("{Q_LOG_LEVEL}={}", fig_log::get_log_level()));

    if !state::get_bool_or("qterm.enabled", true) {
        println!("[NOTE] {PTY_BINARY_NAME} is disabled. Autocomplete will not work.");
        logger::stdio_debug_log(format!("{PTY_BINARY_NAME} is disabled. `qterm.enabled` == false"));
        return;
    }

    match figterm_main(command) {
        Ok(()) => {
            info!("Exiting");
        },
        Err(err) => {
            error!("Error in async runtime: {err}");
            println!("{PRODUCT_NAME} had an Error!: {err:?}");
            // capture_anyhow(&err);

            // Fallback to normal shell
            #[cfg(unix)]
            if let Err(err) = launch_shell(command) {
                // capture_anyhow(&err);
                logger::stdio_debug_log(err.to_string());
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_does_not_need_sysinfo() {
        assert!(hostname().is_some_and(|name| !name.is_empty()));
    }

    #[test]
    fn disabled_pty_note_uses_product_binary_name() {
        let production = include_str!("main.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        assert!(
            !production.contains("[NOTE] qterm is disabled"),
            "user-facing disable note must not say qterm"
        );
        assert!(
            production.contains("[NOTE] {PTY_BINARY_NAME} is disabled"),
            "disable note should name ecterm"
        );
        assert!(
            production.contains("qterm.enabled"),
            "the settings key stays qterm.enabled"
        );
    }

    #[test]
    fn intercept_remote_sender_does_not_unwrap() {
        let production = include_str!("main.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        // Concat so `rg` for the old unwrap is empty of this pin too.
        assert!(
            !production.contains(&["remote_sender.send(hook_to_message(hook))", ".unwrap()"].concat()),
            "a closed intercept channel must log instead of panicking ecterm"
        );
        assert!(
            production.contains("remote_sender.send(hook_to_message(hook))"),
            "intercept still sends the intercepted-key hook"
        );
        assert!(
            production.contains("Sender error"),
            "intercept send failure should log like EventHandler"
        );
    }

    #[test]
    fn tokio_runtime_caps_blocking_threads() {
        let production = include_str!("main.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        assert!(
            production.contains(".worker_threads(2)"),
            "ecterm must keep two tokio workers"
        );
        assert!(
            production.contains(".max_blocking_threads(8)"),
            "ecterm blocking pool must match the desktop cap of 8"
        );
    }

    #[test]
    fn term_keeps_one_line_of_scrollback() {
        assert_eq!(TERM_SCROLLBACK_LINES, 1);
    }

    #[test]
    fn empty_command_does_not_panic() {
        let err = build_shell_command(Some(&[])).unwrap_err();
        assert!(
            err.to_string().contains("empty command"),
            "an empty wrap argv must fail, not unwrap the program name"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wrap_argv_with_interior_nul_does_not_panic() {
        use std::ffi::OsStr;
        let err = cstring_from_arg(OsStr::new("bad\0arg")).unwrap_err();
        assert!(
            err.to_string().contains("NUL"),
            "interior NUL must be a Result, not a CString expect"
        );
    }

    #[test]
    fn launch_shell_does_not_unwrap_cstring_or_exec() {
        let production = include_str!("main.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production");
        assert!(
            !production.contains(&["expect(", "\"Failed to convert arg to CString\")"].concat())
                && !production.contains(&["expect(", "\"Failed to execvp\")"].concat()),
            "a NUL in the wrap argv or a failed execvp must return Err, not panic ecterm"
        );
        assert!(
            production.contains("cstring_from_arg") && production.contains("Failed to execvp"),
            "unix wrap still converts args and execs the parent shell"
        );
    }

    #[test]
    fn mutex_lock_recovers_from_poison() {
        let mutex = Mutex::new(7u8);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("poison");
        }));
        assert_eq!(*recover_mutex(&mutex), 7);
    }

    #[test]
    fn production_figterm_locks_recover_from_poison() {
        for (name, src) in [
            ("message.rs", include_str!("message.rs")),
            ("event_handler.rs", include_str!("event_handler.rs")),
        ] {
            assert!(!src.contains(".lock().unwrap()"), "{name} still panics on mutex poison");
            assert!(
                !src.contains(".write().unwrap()"),
                "{name} still panics on rwlock poison"
            );
        }
    }

    #[test]
    fn autocomplete_enabled_test() {
        assert!(autocomplete_enabled(&Env::new_fake()));
        assert!(autocomplete_enabled(&Env::from_slice(&[(Q_DISABLE_AUTOCOMPLETE, "")])));
        assert!(!autocomplete_enabled(&Env::from_slice(&[(
            Q_DISABLE_AUTOCOMPLETE,
            "1"
        )])));
        assert!(!autocomplete_enabled(&Env::from_slice(&[(
            Q_DISABLE_AUTOCOMPLETE,
            "1"
        )])));
    }

    #[test]
    fn edit_buffer_sends_env_only_after_shell_context_updates() {
        note_shell_context_updated();
        let epoch = pending_shell_context_epoch().expect("pending after update");
        assert_eq!(pending_shell_context_epoch(), Some(epoch));
        mark_shell_context_sent(epoch);
        assert_eq!(pending_shell_context_epoch(), None);
        note_shell_context_updated();
        let next = pending_shell_context_epoch().expect("pending after a later update");
        assert_ne!(next, epoch);
        mark_shell_context_sent(next);
        assert_eq!(pending_shell_context_epoch(), None);
    }

    #[test]
    fn unchanged_edit_buffer_frame_is_duplicate() {
        let last = SentEditBuffer {
            text: "git ch".into(),
            cursor: 6,
            cwd: Some("/tmp".into()),
            coords: None,
        };
        assert!(edit_buffer_frame_is_duplicate(
            Some(&last),
            "git ch",
            6,
            Some(Path::new("/tmp")),
            None,
            false,
        ));
        assert!(!edit_buffer_frame_is_duplicate(
            Some(&last),
            "git che",
            7,
            Some(Path::new("/tmp")),
            None,
            false,
        ));
        assert!(!edit_buffer_frame_is_duplicate(
            Some(&last),
            "git ch",
            6,
            Some(Path::new("/var")),
            None,
            false,
        ));
        assert!(!edit_buffer_frame_is_duplicate(
            Some(&last),
            "git ch",
            6,
            Some(Path::new("/tmp")),
            None,
            true,
        ));
        assert!(!edit_buffer_frame_is_duplicate(
            None,
            "git ch",
            6,
            Some(Path::new("/tmp")),
            None,
            false
        ));
    }

    #[test]
    fn send_edit_buffer_skips_duplicate_frames_before_encode() {
        let src = include_str!("main.rs");
        let start = src.find("async fn send_edit_buffer").expect("send_edit_buffer");
        let body = &src[start..];
        let end = body.find("\nfn get_parent_shell").expect("get_parent_shell");
        let body = &body[..end];
        assert!(
            body.contains("edit_buffer_frame_is_duplicate") && body.contains("return Ok(())"),
            "unchanged edit-buffer ticks must skip protobuf encode and socket send"
        );
        assert!(
            body.contains("pending_shell_context_epoch") && body.contains("sent_epoch.is_some()"),
            "a pending env/alias epoch must still send even when the buffer text is unchanged"
        );
        assert!(
            body.contains("current_working_directory.as_deref()") && !body.contains("cwd_string"),
            "duplicate frames must compare the PathBuf, not format cwd via Display"
        );
    }
}
