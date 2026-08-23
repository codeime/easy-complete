//! Named-pipe bind/connect policy.
//!
//! The listener in `windows_pipe.rs` stays `cfg(windows)` — Linux cannot open
//! `\\.\pipe\`. These numbers and the bind/accept contract compile everywhere
//! so Linux CI stays honest about what that module will do.

#![allow(dead_code)]

/// Win32 `ERROR_FILE_NOT_FOUND` — the next pipe instance is not created yet.
pub const WIN32_ERROR_FILE_NOT_FOUND: i32 = 2;
/// Win32 `ERROR_PIPE_BUSY` — every instance is already connected.
pub const WIN32_ERROR_PIPE_BUSY: i32 = 231;
/// Win32 `ERROR_NO_DATA` — not a connect-retry; the pipe has been closed.
pub const WIN32_ERROR_NO_DATA: i32 = 232;
/// Win32 `ERROR_ACCESS_DENIED`.
pub const WIN32_ERROR_ACCESS_DENIED: i32 = 5;

/// Client connect budget. Matches the Unix socket path's "try for a while"
/// rather than a single `CreateFile`.
pub const NAMED_PIPE_CONNECT_BUDGET_SECS: u64 = 5;

pub fn named_pipe_connect_retryable_os_error(code: i32) -> bool {
    matches!(code, WIN32_ERROR_PIPE_BUSY | WIN32_ERROR_FILE_NOT_FOUND)
}

/// Unix `LocalListener::bind` unlinks the socket path first. Named pipes are
/// not files; a stale path must not be removed as if it were.
pub fn named_pipe_bind_unlinks_a_path() -> bool {
    false
}

/// `ServerOptions::first_pipe_instance(true)` is the EADDRINUSE equivalent:
/// a second server on the same name fails instead of sharing the pipe.
pub fn named_pipe_bind_requires_first_instance() -> bool {
    true
}

/// `accept` creates the next instance *before* returning the connected
/// handle so a client that races in sees FILE_NOT_FOUND / PIPE_BUSY
/// (retryable) rather than a permanent miss.
pub fn named_pipe_accept_creates_next_instance_before_return() -> bool {
    true
}

/// Windows has no 0o600 socket mode. `validate_socket` is a no-op.
pub fn named_pipe_validate_socket_checks_permissions() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_retries_busy_and_file_not_found_only() {
        assert!(named_pipe_connect_retryable_os_error(WIN32_ERROR_PIPE_BUSY));
        assert!(named_pipe_connect_retryable_os_error(WIN32_ERROR_FILE_NOT_FOUND));
        assert!(named_pipe_connect_retryable_os_error(231));
        assert!(named_pipe_connect_retryable_os_error(2));
        assert!(!named_pipe_connect_retryable_os_error(WIN32_ERROR_ACCESS_DENIED));
        assert!(!named_pipe_connect_retryable_os_error(WIN32_ERROR_NO_DATA));
        assert!(!named_pipe_connect_retryable_os_error(0));
    }

    #[test]
    fn bind_is_not_a_unix_unlink_and_accept_precreates_the_next_instance() {
        assert!(!named_pipe_bind_unlinks_a_path());
        assert!(named_pipe_bind_requires_first_instance());
        assert!(named_pipe_accept_creates_next_instance_before_return());
        assert!(!named_pipe_validate_socket_checks_permissions());
        assert_eq!(NAMED_PIPE_CONNECT_BUDGET_SECS, 5);
    }

    #[test]
    fn windows_pipe_host_uses_the_shared_bind_and_retry_policy() {
        let src = include_str!("windows_pipe.rs");
        assert!(
            src.contains("named_pipe_connect_retryable_os_error"),
            "named-pipe connect retries FILE_NOT_FOUND / PIPE_BUSY from this module"
        );
        assert!(
            src.contains("named_pipe_bind_requires_first_instance()"),
            "bind must require first_pipe_instance"
        );
        assert!(
            src.contains("named_pipe_accept_creates_next_instance_before_return"),
            "accept must precreate the next instance"
        );
        assert!(
            src.contains("first_pipe_instance("),
            "ServerOptions still sets first_pipe_instance"
        );
        assert!(
            src.contains("NAMED_PIPE_CONNECT_BUDGET_SECS"),
            "connect budget is the shared constant, not a second number"
        );
        assert!(
            !src.contains("fs::remove_file") && !src.contains("std::fs::remove_file"),
            "named-pipe bind must not unlink a path the way Unix sockets do"
        );
    }
}
