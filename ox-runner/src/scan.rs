//! Declarative log-pattern signal detection.
//!
//! After a step's process exits, the runner scans the tail of its log
//! file for regexes declared in the runtime config. Each match emits
//! a named signal that the workflow engine can act on (e.g. bypass
//! retries on `auth_failed`).
//!
//! The scan is bounded by `tail_bytes` per signal — the runner never
//! walks unbounded log content, and the `regex` crate guarantees
//! linear-time matching, so a malicious or pathological log can't
//! stall a runner.

use ox_core::events::SignalMatch;
use ox_core::runtime::CompiledFailureSignal;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Scan the tail of a log file for the given signals' patterns.
///
/// For each signal whose regex matches anywhere in the tail, returns
/// one `SignalMatch` carrying the signal name and the matched line.
/// If a signal matches multiple lines, only the first match is reported
/// (one entry per declared signal name, not per match).
///
/// Errors (missing file, unreadable file) are logged and treated as
/// "no matches" — the runner should not fail a step because the log
/// scan couldn't run.
pub fn scan_failure_signals(
    log_path: &Path,
    signals: &[CompiledFailureSignal],
) -> Vec<SignalMatch> {
    if signals.is_empty() {
        return vec![];
    }

    // Read once, sized to the largest tail any signal asks for. Each
    // signal then scans its own (possibly shorter) suffix of that read.
    let max_tail = signals.iter().map(|s| s.tail_bytes).max().unwrap_or(0);
    let bytes = match read_tail(log_path, max_tail) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                path = %log_path.display(),
                error = %e,
                "failure-signal scan: log unreadable, skipping"
            );
            return vec![];
        }
    };

    let mut out = Vec::new();
    for sig in signals {
        let suffix = tail_suffix(&bytes, sig.tail_bytes);
        let text = drop_partial_utf8_prefix(suffix);
        if let Some(m) = sig.regex.find(text) {
            let line = enclosing_line(text, m.start(), m.end()).to_string();
            out.push(SignalMatch {
                name: sig.name.clone(),
                line,
            });
        }
    }
    out
}

/// Read at most `max_bytes` from the end of `path`. Files smaller than
/// `max_bytes` are returned in full.
fn read_tail(path: &Path, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let to_read = std::cmp::min(len, max_bytes as u64);
    let start = len - to_read;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity(to_read as usize);
    file.take(to_read).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Last `n` bytes of `bytes` (or all of it if shorter).
fn tail_suffix(bytes: &[u8], n: usize) -> &[u8] {
    let start = bytes.len().saturating_sub(n);
    &bytes[start..]
}

/// Drop any leading bytes that aren't a valid UTF-8 boundary so the
/// remainder is safe to treat as `&str`. Invalid bytes elsewhere are
/// replaced lazily via `String::from_utf8_lossy` is overkill — patterns
/// only need to find ASCII-ish anchors, so a clean prefix is enough.
fn drop_partial_utf8_prefix(bytes: &[u8]) -> &str {
    // Find the first index that begins a valid UTF-8 sequence by
    // scanning forward up to 4 bytes (max UTF-8 char width) and trying
    // to parse from there. If nothing parses cleanly, return "".
    for skip in 0..bytes.len().min(4) {
        if let Ok(s) = std::str::from_utf8(&bytes[skip..]) {
            return s;
        }
    }
    // Last resort: only the empty suffix is guaranteed valid UTF-8.
    ""
}

/// Return the substring of `text` that contains the byte range
/// `[match_start, match_end)` extended out to enclosing newline
/// boundaries.
fn enclosing_line(text: &str, match_start: usize, match_end: usize) -> &str {
    let line_start = text[..match_start]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let line_end = text[match_end..]
        .find('\n')
        .map(|i| match_end + i)
        .unwrap_or(text.len());
    &text[line_start..line_end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ox_core::runtime::{CompiledFailureSignal, RuntimeDef, RuntimeFailureSignal};
    use std::io::Write;

    fn compile(name: &str, pattern: &str, tail_bytes: usize) -> CompiledFailureSignal {
        let rt = RuntimeDef {
            name: "fixture".into(),
            vars: Default::default(),
            command: ox_core::runtime::CommandDef {
                cmd: vec!["true".into()],
                interactive_cmd: None,
                optional: vec![],
            },
            files: vec![],
            env: Default::default(),
            proxy: vec![],
            metrics: vec![],
            failure_signals: vec![RuntimeFailureSignal {
                name: name.into(),
                pattern: pattern.into(),
                retriable: false,
                tail_bytes,
            }],
        };
        rt.compile_failure_signals().unwrap().pop().unwrap()
    }

    fn tmp_log(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn match_found_in_tail() {
        let log = tmp_log(b"info: starting\nerror: authentication_error happened\ninfo: bye\n");
        let sig = compile("auth_failed", "authentication_error", 65_536);
        let matches = scan_failure_signals(log.path(), &[sig]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "auth_failed");
        assert!(
            matches[0].line.contains("authentication_error"),
            "matched line should contain the pattern, got: {:?}",
            matches[0].line
        );
    }

    #[test]
    fn no_match_returns_empty() {
        let log = tmp_log(b"all clear\nnothing to see\n");
        let sig = compile("auth_failed", "authentication_error", 65_536);
        let matches = scan_failure_signals(log.path(), &[sig]);
        assert!(matches.is_empty());
    }

    #[test]
    fn partial_utf8_at_tail_boundary_does_not_panic() {
        // A 4-byte UTF-8 char (🦀 = F0 9F A6 80). We craft a tail that
        // begins mid-multibyte: the 3 trailing bytes of 🦀 followed by
        // an ASCII line containing the pattern. With tail_bytes = 16
        // the seek lands inside the 🦀, so the scanner must drop the
        // partial bytes before treating the slice as UTF-8.
        let mut bytes = vec![b'A'; 32];
        bytes.extend_from_slice("🦀rust".as_bytes());
        bytes.extend_from_slice(b"\nauth: authentication_error\n");
        let log = tmp_log(&bytes);
        let sig = compile("auth_failed", "authentication_error", 16);
        // The scan must not panic. The pattern lies past the 16-byte
        // tail boundary, so it might not match — the only assertion is
        // that we get a clean Vec back.
        let _ = scan_failure_signals(log.path(), &[sig]);
    }

    #[test]
    fn missing_log_file_returns_empty() {
        let path = Path::new("/tmp/ox-scan-does-not-exist-9d3f.log");
        let sig = compile("auth_failed", "authentication_error", 65_536);
        let matches = scan_failure_signals(path, &[sig]);
        assert!(matches.is_empty());
    }

    #[test]
    fn empty_log_file_returns_empty() {
        let log = tmp_log(b"");
        let sig = compile("auth_failed", "authentication_error", 65_536);
        let matches = scan_failure_signals(log.path(), &[sig]);
        assert!(matches.is_empty());
    }

    #[test]
    fn file_smaller_than_tail_bytes_scans_whole_file() {
        let log = tmp_log(b"oops authentication_error\n");
        // tail_bytes way larger than file — must still match.
        let sig = compile("auth_failed", "authentication_error", 1_048_576);
        let matches = scan_failure_signals(log.path(), &[sig]);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn file_larger_than_tail_bytes_misses_early_only_match() {
        // 200 KiB of filler followed by an unrelated tail. The pattern
        // appears only in the early region, well before the 64 KiB
        // tail window — proves the bound is honored, not bypassed.
        let mut bytes = Vec::with_capacity(220_000);
        bytes.extend_from_slice(b"start: authentication_error here\n");
        bytes.extend(std::iter::repeat_n(b'.', 200_000));
        bytes.extend_from_slice(b"\nclean tail with no signal\n");
        let log = tmp_log(&bytes);
        let sig = compile("auth_failed", "authentication_error", 65_536);
        let matches = scan_failure_signals(log.path(), &[sig]);
        assert!(
            matches.is_empty(),
            "early-only match must be missed once it falls outside the tail window"
        );
    }

    #[test]
    fn multiple_signals_each_match_independently() {
        let log = tmp_log(
            b"info: launching\nerror: authentication_error\nwarn: rate_limit_exceeded\nbye\n",
        );
        let auth = compile("auth_failed", "authentication_error", 65_536);
        let rate = compile("rate_limited", "rate_limit_exceeded", 65_536);
        let matches = scan_failure_signals(log.path(), &[auth, rate]);
        let names: Vec<_> = matches.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"auth_failed"));
        assert!(names.contains(&"rate_limited"));
        assert_eq!(matches.len(), 2);
    }
}
