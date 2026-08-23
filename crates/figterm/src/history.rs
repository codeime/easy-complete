use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use fig_settings::history::{HistoryColumn, Order, OrderBy, WhereExpression};
use flume::Sender;
use tracing::{error, trace};

use crate::HOSTNAME;

#[derive(Debug)]
pub struct HistoryQueryParams {
    pub limit: usize,
}

pub enum HistoryCommand {
    Insert(alacritty_terminal::term::CommandInfo),
    Query(
        HistoryQueryParams,
        Sender<Option<Vec<fig_settings::history::CommandInfo>>>,
    ),
}

pub type HistorySender = Sender<HistoryCommand>;

static HISTORY_SENDER: OnceLock<HistorySender> = OnceLock::new();
static HISTORY_BUILDER_RUNS: AtomicUsize = AtomicUsize::new(0);

/// History SQLite must not occupy one of figterm's two Tokio workers.
///
/// A forever `recv` on `spawn_blocking` holds a Tokio blocking-pool thread
/// for the life of the tab — `ecterm` multiplies by tab, so that was a
/// parked OS thread per terminal. A dedicated `std::thread` keeps SQLite
/// off the runtime without consuming the blocking pool.
///
/// The writer is process-wide: callers clone the sender. Starting a thread
/// on every `spawn_history_task` used to mean one forever Global SQLite
/// client per call.
pub fn spawn_history_task() -> HistorySender {
    HISTORY_SENDER
        .get_or_init(|| {
            trace!("Spawning history task");

            let (sender, receiver) = flume::bounded::<HistoryCommand>(64);

            std::thread::Builder::new()
                .name("ecterm-history".into())
                .spawn(move || {
                    let history = fig_settings::history::History::new();

                    while let Ok(command) = receiver.recv() {
                        match command {
                            HistoryCommand::Insert(command) => {
                                let command_info = fig_settings::history::CommandInfo {
                                    command: command.command,
                                    shell: command.shell,
                                    pid: command.pid,
                                    session_id: command.session_id,
                                    cwd: command.cwd,
                                    start_time: command.start_time,
                                    end_time: command.end_time,
                                    hostname: command.username.as_deref().and_then(|username| {
                                        HOSTNAME.as_deref().map(|hostname| format!("{username}@{hostname}"))
                                    }),
                                    exit_code: command.exit_code,
                                };

                                if let Err(err) = history.insert_command_history(&command_info, true) {
                                    error!(%err, "Failed to insert command into history");
                                }
                            },
                            HistoryCommand::Query(query, sender) => {
                                match history.rows(
                                    Some(WhereExpression::NotNull(HistoryColumn::ExitCode)),
                                    vec![OrderBy::new(HistoryColumn::Id, Order::Desc)],
                                    query.limit,
                                    0,
                                ) {
                                    Ok(rows) => {
                                        if let Err(err) = sender.send(Some(rows)) {
                                            error!(%err, "Failed to send history query result");
                                        }
                                    },
                                    Err(err) => {
                                        error!(%err, "Failed to query history");
                                        if let Err(err) = sender.send(None) {
                                            error!(%err, "Failed to send history query result");
                                        }
                                    },
                                }
                            },
                        }
                    }
                })
                // OnceLock stores this sender. Logging a spawn failure and still
                // caching a disconnected sender would drop every later insert.
                .expect("failed to spawn ecterm-history thread");

            HISTORY_BUILDER_RUNS.fetch_add(1, Ordering::Relaxed);
            sender
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::{HISTORY_BUILDER_RUNS, spawn_history_task};

    #[test]
    fn history_sqlite_runs_on_a_std_thread() {
        let src = include_str!("history.rs");
        let start = src.find("pub fn spawn_history_task").expect("spawn_history_task");
        let body = &src[start..];
        let end = body.find("#[cfg(test)]").unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.contains("std::thread::Builder"),
            "history SQLite must not occupy a Tokio blocking-pool thread for the life of the tab"
        );
        assert!(
            !body.contains("spawn_blocking"),
            "a forever recv on spawn_blocking holds a blocking-pool thread per tab"
        );
        assert!(!body.contains("recv_async"), "the history loop is a blocking recv");
        assert!(body.contains("receiver.recv()"), "SQLite runs on a blocking recv loop");
        let production = src.split("#[cfg(test)]").next().expect("production");
        assert!(
            production.contains("OnceLock") && body.contains("get_or_init"),
            "tabs must clone one Sender, not start a history thread per call"
        );
        assert!(body.contains(".clone()"), "callers receive a cloned Sender");
        assert!(
            body.contains(".expect(") && !body.contains("Failed to spawn history thread"),
            "spawn failure must not store a disconnected sender in the OnceLock"
        );
    }

    #[test]
    fn history_thread_builder_runs_once() {
        let first = spawn_history_task();
        let second = spawn_history_task();
        assert_eq!(
            HISTORY_BUILDER_RUNS.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "N spawn_history_task calls must start one ecterm-history thread"
        );
        assert!(
            first.receiver_count() > 0 && second.receiver_count() > 0,
            "OnceLock must keep a live history receiver, not a disconnected sender"
        );
    }
}
