use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use std::io::{BufRead, BufReader, Write};
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
        Command::Done { .. } => todo!("slice 1b"),
        Command::Artifact { .. } => todo!("slice 1c"),
        Command::ArtifactDone { .. } => todo!("slice 1d"),
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

    #[test]
    fn metric_sends_newline_terminated_message() {
        let (path, rx) = start_listener();
        let cli = Cli::parse_from([
            "ox-rt",
            "metric",
            "input_tokens",
            "14523",
        ]);
        run(cli, &path).expect("run");
        let line = rx.recv().expect("listener received line");
        assert_eq!(line, "metric input_tokens 14523\n");
    }
}
