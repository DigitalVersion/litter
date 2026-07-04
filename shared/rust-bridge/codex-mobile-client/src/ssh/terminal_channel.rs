//! Open a PTY-backed interactive terminal command over an established
//! [`SshClient`]. Returned to the caller as a raw `russh::Channel` so the
//! caller (currently `crate::terminal::ssh`) owns the channel lifecycle
//! and drives the `wait` / control loop on its own task.

use russh::Channel;
use russh::client::Msg;

use super::{SshClient, SshError, append_bridge_info_log};


impl SshClient {
    /// Open a session channel, request a PTY of the given grid size with the
    /// default `xterm-256color` terminfo, then exec a terminal command. By
    /// default this attaches/creates a tmux session so the shell survives
    /// mobile disconnects. If `shell` is `Some`, preserve the legacy explicit
    /// shell override behavior (optionally prefixed with `cd <cwd> &&`).
    pub(crate) async fn open_terminal_channel(
        &self,
        cols: u16,
        rows: u16,
        shell: Option<&str>,
        cwd: Option<&str>,
        host: Option<&str>,
    ) -> Result<Channel<Msg>, SshError> {
        let handle = self.handle.lock().await;
        if handle.is_closed() {
            return Err(SshError::Disconnected);
        }
        let channel = handle
            .channel_open_session()
            .await
            .map_err(|error| SshError::ConnectionFailed(format!("open session: {error}")))?;
        drop(handle);

        channel
            .request_pty(
                true,
                "xterm-256color",
                cols as u32,
                rows as u32,
                0,
                0,
                &[],
            )
            .await
            .map_err(|error| SshError::ConnectionFailed(format!("request pty: {error}")))?;

        let command = build_terminal_command(shell, cwd, host);
        channel
            .exec(true, command.as_bytes())
            .await
            .map_err(|error| {
                SshError::ConnectionFailed(format!("exec terminal command: {error}"))
            })?;

        append_bridge_info_log(&format!(
            "ssh_terminal_channel_opened cols={} rows={} shell={}",
            cols,
            rows,
            shell.unwrap_or("tmux:codex-litter")
        ));

        Ok(channel)
    }
}

fn build_terminal_command(shell: Option<&str>, cwd: Option<&str>, host: Option<&str>) -> String {
    if let Some(shell) = shell {
        return match cwd {
            Some(dir) if !dir.is_empty() => format!(
                "cd {} && exec {}",
                super::shell_quote(dir),
                super::shell_quote(shell)
            ),
            _ => format!("exec {}", super::shell_quote(shell)),
        };
    }

    let session_name = tmux_session_name(host);
    let session = super::shell_quote(&session_name);
    let start_dir = match cwd.map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(dir) => super::shell_quote(dir),
        None => "\"$HOME/Central_Command\"".to_string(),
    };
    let tmux = format!("tmux new-session -A -s {session} -c {start_dir}");

    // Prefer tmux so the remote shell survives mobile disconnects. Default to
    // Central_Command when present so Litter lands in the same workspace as Tin.
    // If tmux is absent, fall back to the user's login shell instead of failing.
    format!(
        "if [ ! -d {start_dir} ]; then cd \"$HOME\"; else cd {start_dir}; fi; if command -v tmux >/dev/null 2>&1; then exec {tmux}; else exec \"${{SHELL:-sh}}\"; fi"
    )
}

fn tmux_session_name(host: Option<&str>) -> String {
    let short = host
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .and_then(|host| host.split('.').next())
        .map(|host| {
            host.chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                        ch
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        })
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "remote".to_string());
    format!("codex-litter@{short}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_terminal_uses_tmux() {
        let command = build_terminal_command(None, None, Some("nec.tail7f125e.ts.net"));
        assert!(command.contains("command -v tmux"));
        assert!(command.contains("exec tmux new-session -A -s codex-litter@nec"));
        assert!(command.contains("${SHELL:-sh}"));
    }

    #[test]
    fn default_terminal_passes_cwd_to_tmux() {
        let command = build_terminal_command(None, Some("/home/tin/Central Command"), Some("nec.tail7f125e.ts.net"));
        assert!(command.contains("tmux new-session -A -s codex-litter@nec -c '/home/tin/Central Command'"));
    }

    #[test]
    fn explicit_shell_preserves_legacy_behavior() {
        assert_eq!(
            build_terminal_command(Some("/bin/zsh"), Some("/tmp/work"), Some("nec")),
            "cd /tmp/work && exec /bin/zsh"
        );
    }
}
