//! Dependency-light JSONL recorder for `--log`: appends one JSON line per
//! ping sample plus outage start/end events, so an overnight run leaves a
//! durable, script-friendly record. Hand-serialized rather than pulling in
//! serde — see Cargo.toml's four-dependency budget — with explicit string
//! escaping for host names since they're the only field that isn't already
//! a well-formed number.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Appends newline-delimited JSON records to a log file. Flushes after
/// every write (not just on drop) so a killed or crashed process still
/// leaves a durable, readable log up through the last completed sample —
/// the whole point of a recorder meant for unattended overnight runs.
pub struct Recorder {
    file: BufWriter<File>,
}

impl Recorder {
    /// Opens `path` in append mode, creating it if missing. Append (rather
    /// than truncate) means re-running ekg with the same `--log` path
    /// layers new history onto old instead of clobbering it.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: BufWriter::new(file),
        })
    }

    /// Logs one ping sample. `rtt_ms` is `None` on timeout, so JSONL
    /// consumers can distinguish "no reply" from "reply was 0ms".
    pub fn log_sample(&mut self, host: &str, rtt_ms: Option<f64>) -> io::Result<()> {
        let rtt_field = match rtt_ms {
            Some(ms) => ms.to_string(),
            None => "null".to_string(),
        };
        self.write_line(&format!(
            "{{\"ts\":{},\"host\":\"{}\",\"rtt_ms\":{rtt_field}}}",
            now_ms(),
            escape_json(host),
        ))
    }

    /// Logs an outage declaration, i.e. the moment consecutive timeouts
    /// crossed [`crate::display::OUTAGE_THRESHOLD`]. `started_wall` is the
    /// wall-clock time of that moment (not "now"), matching the timestamp
    /// used elsewhere for the same outage.
    pub fn log_outage_start(&mut self, host: &str, started_wall: SystemTime) -> io::Result<()> {
        self.write_line(&format!(
            "{{\"ts\":{},\"host\":\"{}\",\"event\":\"outage_start\"}}",
            to_ms(started_wall),
            escape_json(host),
        ))
    }

    /// Logs an outage's recovery, including how long it lasted. `ended_wall`
    /// should be `started_wall + duration` (as the display layer computes
    /// it) rather than a fresh `SystemTime::now()`, so the JSONL record and
    /// the printed outage line always agree on end time.
    pub fn log_outage_end(
        &mut self,
        host: &str,
        ended_wall: SystemTime,
        duration: Duration,
    ) -> io::Result<()> {
        self.write_line(&format!(
            "{{\"ts\":{},\"host\":\"{}\",\"event\":\"outage_end\",\"duration_ms\":{}}}",
            to_ms(ended_wall),
            escape_json(host),
            duration.as_millis(),
        ))
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
}

fn now_ms() -> u128 {
    to_ms(SystemTime::now())
}

fn to_ms(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// Minimal JSON string escaping. Hosts are DNS names or IP literals in
/// practice, but escaped properly (control chars, quotes, backslashes)
/// rather than assumed safe — a hostname is still attacker/DNS-controlled
/// input as far as this log file is concerned.
fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn escape_json_handles_quotes_and_backslashes() {
        assert_eq!(escape_json(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn escape_json_handles_control_chars() {
        assert_eq!(escape_json("a\nb\tc"), "a\\nb\\tc");
        assert_eq!(escape_json("\u{01}"), "\\u0001");
    }

    #[test]
    fn escape_json_passthrough_plain_host() {
        assert_eq!(escape_json("1.1.1.1"), "1.1.1.1");
        assert_eq!(escape_json("my-router.local"), "my-router.local");
    }

    #[test]
    fn log_sample_writes_null_rtt_on_timeout() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-recorder-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut rec = Recorder::open(&path).unwrap();
        rec.log_sample("1.1.1.1", None).unwrap();
        rec.log_sample("1.1.1.1", Some(12.5)).unwrap();
        drop(rec);

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"rtt_ms\":null"));
        assert!(lines[1].contains("\"rtt_ms\":12.5"));
        assert!(lines[0].contains("\"host\":\"1.1.1.1\""));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_appends_rather_than_truncates() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-recorder-append-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let mut rec = Recorder::open(&path).unwrap();
            rec.log_sample("a", Some(1.0)).unwrap();
        }
        {
            let mut rec = Recorder::open(&path).unwrap();
            rec.log_sample("b", Some(2.0)).unwrap();
        }

        let file = File::open(&path).unwrap();
        let line_count = BufReader::new(file).lines().count();
        assert_eq!(line_count, 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn log_outage_end_includes_duration_ms() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ekg-recorder-outage-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut rec = Recorder::open(&path).unwrap();
        let start = SystemTime::now();
        rec.log_outage_start("host", start).unwrap();
        rec.log_outage_end(
            "host",
            start + Duration::from_secs(36),
            Duration::from_secs(36),
        )
        .unwrap();
        drop(rec);

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"event\":\"outage_start\""));
        assert!(lines[1].contains("\"event\":\"outage_end\""));
        assert!(lines[1].contains("\"duration_ms\":36000"));

        let _ = std::fs::remove_file(&path);
    }
}
