use anyhow::Result;
use std::os::fd::BorrowedFd;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::signal::unix::{signal, SignalKind};
use tracing::debug;

use crate::protocol::Message;
use crate::relay::next_conn_id;
use crate::tunnel::{Tunnel, TunnelEvent};

/// stdin as a BorrowedFd (fd 0).
fn stdin_fd() -> BorrowedFd<'static> {
    // SAFETY: fd 0 is valid for the lifetime of the process.
    unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) }
}

/// Current terminal size as (cols, rows), defaulting to 80x24 if unavailable.
fn terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

/// Puts the terminal into raw mode and restores the original settings on drop
/// (return, error, or panic).
struct RawGuard {
    original: nix::sys::termios::Termios,
}

impl RawGuard {
    fn enter() -> Result<Self> {
        use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
        let fd = stdin_fd();
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
        let _ = tcsetattr(stdin_fd(), SetArg::TCSANOW, &self.original);
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

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stdin_buf = vec![0u8; 8192];
    let mut stdin_open = true;
    let mut winch = signal(SignalKind::window_change())?;
    let mut exit_code = 0;
    // Only a real ExitStatus from the server makes the command's exit code
    // meaningful. If the tunnel drops or errors first, we must not report 0.
    let mut saw_exit = false;
    // Whether we've forwarded any stdin bytes to the remote PTY.
    let mut sent_input = false;

    loop {
        tokio::select! {
            n = stdin.read(&mut stdin_buf), if stdin_open => {
                match n {
                    Ok(0) => {
                        // Local stdin hit EOF. If we actually sent input, signal EOF
                        // to the remote PTY with the terminal EOF byte (Ctrl-D/VEOF)
                        // so canonical-mode consumers (cat, read, filters) terminate.
                        // Skip it when nothing was sent (e.g. `cmd </dev/null`) to
                        // avoid a stray ^D echo for commands that ignore stdin.
                        if sent_input {
                            let eof = Message::Data { conn_id, data: vec![0x04] };
                            let _ = tunnel.send_message(&eof).await;
                        }
                        stdin_open = false;
                    }
                    Ok(n) => {
                        sent_input = true;
                        let msg = Message::Data { conn_id, data: stdin_buf[..n].to_vec() };
                        if let Err(e) = tunnel.send_message(&msg).await {
                            anyhow::bail!("stdin send failed: {}", e);
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
                let _ = tunnel
                    .send_message(&Message::Resize { conn_id, cols, rows })
                    .await;
            }
        }
    }

    let _ = tunnel.send_message(&Message::Close { conn_id }).await;
    // _guard drops here, restoring the terminal before we return.

    if saw_exit {
        Ok(exit_code)
    } else {
        anyhow::bail!("connection closed before the remote command reported an exit status");
    }
}
