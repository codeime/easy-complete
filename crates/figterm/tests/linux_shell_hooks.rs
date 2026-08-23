//! M2: `ecterm` plus a shell post-hook (OSC 697) drives the engine on Linux
//! without a desktop caret or overlay.
//!
//! The wrap gate (`ec _ should-figterm-launch`) is pinned in `ec_cli`. This
//! test execs `ecterm` the way a successful wrap does: a real TTY, a child
//! bash/zsh/fish that emits the same OSC 697 the post hooks emit, and a mock
//! desktop `remote.sock`. Completions are printed to the test stdout.

#![cfg(unix)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fig_ipc::{LocalListener, RecvMessage, SendMessage};
use fig_proto::remote::{Clientbound, Hostbound, clientbound, hostbound};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tempfile::TempDir;

const SESSION: &str = "m2hooks001";

fn ecterm_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("ecterm")
}

fn specs_ir() -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bundle/specs-ir");
    dir.join("index.json").is_file().then_some(dir)
}

fn which_shell(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find_map(|dir| {
            let candidate = dir.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}

fn write_hook_rc(dir: &Path, shell: &str, session: &str) -> PathBuf {
    match shell {
        "bash" => {
            let path = dir.join(".bashrc");
            std::fs::write(
                &path,
                format!(
                    r#"
printf '\033]697;Shell=bash\007'
printf '\033]697;ShellPath=%s\007' /bin/bash
PS1='\[\033]697;StartPrompt\007\]> \[\033]697;EndPrompt\007\033]697;NewCmd={session}\007\]'
"#
                ),
            )
            .unwrap();
            path
        },
        "zsh" => {
            let path = dir.join(".zshrc");
            std::fs::write(
                &path,
                format!(
                    r#"
print -n $'\033]697;Shell=zsh\007'
print -n $'\033]697;ShellPath=/bin/zsh\007'
PROMPT=$'%{{\033]697;StartPrompt\007%}}> %{{\033]697;EndPrompt\007\033]697;NewCmd={session}\007%}}'
"#
                ),
            )
            .unwrap();
            path
        },
        "fish" => {
            let path = dir.join("config.fish");
            std::fs::write(
                &path,
                format!(
                    r#"
printf '\033]697;Shell=fish\007'
printf '\033]697;ShellPath=/usr/bin/fish\007'
function fish_prompt
    printf '\033]697;StartPrompt\007'
    printf '> '
    printf '\033]697;EndPrompt\007'
    printf '\033]697;NewCmd={session}\007'
end
"#
                ),
            )
            .unwrap();
            path
        },
        other => panic!("unsupported shell {other}"),
    }
}

fn output_text(output: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&output.lock().unwrap()).into_owned()
}

fn looks_like_git_ch(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "git ch" || trimmed.ends_with("git ch") || trimmed.contains("git ch")
}

async fn collect_git_ch_buffer(shell: &str) -> String {
    let shell_bin = which_shell(shell).unwrap_or_else(|| {
        panic!("skip-or-fail: {shell} is not installed");
    });
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let runtime = home.join("run");
    let ecrun = runtime.join("ecrun");
    std::fs::create_dir_all(&ecrun).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ecrun, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let socket = ecrun.join("remote.sock");
    let rc = write_hook_rc(home, shell, SESSION);

    let mut listener = LocalListener::bind(&socket).await.expect("bind remote.sock");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(ecterm_bin());
    cmd.arg("--");
    cmd.arg(&shell_bin);
    match shell {
        "bash" => {
            // Interactive bash reads $HOME/.bashrc. Do not pass GNU long
            // options after clap's `--` terminator; a stray `--` is a bash error.
            let _ = rc;
            cmd.arg("-i");
        },
        "zsh" => {
            // `zsh -f` skips ZDOTDIR/.zshrc, so OSC 697 would never fire.
            let _ = rc;
            cmd.arg("-i");
        },
        "fish" => {
            cmd.arg("--no-config");
            cmd.arg("-i");
            cmd.arg("-C");
            cmd.arg(format!("source {}", rc.display()));
        },
        _ => unreachable!(),
    }
    cmd.cwd(home);
    cmd.env_clear();
    cmd.env("HOME", home);
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("XDG_RUNTIME_DIR", &runtime);
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
    cmd.env("XDG_DATA_HOME", home.join(".local/share"));
    cmd.env("XDG_STATE_HOME", home.join(".local/state"));
    cmd.env("XDG_CACHE_HOME", home.join(".cache"));
    cmd.env("SHELL", &shell_bin);
    cmd.env("Q_SHELL", &shell_bin);
    cmd.env("MOCK_QTERM_SESSION_ID", SESSION);
    cmd.env("TERM", "xterm-256color");
    cmd.env("HISTFILE", "");
    cmd.env("ZDOTDIR", home);
    cmd.env("USER", "m2");
    cmd.env("LANG", "C.UTF-8");

    let mut child = pair.slave.spawn_command(cmd).expect("spawn ecterm");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader_out = output.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            reader_out.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let mut writer = pair.master.take_writer().expect("pty writer");

    let mut stream = tokio::time::timeout(Duration::from_secs(8), listener.accept())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{shell}: ecterm did not connect to remote.sock\npty:\n{}",
                output_text(&output)
            )
        })
        .expect("accept remote.sock");

    let handshake_deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if tokio::time::Instant::now() > handshake_deadline {
            let _ = child.kill();
            panic!("{shell}: no handshake from ecterm\npty:\n{}", output_text(&output));
        }
        let message = tokio::time::timeout(Duration::from_secs(2), stream.recv_message::<Hostbound>())
            .await
            .unwrap_or_else(|_| panic!("{shell}: timed out reading handshake\npty:\n{}", output_text(&output)))
            .expect("recv handshake");
        match message.and_then(|m| m.packet) {
            Some(hostbound::Packet::Handshake(_)) => {
                stream
                    .send_message(Clientbound {
                        packet: Some(clientbound::Packet::HandshakeResponse(clientbound::HandshakeResponse {
                            success: true,
                        })),
                    })
                    .await
                    .expect("handshake response");
                break;
            },
            Some(_) => {},
            None => {
                let _ = child.kill();
                panic!(
                    "{shell}: remote socket EOF before handshake\npty:\n{}",
                    output_text(&output)
                );
            },
        }
    }

    let buffers: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let server_buffers = buffers.clone();
    tokio::spawn(async move {
        loop {
            match stream.recv_message::<Hostbound>().await {
                Ok(Some(message)) => {
                    if let Some(hostbound::Packet::Request(req)) = message.packet {
                        if let Some(hostbound::request::Request::EditBuffer(edit)) = req.request {
                            server_buffers.lock().unwrap().push(edit.text);
                        }
                    }
                },
                _ => break,
            }
        }
    });

    let prompt_deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        if tokio::time::Instant::now() > prompt_deadline {
            let _ = child.kill();
            panic!("{shell}: no prompt\npty:\n{}", output_text(&output));
        }
        let screen = output_text(&output);
        if screen.contains("> ") || screen.contains("% ") || screen.contains("$ ") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    for byte in b"git ch" {
        writer.write_all(&[*byte]).expect("type");
        writer.flush().ok();
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let buffer = loop {
        if tokio::time::Instant::now() > deadline {
            let dump = output_text(&output);
            let seen = buffers.lock().unwrap().clone();
            let _ = child.kill();
            panic!("{shell}: timed out waiting for edit buffer 'git ch'\npty:\n{dump}\nbuffers:{seen:?}");
        }
        let snapshot = buffers.lock().unwrap().clone();
        if let Some(hit) = snapshot.into_iter().rev().find(|text| looks_like_git_ch(text)) {
            break hit;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let _ = child.kill();
    let _ = child.wait();
    buffer
}

async fn assert_checkout(buffer: &str, shell: &str) {
    let Some(specs) = specs_ir() else {
        panic!("bundle/specs-ir/index.json is missing; cannot prove engine complete");
    };
    let engine = ec_engine::EngineClient::spawn(specs).expect("spawn engine");
    let result = engine
        .complete(ec_engine::CompleteRequest {
            buffer: buffer.trim().to_string(),
            cwd: "/tmp".into(),
            include_history: false,
            ..ec_engine::CompleteRequest::default()
        })
        .await
        .expect("engine complete");
    let names: Vec<&str> = result.suggestions.iter().map(|row| row.name.as_str()).collect();
    println!("{shell} buffer={buffer:?} suggestions={names:?}");
    assert!(
        names.iter().any(|name| name.contains("checkout")),
        "{shell}: engine must suggest checkout for {buffer:?}, got {names:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ecterm_bash_hook_drives_engine_without_overlay() {
    if which_shell("bash").is_none() {
        eprintln!("skip: bash is not installed");
        return;
    }
    let buffer = collect_git_ch_buffer("bash").await;
    assert_checkout(&buffer, "bash").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ecterm_zsh_hook_drives_engine_without_overlay() {
    if which_shell("zsh").is_none() {
        eprintln!("skip: zsh is not installed");
        return;
    }
    let buffer = collect_git_ch_buffer("zsh").await;
    assert_checkout(&buffer, "zsh").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ecterm_fish_hook_drives_engine_without_overlay() {
    if which_shell("fish").is_none() {
        eprintln!("skip: fish is not installed");
        return;
    }
    let buffer = collect_git_ch_buffer("fish").await;
    assert_checkout(&buffer, "fish").await;
}
