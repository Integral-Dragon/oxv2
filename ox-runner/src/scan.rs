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
    let _ = (log_path, signals);
    todo!("read tail of log_path, scan each signal regex, return SignalMatch per match")
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
