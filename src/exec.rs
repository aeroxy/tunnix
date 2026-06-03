use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use tracing::debug;

use crate::protocol::Message;
use crate::relay::next_conn_id;
use crate::tunnel::{Tunnel, TunnelEvent};

/// Current terminal size as (cols, rows), defaulting to 80x24 if unavailable.
fn terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // stdin may be redirected (piped input) while stdout/stderr is still a TTY,
    // so fall back across all three before defaulting.
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
        if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (80, 24)
}

/// Puts the terminal into raw mode and restores the original settings on drop
/// (return, error, or panic).
struct RawGuard {
    original: nix::sys::termios::Termios,
}

impl RawGuard {
    fn enter() -> Result<Self> {
        use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
        use std::os::fd::AsFd;
        let stdin = std::io::stdin();
        let fd = stdin.as_fd();
        let original = tcgetattr(fd)?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(fd, SetArg::TCSANOW, &raw)?;
        Ok(Self { original })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        use nix::sys::termios::{tcsetattr, SetArg};
        use std::os::fd::AsFd;
        let _ = tcsetattr(std::io::stdin().as_fd(), SetArg::TCSANOW, &self.original);
    }
}

/// Run a remote command (or interactive shell when `cmd` is None) over the tunnel,
/// wiring the local terminal to a PTY on the server. Returns the child's exit code.
pub async fn run(tunnel: Arc<Tunnel>, cmd: Option<String>) -> Result<i32> {
    let conn_id = next_conn_id();
    let mut event_rx = tunnel.register_connection(conn_id).await;

    // Run the session in an inner block so the connection is always unregistered,
    // even on an early error return.
    let result = session(&tunnel, conn_id, &mut event_rx, cmd).await;
    tunnel.unregister_connection(conn_id).await;
    result
}

async fn session(
    tunnel: &Arc<Tunnel>,
    conn_id: u32,
    event_rx: &mut tokio::sync::mpsc::Receiver<TunnelEvent>,
    cmd: Option<String>,
) -> Result<i32> {
    let (cols, rows) = terminal_size();

    // Run the session body in an inner async block so we can guarantee a
    // `Message::Close` is sent to the server on every exit path (including
    // `?` early returns). Without this, a tunnel error during the session
    // would leave the server's PTY and child process orphaned.
    let result = async {
        // Ask the server to open the PTY; the ACK is empty Data on success, or an
        // Error (e.g. exec disabled) which we surface before touching the terminal.
        let exec_msg = Message::Exec { conn_id, cmd, cols, rows };
        if let Some(Message::Error { message, .. }) = tunnel.send_connect(&exec_msg).await? {
            anyhow::bail!("{}", message);
        }

        // PTY is live — switch the local terminal to raw mode so keystrokes and
        // control sequences pass through untouched. Best-effort: when stdin is not a
        // TTY (piped / redirected), we just stream without raw mode.
        let _guard = RawGuard::enter().ok();

        // Dedicated background sender: all outbound messages (stdin data, EOF,
        // resize) are queued into the mpsc and shipped by this task. Keeps the
        // select! arms non-blocking so a slow POST /send can't stall stdout
        // writes or SIGWINCH handling.
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let sender_tunnel = tunnel.clone();
        let sender_task = tokio::spawn(async move {
            while let Some(msg) = msg_rx.recv().await {
                if let Err(e) = sender_tunnel.send_message(&msg).await {
                    debug!("send_message failed: {}", e);
                    break;
                }
            }
        });

        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut stdin_buf = vec![0u8; 8192];
        let mut stdin_open = true;
        let mut winch = signal(SignalKind::window_change())?;
        let mut exit_code = 0;
        // Only a real ExitStatus from the server makes the command's exit code
        // meaningful. If the tunnel drops or errors first, we must not report 0.
        let mut saw_exit = false;
        // Track whether the last forwarded byte was '\n'. On local stdin EOF,
        // canonical-mode consumers (cat, read, ...) need a real EOF, not just a
        // VEOF flush of an unterminated pending line. If the line is still
        // buffered (no trailing newline), one Ctrl-D only delivers the partial
        // line; the consumer then waits for more input forever. We send
        // '\n' + 0x04 in that case so the line is delivered AND the next read
        // sees EOF.
        let mut last_was_newline = true;

        loop {
            tokio::select! {
                n = stdin.read(&mut stdin_buf), if stdin_open => {
                    match n {
                        Ok(0) => {
                            // Local stdin hit EOF. Send a real EOF to the
                            // remote PTY so canonical-mode consumers terminate.
                            let mut eof_data = Vec::with_capacity(2);
                            if !last_was_newline {
                                // Terminate the pending line so the VEOF below
                                // can actually signal EOF on an empty buffer.
                                eof_data.push(b'\n');
                            }
                            eof_data.push(0x04);
                            let _ = msg_tx.send(Message::Data { conn_id, data: eof_data });
                            stdin_open = false;
                        }
                        Ok(n) => {
                            last_was_newline = stdin_buf[n - 1] == b'\n';
                            let msg = Message::Data { conn_id, data: stdin_buf[..n].to_vec() };
                            if msg_tx.send(msg).is_err() {
                                anyhow::bail!("sender channel closed");
                            }
                        }
                        Err(e) => {
                            debug!("stdin read error: {}", e);
                            stdin_open = false;
                        }
                    }
                }
                event = event_rx.recv() => {
                    match event {
                        Some(TunnelEvent::Data(data)) => {
                            if stdout.write_all(&data).await.is_err() {
                                break;
                            }
                            let _ = stdout.flush().await;
                        }
                        Some(TunnelEvent::Exit(code)) => {
                            exit_code = code;
                            saw_exit = true;
                        }
                        Some(TunnelEvent::Error(msg)) => {
                            anyhow::bail!("remote error: {}", msg);
                        }
                        Some(TunnelEvent::Close) | None => break,
                    }
                }
                _ = winch.recv() => {
                    let (cols, rows) = terminal_size();
                    let _ = msg_tx.send(Message::Resize { conn_id, cols, rows });
                }
            }
        }

        // Close the sender channel and wait for it to flush all queued
        // messages. This guarantees the final Message::Close below is the
        // last thing sent on the wire for this conn_id.
        drop(msg_tx);
        let _ = sender_task.await;

        if saw_exit {
            Ok(exit_code)
        } else {
            anyhow::bail!("connection closed before the remote command reported an exit status");
        }
    }
    .await;

    // Always tell the server to tear down the PTY, even on early bail — the
    // server-side relay will then kill the child via the closed writer channel.
    let _ = tunnel.send_message(&Message::Close { conn_id }).await;
    // _guard drops here on the success path, restoring the terminal.
    result
}
