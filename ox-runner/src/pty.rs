//! PTY allocation and server-relayed websocket bridge for interactive (tty=true) runtime steps.
//!
//! Uses raw libc openpty/fork for minimal dependencies. Linux-only.
//! PTY I/O is relayed through ox-server via websocket, not exposed directly.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use tokio_tungstenite::tungstenite::Message;

/// A spawned PTY process.
pub struct PtyProcess {
    /// The master side of the PTY (read/write to communicate with the child).
    pub master_fd: OwnedFd,
    /// The child process PID.
    pub child_pid: libc::pid_t,
}

/// Spawn a process attached to a new PTY.
pub fn spawn_pty(
    cmd: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
) -> Result<PtyProcess> {
    let mut master_fd: libc::c_int = 0;
    let mut slave_fd: libc::c_int = 0;

    let ret = unsafe {
        libc::openpty(
            &mut master_fd,
            &mut slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ret != 0 {
        anyhow::bail!("openpty failed: {}", std::io::Error::last_os_error());
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        anyhow::bail!("fork failed: {}", std::io::Error::last_os_error());
    }

    if pid == 0 {
        // ── Child process ──────────────────────────────────────────
        unsafe {
            libc::close(master_fd);
            libc::setsid();
            libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);

            libc::dup2(slave_fd, 0);
            libc::dup2(slave_fd, 1);
            libc::dup2(slave_fd, 2);
            if slave_fd > 2 {
                libc::close(slave_fd);
            }

            let cwd_c = CString::new(cwd.to_string_lossy().as_bytes()).unwrap();
            libc::chdir(cwd_c.as_ptr());

            for (k, v) in env {
                let var = CString::new(format!("{k}={v}")).unwrap();
                libc::putenv(var.into_raw());
            }

            let prog = CString::new(cmd[0].as_bytes()).unwrap();
            let c_args: Vec<CString> = cmd
                .iter()
                .map(|a| CString::new(a.as_bytes()).unwrap())
                .collect();
            let c_arg_ptrs: Vec<*const libc::c_char> = c_args
                .iter()
                .map(|a| a.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect();

            libc::execvp(prog.as_ptr(), c_arg_ptrs.as_ptr());
            libc::_exit(127);
        }
    }

    // ── Parent process ─────────────────────────────────────────────
    unsafe { libc::close(slave_fd) };

    Ok(PtyProcess {
        master_fd: unsafe { OwnedFd::from_raw_fd(master_fd) },
        child_pid: pid,
    })
}

/// Handle for a running PTY relay through the server.
pub struct PtyRelay {
    /// Abort to stop the relay.
    pub task: tokio::task::JoinHandle<()>,
}

/// Start a PTY relay that connects to the server via websocket.
///
/// PTY output is teed to `log_path` for the log pusher AND sent as binary
/// websocket frames to the server. Input from the server websocket is
/// written to the PTY master fd.
pub async fn start_pty_relay(
    master_fd: &OwnedFd,
    log_path: &Path,
    server_url: &str,
    execution_id: &str,
    step: &str,
) -> Result<PtyRelay> {
    // Build websocket URL from server HTTP URL
    let ws_url = server_url
        .replace("http://", "ws://")
        .replace("https://", "wss://");
    let url = format!(
        "{}/api/executions/{}/steps/{}/pty/runner",
        ws_url, execution_id, step
    );

    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .context(format!("failed to connect PTY relay websocket to {url}"))?;

    tracing::info!(url = %url, "PTY relay: connected to server");

    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // Dup the master fd for the relay task
    let raw = master_fd.as_raw_fd();
    let pty_read_fd = unsafe { libc::dup(raw) };
    let pty_write_fd = unsafe { libc::dup(raw) };
    if pty_read_fd < 0 || pty_write_fd < 0 {
        anyhow::bail!("dup failed: {}", std::io::Error::last_os_error());
    }

    let log_path = log_path.to_path_buf();

    let task = tokio::spawn(async move {
        // Channel for PTY output — blocking reader publishes here
        let (pty_tx, mut pty_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        // Blocking reader thread: reads from PTY master, sends to channel
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let n = unsafe {
                    libc::read(pty_read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n <= 0 {
                    break;
                }
                if pty_tx.blocking_send(buf[..n as usize].to_vec()).is_err() {
                    break;
                }
            }
            unsafe { libc::close(pty_read_fd) };
        });

        // Open log file for tee
        let mut log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();

        // Server → PTY: read ws frames, write to PTY master fd
        let ws_to_pty = tokio::spawn(async move {
            while let Some(Ok(msg)) = ws_rx.next().await {
                match msg {
                    Message::Binary(data) => {
                        let fd = pty_write_fd;
                        let ok = tokio::task::spawn_blocking(move || {
                            let written = unsafe {
                                libc::write(
                                    fd,
                                    data.as_ptr() as *const libc::c_void,
                                    data.len(),
                                )
                            };
                            written > 0
                        })
                        .await
                        .unwrap_or(false);
                        if !ok {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            unsafe { libc::close(pty_write_fd) };
        });

        // PTY → log + server: read from channel, tee to log file and ws
        let pty_to_server = tokio::spawn(async move {
            while let Some(data) = pty_rx.recv().await {
                // Tee to log file
                if let Some(ref mut f) = log_file {
                    use std::io::Write;
                    let _ = f.write_all(&data);
                }
                // Send to server as binary frame
                if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                    break;
                }
            }
        });

        tokio::select! {
            _ = ws_to_pty => {}
            _ = pty_to_server => {}
        }
    });

    Ok(PtyRelay { task })
}
