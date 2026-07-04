#[cfg(target_os = "android")]
mod imp {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::path::PathBuf;
    use std::sync::Arc;

    use async_trait::async_trait;
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use tokio::sync::{Mutex, mpsc};
    use uuid::Uuid;

    use super::super::backend::{OpenBackendResult, TerminalBackend, TerminalBackendEvent};
    use super::super::session::{TerminalError, TerminalSize};
    use super::super::ssh::TerminalSshAuth;
    use super::super::ssh_known_hosts::{TerminalSshTrustStore, normalize_host};
    use crate::ssh::{SshClient, SshCredentials, SshError, shell_quote};

    const OUTPUT_CHUNK: usize = 16 * 1024;

    pub(crate) async fn open(
        host: String,
        ssh_port: u16,
        et_port: u16,
        username: String,
        auth: TerminalSshAuth,
        accept_unknown_host: bool,
        cwd: Option<String>,
        et_client_path: String,
        tmux_session: Option<String>,
        size: TerminalSize,
        trust_store: Option<Arc<TerminalSshTrustStore>>,
    ) -> Result<OpenBackendResult, TerminalError> {
        if et_client_path.trim().is_empty() {
            return Err(TerminalError::Backend {
                detail: "missing bundled ET client path".to_string(),
            });
        }

        let idpass = bootstrap_etterminal(
            &host,
            ssh_port,
            username.clone(),
            auth,
            accept_unknown_host,
            trust_store,
        )
        .await?;

        let session_name = tmux_session.unwrap_or_else(|| tin_tmux_session_name(&host));
        let command = tmux_command(&host, cwd.as_deref(), &session_name);
        let host_arg = if username.trim().is_empty() {
            format!("{host}:{et_port}")
        } else {
            format!("{username}@{host}:{et_port}")
        };

        let mut builder = CommandBuilder::new(et_client_path);
        builder.arg("--litter-skip-ssh");
        builder.arg("--litter-idpasskey");
        builder.arg(idpass);
        builder.arg("--keepalive");
        builder.arg("15");
        builder.arg("--command");
        builder.arg(command);
        builder.arg("--noexit");
        builder.arg(host_arg);
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");
        builder.env("HOME", "/data/data/com.sigkitten.litter.android.yolo/files");

        let rows = size.rows;
        let cols = size.cols;
        let pty_system = native_pty_system();
        let pair = tokio::task::spawn_blocking(move || {
            pty_system.openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
        })
        .await
        .map_err(|error| TerminalError::Backend {
            detail: format!("joining ET pty open task: {error}"),
        })?
        .map_err(|error| TerminalError::Backend {
            detail: format!("open ET pty: {error}"),
        })?;

        let child = pair.slave.spawn_command(builder).map_err(|error| TerminalError::Backend {
            detail: format!("spawn ET client: {error}"),
        })?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().map_err(|error| TerminalError::Backend {
            detail: format!("clone ET pty reader: {error}"),
        })?;
        let writer = pair.master.take_writer().map_err(|error| TerminalError::Backend {
            detail: format!("take ET pty writer: {error}"),
        })?;

        let (output_tx, output_rx) = mpsc::channel(256);
        spawn_reader(reader, output_tx.clone());
        spawn_waiter(child, output_tx);

        Ok((
            Arc::new(RemoteEtBackend {
                pty: Mutex::new(Some(pair.master)),
                writer: Mutex::new(writer),
            }),
            output_rx,
        ))
    }

    async fn bootstrap_etterminal(
        host: &str,
        ssh_port: u16,
        username: String,
        auth: TerminalSshAuth,
        accept_unknown_host: bool,
        trust_store: Option<Arc<TerminalSshTrustStore>>,
    ) -> Result<String, TerminalError> {
        let normalized = normalize_host(host);
        let pinned_fingerprint = trust_store
            .as_ref()
            .and_then(|store| store.lookup(&normalized, ssh_port));
        let credentials = SshCredentials {
            host: host.to_string(),
            port: ssh_port,
            username,
            auth: auth.into_ssh_auth(),
            unlock_macos_keychain: false,
        };
        let policy_pin = pinned_fingerprint.clone();
        let observed_fingerprint: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cb_observed = Arc::clone(&observed_fingerprint);
        let client = SshClient::connect(
            credentials,
            Box::new(move |fingerprint| {
                let pin = policy_pin.clone();
                let fingerprint = fingerprint.to_string();
                let observed = Arc::clone(&cb_observed);
                Box::pin(async move {
                    *observed.lock().await = Some(fingerprint.clone());
                    match pin {
                        Some(expected) => expected == fingerprint,
                        None => accept_unknown_host,
                    }
                })
            }),
        )
        .await
        .map_err(|error| map_ssh_error(error, &normalized, pinned_fingerprint.as_deref()))?;

        if let (Some(store), None) = (trust_store.as_ref(), &pinned_fingerprint)
            && accept_unknown_host
        {
            if let Some(fingerprint) = observed_fingerprint.lock().await.clone() {
                store.pin(normalized.clone(), ssh_port, fingerprint);
            }
        }

        let requested = format!(
            "{}/{}_xterm-256color",
            random_token(16),
            random_token(32)
        );
        let script = format!(
            "printf '%s\\n' {} | (command -v etterminal >/dev/null 2>&1 && etterminal --verbose=0 || /usr/local/bin/etterminal --verbose=0)",
            shell_quote(&requested)
        );
        let result = client.exec(&script).await.map_err(|error| {
            map_ssh_error(error, &normalized, pinned_fingerprint.as_deref())
        })?;
        if result.exit_code != 0 {
            return Err(TerminalError::Backend {
                detail: format!("remote etterminal failed: {}", first_nonempty(&result.stderr, &result.stdout)),
            });
        }
        parse_idpasskey(&result.stdout).ok_or_else(|| TerminalError::Backend {
            detail: format!(
                "remote etterminal did not return IDPASSKEY; output: {}",
                first_nonempty(&result.stdout, &result.stderr)
            ),
        })
    }

    struct RemoteEtBackend {
        pty: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
        writer: Mutex<Box<dyn Write + Send>>,
    }

    #[async_trait]
    impl TerminalBackend for RemoteEtBackend {
        async fn write(&self, data: &[u8]) -> Result<(), TerminalError> {
            let mut writer = self.writer.lock().await;
            writer.write_all(data).map_err(|error| TerminalError::Backend {
                detail: format!("ET write: {error}"),
            })
        }

        async fn resize(&self, size: TerminalSize) -> Result<(), TerminalError> {
            let mut pty = self.pty.lock().await;
            if let Some(pty) = pty.as_mut() {
                pty.resize(PtySize {
                    rows: size.rows,
                    cols: size.cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|error| TerminalError::Backend {
                    detail: format!("ET resize: {error}"),
                })?;
            }
            Ok(())
        }

        async fn close(&self) -> Result<(), TerminalError> {
            let _ = self.pty.lock().await.take();
            Ok(())
        }
    }

    fn spawn_reader(
        mut reader: Box<dyn Read + Send>,
        output_tx: mpsc::Sender<TerminalBackendEvent>,
    ) {
        std::thread::Builder::new()
            .name("litter-et-terminal-reader".to_string())
            .spawn(move || {
                let mut buf = vec![0u8; OUTPUT_CHUNK];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if output_tx
                                .blocking_send(TerminalBackendEvent::Bytes(buf[..n].to_vec()))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            let _ = output_tx.blocking_send(TerminalBackendEvent::Bytes(
                                format!("\r\n[et] read error: {error}\r\n").into_bytes(),
                            ));
                            break;
                        }
                    }
                }
            })
            .expect("spawn ET reader thread");
    }

    fn spawn_waiter(
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
        output_tx: mpsc::Sender<TerminalBackendEvent>,
    ) {
        std::thread::Builder::new()
            .name("litter-et-terminal-waiter".to_string())
            .spawn(move || {
                let code = match child.wait() {
                    Ok(status) => status.exit_code() as i32,
                    Err(error) => {
                        let _ = output_tx.blocking_send(TerminalBackendEvent::Bytes(
                            format!("\r\n[et] wait error: {error}\r\n").into_bytes(),
                        ));
                        -1
                    }
                };
                let _ = output_tx.blocking_send(TerminalBackendEvent::Exit(code));
            })
            .expect("spawn ET waiter thread");
    }

    fn tmux_command(host: &str, cwd: Option<&str>, session: &str) -> String {
        let start_dir = cwd
            .map(str::trim)
            .filter(|dir| !dir.is_empty())
            .unwrap_or("$HOME/Central_Command");
        format!(
            "dir={}; if [ ! -d \"$dir\" ]; then dir=\"$HOME\"; fi; exec tmux new-session -A -s {} -c \"$dir\"",
            shell_quote(start_dir),
            shell_quote(session),
        )
    }

    fn tin_tmux_session_name(host: &str) -> String {
        let short = host
            .trim()
            .split('.')
            .next()
            .unwrap_or("remote")
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '-' })
            .collect::<String>();
        format!("codex-litter@{}", if short.is_empty() { "remote" } else { &short })
    }

    fn random_token(len: usize) -> String {
        Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(len)
            .collect()
    }

    fn parse_idpasskey(output: &str) -> Option<String> {
        let idx = output.find("IDPASSKEY:")? + "IDPASSKEY:".len();
        let value = output[idx..].trim();
        value
            .split_whitespace()
            .next()
            .map(str::to_string)
            .filter(|value| value.contains('/'))
    }

    fn first_nonempty(a: &str, b: &str) -> String {
        let a = a.trim();
        if !a.is_empty() { a.to_string() } else { b.trim().to_string() }
    }

    fn map_ssh_error(error: SshError, host: &str, pinned: Option<&str>) -> TerminalError {
        match error {
            SshError::HostKeyVerification { fingerprint } => {
                let detail = match pinned {
                    Some(_) => format!("host-key-changed:{host}:{fingerprint}"),
                    None => format!("unknown-host:{fingerprint}"),
                };
                TerminalError::Backend { detail }
            }
            SshError::AuthFailed(detail) => TerminalError::Backend {
                detail: format!("auth-failed:{detail}"),
            },
            SshError::ConnectionFailed(detail) => TerminalError::Backend {
                detail: format!("connect-failed:{detail}"),
            },
            SshError::Timeout => TerminalError::Backend {
                detail: "connect-timeout".to_string(),
            },
            SshError::Disconnected => TerminalError::Backend {
                detail: "disconnected".to_string(),
            },
            SshError::ExecFailed { exit_code, stderr } => TerminalError::Backend {
                detail: format!("exec-failed:{exit_code}:{stderr}"),
            },
            SshError::PortForwardFailed(detail) => TerminalError::Backend {
                detail: format!("port-forward-failed:{detail}"),
            },
        }
    }
}

#[cfg(target_os = "android")]
pub(crate) use imp::open;

#[cfg(not(target_os = "android"))]
pub(crate) async fn open(
    _host: String,
    _ssh_port: u16,
    _et_port: u16,
    _username: String,
    _auth: super::ssh::TerminalSshAuth,
    _accept_unknown_host: bool,
    _cwd: Option<String>,
    _et_client_path: String,
    _tmux_session: Option<String>,
    _size: super::session::TerminalSize,
    _trust_store: Option<std::sync::Arc<super::ssh_known_hosts::TerminalSshTrustStore>>,
) -> Result<super::backend::OpenBackendResult, super::session::TerminalError> {
    Err(super::session::TerminalError::Unsupported {
        detail: "ET terminal backend is only available on Android".to_string(),
    })
}
