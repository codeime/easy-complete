use std::io::{
    Write,
    stdout,
};
use std::process::ExitCode;
use std::time::Duration;

use fastab_ipc::{
    BufferedUnixStream,
    SendMessage,
    SendRecvMessage,
};
use fastab_proto::figterm::figterm_request_message::Request;
use fastab_proto::figterm::figterm_response_message::Response;
use fastab_proto::figterm::{
    FigtermRequestMessage,
    FigtermResponseMessage,
    InlineShellCompletionAcceptRequest,
    InlineShellCompletionRequest,
    InlineShellCompletionResponse,
};
use fastab_util::env_var::QTERM_SESSION_ID;
use tracing::error;

macro_rules! unwrap_or_exit {
    ($expr:expr, $err_msg:expr) => {
        match $expr {
            Ok(value) => value,
            Err(err) => {
                error!(%err, $err_msg);
                return ExitCode::FAILURE;
            }
        }
    };
}

pub(super) async fn inline_shell_completion(buffer: String) -> ExitCode {
    let session_id = unwrap_or_exit!(std::env::var(QTERM_SESSION_ID), "Failed to get session ID");

    let fastabterm_socket_path = unwrap_or_exit!(
        fastab_util::directories::fastabterm_socket_path(&session_id),
        "Failed to get fastabterm socket path"
    );

    let mut conn = unwrap_or_exit!(
        BufferedUnixStream::connect(fastabterm_socket_path).await,
        "Failed to connect to fastabterm"
    );

    match conn
        .send_recv_message_timeout(
            FigtermRequestMessage {
                request: Some(Request::InlineShellCompletion(InlineShellCompletionRequest {
                    buffer: buffer.clone(),
                })),
            },
            Duration::from_secs(5),
        )
        .await
    {
        Ok(Some(FigtermResponseMessage {
            response:
                Some(Response::InlineShellCompletion(InlineShellCompletionResponse {
                    insert_text: Some(insert_text),
                })),
        })) => {
            let _ = writeln!(stdout(), "{buffer}{insert_text}");
            ExitCode::SUCCESS
        },
        Ok(res) => {
            error!(?res, "Unexpected response from fastabterm");
            ExitCode::FAILURE
        },
        Err(err) => {
            error!(%err, "Failed to get inline shell completion from fastabterm");
            ExitCode::FAILURE
        },
    }
}

pub(super) async fn inline_shell_completion_accept(buffer: String, suggestion: String) -> ExitCode {
    let session_id = unwrap_or_exit!(std::env::var(QTERM_SESSION_ID), "Failed to get session ID");

    let fastabterm_socket_path = unwrap_or_exit!(
        fastab_util::directories::fastabterm_socket_path(&session_id),
        "Failed to get fastabterm socket path"
    );

    let mut conn = unwrap_or_exit!(
        BufferedUnixStream::connect(fastabterm_socket_path).await,
        "Failed to connect to fastabterm"
    );

    match conn
        .send_message(FigtermRequestMessage {
            request: Some(Request::InlineShellCompletionAccept(
                InlineShellCompletionAcceptRequest { buffer, suggestion },
            )),
        })
        .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(%err, "Failed to send InlineShellCompletionAccept to fastabterm");
            ExitCode::FAILURE
        },
    }
}
