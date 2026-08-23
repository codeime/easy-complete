use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::ShellState;
use fig_proto::remote::Hostbound;
use fig_proto::remote_hooks::{hook_to_message, new_postexec_hook, new_preexec_hook, new_prompt_hook};
// use fig_telemetry::sentry::configure_scope;
use flume::Sender;
use tracing::level_filters::LevelFilter;
use tracing::{debug, error};

use crate::history::{HistoryCommand, HistorySender};
use crate::{INSERT_ON_NEW_CMD, MainLoopEvent, recover_mutex, shell_state_to_context};

pub struct EventHandler {
    socket_sender: Sender<Hostbound>,
    history_sender: HistorySender,
    main_loop_sender: Sender<MainLoopEvent>,
    csi_u_enabled: bool,
}

impl EventHandler {
    pub fn new(
        socket_sender: Sender<Hostbound>,
        history_sender: HistorySender,
        main_loop_sender: Sender<MainLoopEvent>,
    ) -> Self {
        Self {
            socket_sender,
            history_sender,
            main_loop_sender,
            csi_u_enabled: fig_settings::settings::get_bool_or("qterm.csi-u.enabled", false),
        }
    }
}

impl EventListener for EventHandler {
    fn send_event(&self, event: Event<'_>, shell_state: &ShellState) {
        debug!(?event, ?shell_state, "Handling event");
        match event {
            Event::Prompt => {
                let context = shell_state_to_context(shell_state);
                let hook = new_prompt_hook(Some(context));
                let message = hook_to_message(hook);

                let insert_on_new_cmd = recover_mutex(&INSERT_ON_NEW_CMD).take();

                if let Some(cwd) = &shell_state.local_context.current_working_directory {
                    if cwd.exists() {
                        std::env::set_current_dir(cwd).ok();
                    }
                }

                if let Some((text, bracketed, execute)) = insert_on_new_cmd {
                    if let Err(err) = self.main_loop_sender.send(MainLoopEvent::Insert {
                        insert: text.into_bytes(),
                        unlock: false,
                        bracketed,
                        execute,
                    }) {
                        error!(%err, "Sender error");
                    }
                }

                if let Err(err) = self.main_loop_sender.send(MainLoopEvent::SetImmediateMode(false)) {
                    error!(%err, "Sender error");
                }

                if let Err(err) = self.socket_sender.send(message) {
                    error!(%err, "Sender error");
                }

                if self.csi_u_enabled {
                    if let Err(err) = self.main_loop_sender.send(MainLoopEvent::SetCsiU) {
                        error!(%err, "Sender error");
                    }
                }
            },
            Event::PreExec => {
                let context = shell_state_to_context(shell_state);
                let hook = new_preexec_hook(Some(context));
                let message = hook_to_message(hook);

                if let Err(err) = self.main_loop_sender.send(MainLoopEvent::UnlockInterception) {
                    error!(%err, "Sender error");
                }
                if let Err(err) = self.main_loop_sender.send(MainLoopEvent::SetImmediateMode(true)) {
                    error!(%err, "Sender error");
                }

                if let Err(err) = self.socket_sender.send(message) {
                    error!(%err, "Sender error");
                }

                if self.csi_u_enabled {
                    if let Err(err) = self.main_loop_sender.send(MainLoopEvent::UnsetCsiU) {
                        error!(%err, "Sender error");
                    }
                }
            },
            Event::CommandInfo(command_info) => {
                let context = shell_state_to_context(shell_state);
                let hook = new_postexec_hook(context, command_info.command.clone(), command_info.exit_code);
                let message = hook_to_message(hook);
                if let Err(err) = self.socket_sender.send(message) {
                    error!(%err, "Sender error");
                }

                if let Err(err) = self.history_sender.send(HistoryCommand::Insert(command_info.clone())) {
                    error!(%err, "Sender error");
                }
            },
            Event::ShellChanged => {
                // let shell = &shell_state.local_context.shell;
                // configure_scope(|scope| {
                //     if let Some(shell) = shell {
                //         scope.set_tag("shell", shell);
                //     }
                // });
            },
        }
    }

    fn log_level_event(&self, level: Option<String>) {
        if let Err(err) = fig_log::set_log_level(level.unwrap_or_else(|| LevelFilter::INFO.to_string())) {
            error!(%err, "Failed to set log level");
        }
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::{Event, EventListener};
    use alacritty_terminal::term::ShellState;

    use super::EventHandler;

    #[test]
    fn main_loop_sender_does_not_unwrap() {
        let src = include_str!("event_handler.rs");
        let start = src.find("impl EventListener for EventHandler").expect("EventListener");
        let rest = &src[start..];
        let end = rest.find("#[cfg(test)]").unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            !body.contains(".unwrap()"),
            "main_loop_sender must log like socket/history, not unwrap"
        );
        assert!(
            body.contains("error!"),
            "a closed main-loop channel must log instead of panicking"
        );
    }

    #[test]
    fn closed_main_loop_sender_does_not_panic() {
        let (socket_tx, _socket_rx) = flume::unbounded();
        let (history_tx, _history_rx) = flume::unbounded();
        let (main_tx, main_rx) = flume::unbounded();
        drop(main_rx);
        let handler = EventHandler::new(socket_tx, history_tx, main_tx);
        let shell = ShellState::default();
        handler.send_event(Event::Prompt, &shell);
        handler.send_event(Event::PreExec, &shell);
    }
}
