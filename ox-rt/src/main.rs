use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Parser, Subcommand};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

#[derive(Parser, Debug)]
#[command(name = "ox-rt", about = "runtime interface helper")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Complete the step with an output value.
    Done {
        /// Skip git preflight checks.
        #[arg(long)]
        force: bool,
        /// Output value (e.g. "pass", "fail:lint").
        output: Vec<String>,
    },
    /// Report a metric.
    Metric { name: String, value: String },
    /// Write artifact content. With no args, reads from stdin.
    Artifact {
        name: String,
        content: Vec<String>,
    },
    /// Close an artifact stream.
    ArtifactDone { name: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let socket = std::env::var("OX_SOCKET").context("OX_SOCKET not set")?;
    run(cli, Path::new(&socket))
}

fn run(cli: Cli, socket: &Path) -> Result<()> {
    match cli.cmd {
        Command::Metric { name, value } => send(socket, &format!("metric {name} {value}")),
        Command::Done { force: _, output } => {
            // Preflight lives in slice 1c. For now all done calls go through.
            let msg = if output.is_empty() {
                "done".to_string()
            } else {
                format!("done {}", output.join(" "))
            };
            send(socket, &msg)
        }
        Command::Artifact { name, content } => {
            let bytes = if content.is_empty() {
                let mut buf = Vec::new();
                std::io::stdin().read_to_end(&mut buf)?;
                buf
            } else {
                content.join(" ").into_bytes()
            };
            let encoded = BASE64.encode(&bytes);
            send(socket, &format!("artifact {name} {encoded}"))
        }
        Command::ArtifactDone { name } => send(socket, &format!("artifact-done {name}")),
    }
}

fn send(socket: &Path, msg: &str) -> Result<()> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connect to {}", socket.display()))?;
    stream.write_all(msg.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    let resp = resp.trim();
    if let Some(err) = resp.strip_prefix("error:") {
        bail!("ox-runner rejected command:{err}");
    }
    if resp != "ok" {
        return Err(anyhow!("unexpected response from ox-runner: {resp}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::thread;

    /// Start a one-shot listener on a temp socket. Returns the path and a receiver
    /// that will produce the first line written by the client.
    fn start_listener() -> (std::path::PathBuf, mpsc::Receiver<String>) {
        let dir = std::env::temp_dir().join(format!("ox-rt-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!(
            "sock-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    let _ = tx.send(line);
                }
                let mut writer = stream;
                let _ = writer.write_all(b"ok\n");
            }
        });
        (path, rx)
    }

    /// Listener that replies with a caller-chosen response.
    fn start_listener_with_response(
        response: &'static [u8],
    ) -> (std::path::PathBuf, mpsc::Receiver<String>) {
        let dir = std::env::temp_dir().join(format!("ox-rt-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!(
            "sock-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    let _ = tx.send(line);
                }
                let mut writer = stream;
                let _ = writer.write_all(response);
            }
        });
        (path, rx)
    }

    #[test]
    fn metric_sends_newline_terminated_message() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from(["ox-rt", "metric", "input_tokens", "14523"]);
        run(cli, &path).expect("run");
        assert_eq!(rx.recv().unwrap(), "metric input_tokens 14523\n");
    }

    #[test]
    fn done_with_force_sends_done_with_output() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from(["ox-rt", "done", "--force", "pass:7"]);
        run(cli, &path).expect("run");
        assert_eq!(rx.recv().unwrap(), "done pass:7\n");
    }

    #[test]
    fn done_with_force_no_output_sends_bare_done() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from(["ox-rt", "done", "--force"]);
        run(cli, &path).expect("run");
        assert_eq!(rx.recv().unwrap(), "done\n");
    }

    #[test]
    fn artifact_with_inline_content_is_base64_encoded() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from(["ox-rt", "artifact", "proposal", "Hello"]);
        run(cli, &path).expect("run");
        // "Hello" -> "SGVsbG8="
        assert_eq!(rx.recv().unwrap(), "artifact proposal SGVsbG8=\n");
    }

    #[test]
    fn artifact_done_sends_name() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from(["ox-rt", "artifact-done", "proposal"]);
        run(cli, &path).expect("run");
        assert_eq!(rx.recv().unwrap(), "artifact-done proposal\n");
    }

    #[test]
    fn error_response_surfaces_as_failure() {
        let (path, _rx) = start_listener_with_response(b"error: no such step\n");
        let cli = Cli::parse_from(["ox-rt", "metric", "x", "1"]);
        let err = run(cli, &path).expect_err("should fail");
        assert!(
            err.to_string().contains("no such step"),
            "got: {err}"
        );
    }
}
