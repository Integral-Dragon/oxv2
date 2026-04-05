use anyhow::Result;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

/// Commands received from the runtime via the unix socket.
#[derive(Debug)]
pub enum RuntimeCommand {
    /// Step complete with output value.
    Done { output: String },
    /// Write artifact content (base64-encoded).
    #[allow(dead_code)]
    Artifact { name: String, data: String },
    /// Close a declared artifact.
    ArtifactDone { name: String },
    /// Report a metric.
    Metric { name: String, value: String },
}

/// Start the unix socket server. Returns a receiver for runtime commands.
/// The socket is created at `path` and accepts one connection at a time.
pub fn start_socket_server(
    path: &Path,
) -> Result<(mpsc::Receiver<RuntimeCommand>, tokio::task::JoinHandle<()>)> {
    // Remove stale socket file
    let _ = std::fs::remove_file(path);

    let listener = UnixListener::bind(path)?;
    let (tx, rx) = mpsc::channel(64);

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let (reader, mut writer) = stream.into_split();
                        let mut lines = BufReader::new(reader).lines();

                        while let Ok(Some(line)) = lines.next_line().await {
                            let line = line.trim().to_string();
                            if line.is_empty() {
                                continue;
                            }

                            let cmd = parse_command(&line);
                            match cmd {
                                Some(c) => {
                                    if tx.send(c).await.is_err() {
                                        break;
                                    }
                                    let _ = writer.write_all(b"ok\n").await;
                                }
                                None => {
                                    let _ = writer
                                        .write_all(
                                            format!("error: unknown command: {line}\n").as_bytes(),
                                        )
                                        .await;
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(err = %e, "socket accept error");
                    break;
                }
            }
        }
    });

    Ok((rx, handle))
}

fn parse_command(line: &str) -> Option<RuntimeCommand> {
    if let Some(rest) = line.strip_prefix("done") {
        let output = rest.trim().to_string();
        return Some(RuntimeCommand::Done { output });
    }
    if let Some(rest) = line.strip_prefix("artifact-done ") {
        let name = rest.trim().to_string();
        return Some(RuntimeCommand::ArtifactDone { name });
    }
    if let Some(rest) = line.strip_prefix("artifact ") {
        let mut parts = rest.splitn(2, ' ');
        let name = parts.next()?.trim().to_string();
        let data = parts.next().unwrap_or("").trim().to_string();
        return Some(RuntimeCommand::Artifact { name, data });
    }
    if let Some(rest) = line.strip_prefix("metric ") {
        let mut parts = rest.splitn(2, ' ');
        let name = parts.next()?.trim().to_string();
        let value = parts.next().unwrap_or("").trim().to_string();
        return Some(RuntimeCommand::Metric { name, value });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_done() {
        let cmd = parse_command("done pass:7").unwrap();
        assert!(matches!(cmd, RuntimeCommand::Done { output } if output == "pass:7"));
    }

    #[test]
    fn parse_done_no_output() {
        let cmd = parse_command("done").unwrap();
        assert!(matches!(cmd, RuntimeCommand::Done { output } if output.is_empty()));
    }

    #[test]
    fn parse_metric() {
        let cmd = parse_command("metric input_tokens 14523").unwrap();
        assert!(
            matches!(cmd, RuntimeCommand::Metric { name, value } if name == "input_tokens" && value == "14523")
        );
    }

    #[test]
    fn parse_artifact() {
        let cmd = parse_command("artifact proposal SGVsbG8=").unwrap();
        assert!(
            matches!(cmd, RuntimeCommand::Artifact { name, data } if name == "proposal" && data == "SGVsbG8=")
        );
    }

    #[test]
    fn parse_artifact_done() {
        let cmd = parse_command("artifact-done proposal").unwrap();
        assert!(matches!(cmd, RuntimeCommand::ArtifactDone { name } if name == "proposal"));
    }

    #[test]
    fn parse_unknown() {
        assert!(parse_command("foobar").is_none());
    }
}
