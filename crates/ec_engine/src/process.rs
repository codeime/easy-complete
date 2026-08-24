//! Timed subprocess helper for generators. Never run this on the UI thread.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

const MAX_STDOUT: usize = 256 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum RunResult {
    Output(String),
    TimedOut,
    Failed,
}

pub fn execute(command: &str, args: &[String], cwd: &str, timeout: Duration) -> String {
    output_or_empty(run(command, args, cwd, timeout, false, false))
}

/// Full executeCommand host result. Timeout and spawn failure are `Err` so the
/// JS host can throw the same way the old WebView wrapper did. A non-zero
/// exit status is still `Ok`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    TimedOut,
    Failed,
}

pub fn execute_full(
    command: &str,
    args: &[String],
    cwd: &str,
    env: &[(String, String)],
    timeout: Duration,
) -> Result<CommandOutput, CommandError> {
    if command.is_empty() {
        return Err(CommandError::Failed);
    }
    let mut cmd = Command::new(command);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(|_spawn| CommandError::Failed)?;
    wait_child_output(child, timeout)
}

fn wait_child_output(child: Child, timeout: Duration) -> Result<CommandOutput, CommandError> {
    #[cfg(unix)]
    {
        wait_child_output_unix(child, timeout)
    }
    #[cfg(not(unix))]
    {
        wait_child_output_threaded(child, timeout)
    }
}

/// JS `executeCommand` used to `wait_with_output` on a helper thread. That
/// allocated a stack per hook subprocess and buffered the whole pipe before
/// the 256 KiB cap, so a noisy generator could OOM the engine worker. Unix
/// polls both pipes on this thread and truncates as it reads.
#[cfg(unix)]
fn wait_child_output_unix(mut child: Child, timeout: Duration) -> Result<CommandOutput, CommandError> {
    use std::io::ErrorKind;
    use std::os::fd::AsRawFd;

    let pid = child.id();
    let Some(mut stdout) = child.stdout.take() else {
        kill_and_reap(&mut child, pid);
        return Err(CommandError::Failed);
    };
    let Some(mut stderr) = child.stderr.take() else {
        kill_and_reap(&mut child, pid);
        return Err(CommandError::Failed);
    };
    set_nonblocking(stdout.as_raw_fd());
    set_nonblocking(stderr.as_raw_fd());

    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let deadline = std::time::Instant::now() + timeout;
    let mut stdout_eof = false;
    let mut stderr_eof = false;

    loop {
        if !stdout_eof {
            stdout_eof = drain_stdout(&mut stdout, &mut tmp, &mut out_buf);
        }
        if !stderr_eof {
            stderr_eof = drain_stdout(&mut stderr, &mut tmp, &mut err_buf);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if !stdout_eof {
                    stdout_eof = drain_stdout(&mut stdout, &mut tmp, &mut out_buf);
                }
                if !stderr_eof {
                    stderr_eof = drain_stdout(&mut stderr, &mut tmp, &mut err_buf);
                }
                // A background descendant can outlive the command leader and
                // keep a pipe open. Do not leak that process group.
                if !stdout_eof || !stderr_eof {
                    kill_process_group(pid);
                }
                return Ok(command_output(status.code().unwrap_or(-1), out_buf, err_buf));
            },
            Ok(None) => {},
            Err(_) => {
                kill_and_reap(&mut child, pid);
                return Err(CommandError::Failed);
            },
        }

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            kill_and_reap(&mut child, pid);
            return Err(CommandError::TimedOut);
        }

        if stdout_eof && stderr_eof {
            std::thread::sleep(remaining.min(Duration::from_millis(20)));
            continue;
        }

        let mut fds = [
            libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: if stdout_eof { 0 } else { libc::POLLIN | libc::POLLHUP },
                revents: 0,
            },
            libc::pollfd {
                fd: stderr.as_raw_fd(),
                events: if stderr_eof { 0 } else { libc::POLLIN | libc::POLLHUP },
                revents: 0,
            },
        ];
        let ms = i32::try_from(remaining.as_millis().min(20)).unwrap_or(20);
        // SAFETY: both fds are the child's pipes we still own.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            kill_and_reap(&mut child, pid);
            return Err(CommandError::Failed);
        }
        if n == 0 {
            continue;
        }
        if fds[0].revents & libc::POLLNVAL != 0 {
            stdout_eof = true;
        }
        if fds[1].revents & libc::POLLNVAL != 0 {
            stderr_eof = true;
        }
    }
}

fn command_output(status: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> CommandOutput {
    CommandOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

/// Cap stdout/stderr as they are read. `wait_with_output` buffers the whole
/// pipe first, so a noisy generator could OOM the engine worker before the
/// 256 KiB cap. Unix polls both pipes on the attempt thread; Windows still
/// uses a waiter (no live PeekNamedPipe here).
///
/// Compiled in tests on every OS so Linux CI can pin that we *discard* past
/// the cap instead of stopping. Stopping leaves the pipe full, the child
/// blocked, and `child.wait()` hanging until the timeout — the same lost
/// exit status the unix `drain_stdout` path used to have.
#[cfg(any(test, not(unix)))]
fn read_capped(mut reader: impl std::io::Read) -> Vec<u8> {
    use std::io::ErrorKind;

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                let room = MAX_STDOUT.saturating_sub(buf.len());
                if room > 0 {
                    buf.extend_from_slice(&tmp[..n.min(room)]);
                }
            },
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    buf
}

#[cfg(not(unix))]
fn wait_child_output_threaded(mut child: Child, timeout: Duration) -> Result<CommandOutput, CommandError> {
    use std::sync::mpsc;
    use std::thread;

    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let out_handle = stdout.map(|pipe| thread::spawn(move || read_capped(pipe)));
        let err_handle = stderr.map(|pipe| thread::spawn(move || read_capped(pipe)));
        let status = child.wait().map(|status| status.code().unwrap_or(-1)).unwrap_or(-1);
        let stdout = out_handle.and_then(|handle| handle.join().ok()).unwrap_or_default();
        let stderr = err_handle.and_then(|handle| handle.join().ok()).unwrap_or_default();
        let _ = tx.send((status, stdout, stderr));
    });
    match rx.recv_timeout(timeout) {
        Ok((status, stdout, stderr)) => Ok(command_output(status, stdout, stderr)),
        Err(_) => {
            kill_process_group(pid);
            Err(CommandError::TimedOut)
        },
    }
}

/// [`execute`] that yields `None` on timeout or spawn failure so callers do not cache empties.
pub fn try_execute(command: &str, args: &[String], cwd: &str, timeout: Duration) -> Option<String> {
    match run(command, args, cwd, timeout, false, false) {
        RunResult::Output(stdout) => Some(stdout),
        RunResult::TimedOut | RunResult::Failed => None,
    }
}

/// Isolated session so the child cannot steal the TTY. `None` on timeout or spawn failure.
pub fn try_execute_isolated(command: &str, args: &[String], cwd: &str, timeout: Duration) -> Option<String> {
    match run(command, args, cwd, timeout, true, false) {
        RunResult::Output(stdout) => Some(stdout),
        RunResult::TimedOut | RunResult::Failed => None,
    }
}

/// Isolated execution that also treats a non-zero exit status as failure.
/// History commands use this contract so a broken custom source falls back
/// to the database even if it printed diagnostics on stdout first.
pub fn try_execute_isolated_success(command: &str, args: &[String], cwd: &str, timeout: Duration) -> Option<String> {
    match run(command, args, cwd, timeout, true, true) {
        RunResult::Output(stdout) => Some(stdout),
        RunResult::TimedOut | RunResult::Failed => None,
    }
}

fn output_or_empty(result: RunResult) -> String {
    match result {
        RunResult::Output(stdout) => stdout,
        RunResult::TimedOut | RunResult::Failed => String::new(),
    }
}

fn run(
    command: &str,
    args: &[String],
    cwd: &str,
    timeout: Duration,
    isolated: bool,
    require_success: bool,
) -> RunResult {
    if command.is_empty() {
        return RunResult::Failed;
    }
    let mut cmd = Command::new(command);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        if isolated {
            // SAFETY: setsid() is called in the child after fork and before exec.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        } else {
            cmd.process_group(0);
        }
    }
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return RunResult::Failed,
    };
    wait_child(child, timeout, require_success)
}

#[cfg(unix)]
fn set_nonblocking(fd: i32) {
    // SAFETY: fd is a pipe the caller still owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn wait_child(child: Child, timeout: Duration, require_success: bool) -> RunResult {
    #[cfg(unix)]
    {
        wait_child_unix(child, timeout, require_success)
    }
    #[cfg(not(unix))]
    {
        wait_child_threaded(child, timeout, require_success)
    }
}

#[cfg(unix)]
fn wait_child_unix(mut child: Child, timeout: Duration, require_success: bool) -> RunResult {
    use std::io::ErrorKind;
    use std::os::fd::AsRawFd;

    let pid = child.id();
    let Some(mut stdout) = child.stdout.take() else {
        kill_and_reap(&mut child, pid);
        return RunResult::Failed;
    };
    let fd = stdout.as_raw_fd();
    set_nonblocking(fd);

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let deadline = std::time::Instant::now() + timeout;
    let mut stdout_eof = false;

    loop {
        if !stdout_eof {
            stdout_eof = drain_stdout(&mut stdout, &mut tmp, &mut buf);
        }
        // Unlike `execute_full`, `RunResult::Output` carries no exit status, so
        // there is nothing to preserve by waiting for the child to finish. Once
        // the cap is full we already have everything we will keep: return it
        // now rather than let a long-running noisy script burn its whole budget
        // and time out into an empty result.
        if !require_success && buf.len() >= MAX_STDOUT {
            kill_and_reap(&mut child, pid);
            return RunResult::Output(String::from_utf8_lossy(&buf).into_owned());
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                if !stdout_eof {
                    stdout_eof = drain_stdout(&mut stdout, &mut tmp, &mut buf);
                }
                // A background descendant can outlive the command leader and
                // keep its stdout pipe open. Do not leak that process group or
                // let a later reader wait forever for EOF.
                if !stdout_eof {
                    kill_process_group(pid);
                }
                if require_success && !status.success() {
                    return RunResult::Failed;
                }
                return RunResult::Output(String::from_utf8_lossy(&buf).into_owned());
            },
            Ok(None) => {},
            Err(_) => {
                kill_and_reap(&mut child, pid);
                return RunResult::Failed;
            },
        }

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            kill_and_reap(&mut child, pid);
            // Pipe already closed: stdout is complete even if the process is hung.
            if stdout_eof && !require_success {
                return RunResult::Output(String::from_utf8_lossy(&buf).into_owned());
            }
            return RunResult::TimedOut;
        }

        if stdout_eof {
            std::thread::sleep(remaining.min(Duration::from_millis(20)));
            continue;
        }

        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        let ms = i32::try_from(remaining.as_millis().min(20)).unwrap_or(20);
        // SAFETY: pollfd.fd is the stdout pipe still owned by `stdout`.
        let n = unsafe { libc::poll(&mut pollfd, 1, ms) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            kill_and_reap(&mut child, pid);
            return RunResult::Failed;
        }
        if n == 0 {
            continue;
        }
        if pollfd.revents & libc::POLLNVAL != 0 {
            stdout_eof = true;
        }
    }
}

/// Reads per call before handing control back so the caller can re-check its
/// deadline. A generator that writes without pause must not pin this loop.
#[cfg(unix)]
const DRAIN_READS_PER_CALL: usize = 64;

/// Read what is available, keeping at most [`MAX_STDOUT`] bytes and *discarding*
/// the rest. Returns `true` at EOF.
///
/// Draining past the cap instead of stopping is deliberate: an unread pipe fills
/// up and blocks the child, which would leave killing it as the only way out and
/// lose its real exit status. Sixteen bundled hooks branch on that status, so a
/// capped-but-usable listing must not look like a crash.
#[cfg(unix)]
fn drain_stdout(stdout: &mut impl std::io::Read, tmp: &mut [u8], buf: &mut Vec<u8>) -> bool {
    use std::io::ErrorKind;
    for _ in 0..DRAIN_READS_PER_CALL {
        match stdout.read(tmp) {
            Ok(0) => return true,
            Ok(n) => {
                let room = MAX_STDOUT.saturating_sub(buf.len());
                if room > 0 {
                    buf.extend_from_slice(&tmp[..n.min(room)]);
                }
            },
            Err(err) if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted => {
                return false;
            },
            Err(_) => return true,
        }
    }
    false
}

#[cfg(not(unix))]
fn wait_child_threaded(mut child: Child, timeout: Duration, require_success: bool) -> RunResult {
    use std::sync::mpsc;
    use std::thread;

    let pid = child.id();
    let stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let stdout = stdout.map(read_capped).unwrap_or_default();
        let status = child.wait();
        let _ = tx.send((status, stdout));
    });
    match rx.recv_timeout(timeout) {
        Ok((Ok(status), stdout)) => {
            if require_success && !status.success() {
                return RunResult::Failed;
            }
            RunResult::Output(String::from_utf8_lossy(&stdout).into_owned())
        },
        Ok((Err(_), _)) => RunResult::Failed,
        Err(_) => {
            kill_process_group(pid);
            RunResult::TimedOut
        },
    }
}

fn reap(child: &mut Child) {
    let _ = child.wait();
}

fn kill_and_reap(child: &mut Child, pid: u32) {
    kill_process_group(pid);
    reap(child);
}

/// `taskkill` argv for a timed-out generator. `/T` kills the process tree,
/// matching Unix `kill(-pgid, SIGKILL)`. Compiled in tests on every OS so
/// Linux CI can pin the flags.
#[cfg(any(test, not(unix)))]
fn windows_taskkill_args(pid: u32) -> Vec<String> {
    vec!["/PID".to_string(), pid.to_string(), "/T".to_string(), "/F".to_string()]
}

fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let pgid = pid as i32;
        // SAFETY: the child was started with process_group(0) or setsid(), so its
        // pgid equals pid. Negative pgid sends the signal to the whole group.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = Command::new("taskkill").args(windows_taskkill_args(pid)).status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_timeout_kill_includes_process_tree_flag() {
        assert_eq!(windows_taskkill_args(4242), ["/PID", "4242", "/T", "/F"]);
    }

    #[cfg(unix)]
    #[test]
    fn times_out_and_returns_empty() {
        let out = execute("sleep", &["2".into()], "/", Duration::from_millis(50));
        assert!(out.is_empty());
        assert_eq!(
            try_execute("sleep", &["2".into()], "/", Duration::from_millis(50)),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn captures_stdout_without_an_extra_thread() {
        let out = execute("printf", &["hello-engine".into()], "/", Duration::from_millis(500));
        assert_eq!(out, "hello-engine");
    }

    #[cfg(unix)]
    #[test]
    fn execute_full_captures_stdout_and_stderr_without_an_extra_thread() {
        let out = execute_full(
            "sh",
            &["-c".into(), "printf hello-out; printf hello-err >&2".into()],
            "/",
            &[],
            Duration::from_millis(500),
        )
        .expect("ok");
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout, "hello-out");
        assert_eq!(out.stderr, "hello-err");
    }

    #[cfg(unix)]
    #[test]
    fn execute_full_passes_env() {
        let out = execute_full(
            "sh",
            &["-c".into(), "printf %s \"$EC_TEST_ENV\"".into()],
            "/",
            &[("EC_TEST_ENV".into(), "from-engine".into())],
            Duration::from_millis(500),
        )
        .expect("ok");
        assert_eq!(out.stdout, "from-engine");
    }

    #[cfg(unix)]
    #[test]
    fn execute_full_times_out() {
        assert_eq!(
            execute_full("sleep", &["2".into()], "/", &[], Duration::from_millis(50)),
            Err(CommandError::TimedOut)
        );
    }

    #[cfg(unix)]
    #[test]
    fn execute_full_caps_stdout_and_keeps_the_real_exit_code() {
        // Sixteen bundled hooks branch on `status`. A command that merely wrote
        // more than the cap and then failed must still report its own code, and
        // one that succeeded must not look like a crash.
        let out = execute_full(
            "sh",
            &["-c".into(), "yes aaaaaaaa | head -n 40000; exit 3".into()],
            "/",
            &[],
            Duration::from_secs(10),
        )
        .expect("capped output is not an error");
        assert_eq!(out.stdout.len(), MAX_STDOUT, "stdout is held at the cap");
        assert_eq!(out.status, 3, "the cap must not overwrite the exit code");
    }

    #[cfg(unix)]
    #[test]
    fn a_noisy_stderr_does_not_truncate_stdout() {
        let out = execute_full(
            "sh",
            &["-c".into(), "printf ok; yes aaaaaaaa | head -n 40000 >&2".into()],
            "/",
            &[],
            Duration::from_secs(10),
        )
        .expect("noisy stderr is not an error");
        assert_eq!(out.stdout, "ok", "stdout must not be cut short by stderr volume");
        assert_eq!(out.stderr.len(), MAX_STDOUT);
        assert_eq!(out.status, 0);
    }

    #[cfg(unix)]
    #[test]
    fn an_endless_writer_times_out_instead_of_reporting_a_status() {
        // Draining past the cap keeps the child unblocked, so a generator that
        // never stops writing is bounded by its deadline, not by a fake status.
        // Memory still stops at the cap.
        let started = std::time::Instant::now();
        let err = execute_full(
            "sh",
            &[
                "-c".into(),
                "while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done".into(),
            ],
            "/",
            &[],
            Duration::from_millis(300),
        )
        .expect_err("an endless writer has no exit status to report");
        assert_eq!(err, CommandError::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waited {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn execute_full_polls_on_unix_instead_of_spawning_a_waiter() {
        let src = include_str!("process.rs");
        let start = src.find("fn wait_child_output(").expect("wait_child_output");
        let rest = &src[start..];
        let unix = rest.find("fn wait_child_output_unix").expect("wait_child_output_unix");
        let dispatch = &rest[..unix];
        assert!(
            dispatch.contains("wait_child_output_unix"),
            "JS executeCommand must poll on unix, not wait_with_output on a helper thread"
        );
        assert!(
            !dispatch.contains("thread::spawn"),
            "wait_child_output must not spawn a waiter on unix"
        );
        let unix_fn = &rest[unix..];
        let end = unix_fn.find("fn command_output").unwrap_or(unix_fn.len());
        assert!(
            !unix_fn[..end].contains("thread::spawn"),
            "the unix wait_child_output path must not spawn a waiter thread"
        );
    }

    #[test]
    fn windows_execute_full_caps_while_reading_on_a_helper_thread() {
        let src = include_str!("process.rs");
        let start = src
            .find("fn wait_child_output_threaded")
            .expect("wait_child_output_threaded");
        let rest = &src[start..];
        let end = rest.find("/// [`execute`]").unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("thread::spawn") && body.contains("read_capped"),
            "Windows executeCommand still uses a waiter thread; do not claim PeekNamedPipe poll"
        );
        assert!(
            !body.contains("wait_with_output") && !body.contains("stdout.truncate(MAX_STDOUT)"),
            "Windows must cap as it reads, not buffer the whole pipe then truncate"
        );
        let child_start = src.find("fn wait_child_threaded").expect("wait_child_threaded");
        let child = &src[child_start..];
        let child_end = child.find("fn reap(").unwrap_or(child.len());
        assert!(
            child[..child_end].contains("read_capped") && !child[..child_end].contains("wait_with_output"),
            "Windows generator execute must cap stdout while reading too"
        );
        let capped = src.find("fn read_capped").expect("read_capped");
        let capped_body = &src[capped..];
        let capped_end = capped_body
            .find("fn wait_child_output_threaded")
            .unwrap_or(capped_body.len());
        let capped_body = &capped_body[..capped_end];
        assert!(
            capped_body.contains("saturating_sub") && !capped_body.contains("if buf.len() >= MAX_STDOUT"),
            "read_capped must drain past the cap; stopping blocks the child and loses its status"
        );
    }

    #[test]
    fn read_capped_drains_past_the_cap_so_the_child_can_exit() {
        // Same contract as unix `drain_stdout`: keep the last-`MAX_STDOUT` window
        // in memory, but consume the rest so a blocked writer can finish.
        let extra = 64 * 1024;
        let input = vec![b'x'; MAX_STDOUT + extra];
        let mut cursor = std::io::Cursor::new(input);
        let out = read_capped(&mut cursor);
        assert_eq!(out.len(), MAX_STDOUT, "kept bytes stop at the cap");
        assert_eq!(
            cursor.position() as usize,
            MAX_STDOUT + extra,
            "bytes past the cap must be read and discarded"
        );
        assert!(out.iter().all(|&b| b == b'x'));
    }

    #[cfg(unix)]
    #[test]
    fn returns_stdout_after_pipe_closes_even_if_child_hangs() {
        let started = std::time::Instant::now();
        let out = execute(
            "sh",
            &["-c".into(), "printf hello-eof; exec 1>&-; sleep 2".into()],
            "/",
            Duration::from_millis(200),
        );
        assert_eq!(out, "hello-eof");
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "waited {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn strict_isolated_execution_rejects_nonzero_status() {
        assert_eq!(
            try_execute_isolated_success(
                "sh",
                &["-c".into(), "printf misleading; exit 7".into()],
                "/",
                Duration::from_millis(500),
            ),
            None
        );
    }
}
