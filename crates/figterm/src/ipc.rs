//! Local figterm socket and remote desktop IPC.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Result;
use fig_ipc::{BufferedReader, RecvMessage, SendMessage};
use fig_proto::FigProtobufEncodable;
use fig_proto::figterm::{FigtermRequestMessage, FigtermResponseMessage};
use fig_proto::remote::hostbound::Handshake;
use fig_proto::remote::{Clientbound, Hostbound, clientbound, hostbound};
use fig_util::{PTY_BINARY_NAME, directories, gen_hex_string};
use flume::{Receiver, Sender, unbounded};
use pin_project::pin_project;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::join;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, info, trace};

use crate::MainLoopEvent;

#[allow(dead_code)]
#[pin_project(project = MessageSourceProj)]
enum MessageSource {
    IpcStream(#[pin] tokio::io::ReadHalf<fig_ipc::IpcStream>),
    ChildStdout(#[pin] ChildStdout),
}

impl AsyncRead for MessageSource {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.project() {
            MessageSourceProj::IpcStream(stream) => stream.poll_read(cx, buf),
            MessageSourceProj::ChildStdout(stdout) => stdout.poll_read(cx, buf),
        }
    }
}

#[allow(dead_code)]
#[pin_project(project = MessageSinkProj)]
enum MessageSink {
    IpcStream(#[pin] tokio::io::WriteHalf<fig_ipc::IpcStream>),
    ChildStdin(#[pin] ChildStdin),
}

impl AsyncWrite for MessageSink {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, io::Error>> {
        match self.project() {
            MessageSinkProj::IpcStream(stream) => stream.poll_write(cx, buf),
            MessageSinkProj::ChildStdin(stdin) => stdin.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            MessageSinkProj::IpcStream(stream) => stream.poll_flush(cx),
            MessageSinkProj::ChildStdin(stdin) => stdin.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        match self.project() {
            MessageSinkProj::IpcStream(stream) => stream.poll_shutdown(cx),
            MessageSinkProj::ChildStdin(stdin) => stdin.poll_shutdown(cx),
        }
    }
}

async fn get_forwarded_stream() -> Result<(MessageSource, MessageSink, Option<JoinHandle<()>>)> {
    #[cfg(target_os = "linux")]
    if fig_util::system_info::in_wsl() {
        use std::process::Stdio;

        use anyhow::Context as AnyhowContext;

        let host_cli = format!("{}.exe", fig_util::CLI_BINARY_NAME);
        let mut child = match tokio::process::Command::new(&host_cli)
            .args(["_", "stream-from-socket"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => tokio::process::Command::new("fig.exe")
                .args(["_", "stream-from-socket"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        };

        let stdin = child.stdin.take().context("Failed to open stdin")?;
        let stdout = child.stdout.take().context("Failed to open stdout")?;

        let child_task = tokio::spawn(async move {
            if let Err(err) = child.wait().await {
                error!(%err, "Error waiting for child");
            }
        });

        return Ok((
            MessageSource::ChildStdout(stdout),
            MessageSink::ChildStdin(stdin),
            Some(child_task),
        ));
    }

    let socket = directories::remote_socket_path()?;
    let stream = fig_ipc::socket_connect_timeout(&socket, Duration::from_secs(5)).await?;
    let (reader, writer) = tokio::io::split(stream);
    Ok((MessageSource::IpcStream(reader), MessageSink::IpcStream(writer), None))
}

/// Spawns a local unix socket for communicating with figterm on a local machine
pub async fn spawn_figterm_ipc(
    session_id: impl std::fmt::Display,
) -> Result<Receiver<(FigtermRequestMessage, Sender<FigtermResponseMessage>)>> {
    trace!("Spawning incoming receiver");

    let (incoming_tx, incoming_rx) = unbounded();

    let socket_path = directories::figterm_socket_path(session_id)?;
    if let Some(parent) = socket_path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            error!(%err, "Failed to create {PTY_BINARY_NAME} socket directory");
        }

        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, Permissions::from_mode(0o700))?;
        }
    }

    let mut socket_listener = fig_ipc::LocalListener::bind(&socket_path).await?;

    tokio::spawn(async move {
        loop {
            if let Ok(stream) = socket_listener.accept().await {
                let incoming_tx = incoming_tx.clone();

                let (read_half, mut write_half) = tokio::io::split(stream.into_inner());
                let (response_tx, response_rx) = unbounded::<FigtermResponseMessage>();

                tokio::spawn(async move {
                    let mut read_half = BufferedReader::new(read_half);
                    let mut rx_thread = tokio::spawn(async move {
                        loop {
                            match read_half.recv_message::<FigtermRequestMessage>().await {
                                Ok(Some(message)) => {
                                    // debug!("Received message: {message:?}");
                                    if let Err(err) = incoming_tx.send_async((message, response_tx.clone())).await {
                                        error!(%err, "Sender error");
                                        break;
                                    }
                                },
                                Ok(None) => {
                                    debug!("Received EOF");
                                    break;
                                },
                                Err(err) => {
                                    error!("Error receiving message: {err}");
                                    break;
                                },
                            }
                        }
                    });

                    loop {
                        tokio::select! {
                            // Break once the rx_thread quits
                            _ = &mut rx_thread => break,
                            res = response_rx.recv_async() => {
                                match res {
                                    Ok(response) => {
                                        match response.encode_fig_protobuf() {
                                            Ok(protobuf) => {
                                                if let Err(err) = write_half.write_all(&protobuf).await {
                                                    error!(%err, "Failed to send response");
                                                    break;
                                                }
                                            },
                                            Err(err) => error!(%err, "Failed to encode protobuf")
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                    }
                });
            }
        }
    });

    Ok(incoming_rx)
}

/// Connects to the desktop app and allows for a remote connection from remote hosts
pub async fn spawn_remote_ipc(
    session_id: String,
    parent_id: Option<String>,
    main_loop_sender: Sender<MainLoopEvent>,
) -> Result<(Sender<Hostbound>, Receiver<Clientbound>, oneshot::Sender<()>)> {
    let (stop_ipc_tx, mut stop_ipc_rx) = oneshot::channel::<()>();
    let (outgoing_tx, outgoing_rx) = unbounded::<Hostbound>();
    let (incoming_tx, incoming_rx) = unbounded::<Clientbound>();

    tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let secret = gen_hex_string();

        loop {
            interval.tick().await;
            tokio::select! {
                _ = &mut stop_ipc_rx => {
                    break;
                }
                res = get_forwarded_stream() => {
                    let (reader, mut writer, child) = match res {
                        Ok((reader, writer, child)) => (reader, writer, child),
                        Err(err) => {
                            error!("failed to get forwarded stream: {err}");
                            continue;
                        },
                    };

                    let mut reader = BufferedReader::new(reader);
                    info!("Attempting handshake...");
                    if let Err(err) = writer.send_message(Hostbound {
                        packet: Some(hostbound::Packet::Handshake(Handshake {
                            id: session_id.clone(),
                            parent_id: parent_id.clone(),
                            secret: secret.clone(),
                        })),
                    })
                    .await
                    {
                        error!(%err, "error sending handshake");
                        continue;
                    }
                    let mut handshake_success = false;
                    info!("Awaiting handshake response...");
                    while let Some(message) = reader.recv_message::<Clientbound>().await.unwrap_or_else(|err| {
                        error!(%err, "failed receiving handshake response");
                        None
                    }) {
                        if let Some(clientbound::Packet::HandshakeResponse(response)) = message.packet {
                            handshake_success = response.success;
                            break;
                        }
                    }
                    if !handshake_success {
                        error!("failed performing handshake");
                        continue;
                    }
                    info!("Handshake succeeded");

                    // Whatever we were intercepting belonged to an overlay this
                    // desktop process is not showing, so start unlocked. Without
                    // this a desktop restart while the list was up leaves the tab
                    // swallowing Enter and Tab. `reset` clears the intercept
                    // flags and `window_visible`; leftover key bindings stay
                    // loaded but cannot fire while intercept is off.
                    if let Err(err) = main_loop_sender.send(MainLoopEvent::UnlockInterception) {
                        error!(%err, "Sender error");
                    }
                    // Drop frames queued while nothing was connected: they
                    // describe a buffer this desktop process never saw. The main
                    // loop has to forget them too, or its duplicate check will
                    // suppress the resend.
                    outgoing_rx.drain();
                    if let Err(err) = main_loop_sender.send(MainLoopEvent::ResetSentEditBuffer) {
                        error!(%err, "Sender error");
                    }
                    let outgoing_rx = outgoing_rx.clone();
                    let main_loop_sender = main_loop_sender.clone();
                    let outgoing_task = tokio::spawn(async move {
                        while let Ok(message) = outgoing_rx.recv_async().await {
                            trace!(?message, "Sending remote message");
                            match writer.send_message(message).await {
                                Ok(()) => {
                                    if let Err(err) = writer.flush().await {
                                        error!(%err, "Failed to flush socket");
                                        send_remote_unlock(&main_loop_sender);
                                    }
                                }
                                Err(err) => {
                                    error!(%err, "Failed to send message");
                                    send_remote_unlock(&main_loop_sender);
                                    let _ = writer.shutdown().await;
                                    break;
                                }
                            }
                        }
                        debug!("outgoing_task exited");
                    });

                    // receive incoming messages
                    let incoming_tx = incoming_tx.clone();
                    let incoming_task = tokio::spawn(async move {
                        while let Some(message) = reader.recv_message().await.unwrap_or_else(|err| {
                            error!("failed receiving message from host: {err}");
                            None
                        }) {
                            trace!(?message, "Received remote message");
                            if let Err(err) = incoming_tx.send(message) {
                                error!("no more listeners for incoming messages: {err}");
                                break;
                            }
                        }
                        debug!("incoming_task exited");
                    });

                    if let Some(child) = child {
                        let _ = join!(outgoing_task, incoming_task, child);
                    } else {
                        let _ = join!(outgoing_task, incoming_task);
                    }
                }
            }
        }
    });

    Ok((outgoing_tx, incoming_rx, stop_ipc_tx))
}

fn send_remote_unlock(main_loop_sender: &Sender<MainLoopEvent>) {
    if let Err(err) = main_loop_sender.send(MainLoopEvent::Insert {
        insert: Vec::new(),
        unlock: true,
        bracketed: false,
        execute: false,
    }) {
        error!(%err, "Sender error");
    }
}

#[cfg(test)]
mod tests {
    use super::send_remote_unlock;
    use crate::MainLoopEvent;

    #[test]
    fn local_ipc_incoming_send_does_not_unwrap() {
        let src = include_str!("ipc.rs");
        let start = src.find("pub async fn spawn_figterm_ipc").expect("spawn_figterm_ipc");
        let rest = &src[start..];
        let end = rest.find("pub async fn spawn_remote_ipc").unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            !body.contains(".unwrap()"),
            "a closed incoming_tx must log like EventHandler, not panic ecterm"
        );
        assert!(
            body.contains("Sender error"),
            "local IPC send failure should log Sender error"
        );
    }

    #[test]
    fn ssh_flush_main_loop_sender_does_not_unwrap() {
        let src = include_str!("ipc.rs");
        let start = src.find("pub async fn spawn_remote_ipc").expect("spawn_remote_ipc");
        let rest = &src[start..];
        let end = rest.find("fn send_remote_unlock").unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            !body.contains(".unwrap()"),
            "SSH flush must log a closed main_loop_sender like EventHandler, not unwrap"
        );
        assert!(
            body.contains("send_remote_unlock"),
            "flush/send failures must go through send_remote_unlock"
        );
    }

    #[test]
    fn a_successful_handshake_clears_carried_over_state() {
        // figterm outlives the desktop process. Both the key interceptor and the
        // "already sent" edit buffer describe a desktop that is gone, and the
        // queued frames are dropped here, so all three have to be reset in the
        // same place.
        let src = include_str!("ipc.rs");
        let wait = src.find("Awaiting handshake response").expect("await response");
        let wait_body = &src[wait..src.find("info!(\"Handshake succeeded\")").expect("handshake succeeded")];
        assert!(
            wait_body.contains("HandshakeResponse") && wait_body.contains("break"),
            "the handshake loop must only accept HandshakeResponse; other frames are dropped"
        );
        let start = src.find("info!(\"Handshake succeeded\")").expect("handshake succeeded");
        let rest = &src[start..];
        let end = rest.find("let outgoing_task").expect("outgoing_task");
        let body = &rest[..end];
        assert!(
            body.contains("MainLoopEvent::UnlockInterception"),
            "a reconnect must unlock interception; the new desktop shows no overlay yet"
        );
        assert!(
            body.contains("outgoing_rx.drain()") && body.contains("MainLoopEvent::ResetSentEditBuffer"),
            "draining queued frames must also clear the duplicate-suppression state"
        );
        let drain = body.find("outgoing_rx.drain()").expect("drain");
        let reset = body.find("MainLoopEvent::ResetSentEditBuffer").expect("reset");
        assert!(drain < reset, "reset the sent-buffer marker after dropping the queue");
    }

    #[test]
    fn closed_ssh_flush_sender_does_not_panic() {
        let (tx, rx) = flume::unbounded::<MainLoopEvent>();
        drop(rx);
        send_remote_unlock(&tx);
    }
}
