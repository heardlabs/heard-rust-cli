//! Reading `history.jsonl` — what Heard said — for `heard history` and the
//! console's live feed.
//!
//! One JSON object per line, appended by the speech queue
//! (heard-speech `append_history`): `ts` (UTC, `YYYY-MM-DDTHH:MM:SSZ`),
//! `kind`, `tag`, `via`, `repo_name`, `id`, `session_id`, `spoken`, `voice`,
//! `persona`. Feedback records (`"type":"feedback"`) share the file and are
//! skipped here.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// A spoken record.
pub type Record = Map<String, Value>;

/// Parse one line; `None` for blanks, junk and feedback records.
pub fn parse_line(line: &str) -> Option<Record> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let m = v.as_object()?.clone();
    if m.get("type").and_then(Value::as_str) == Some("feedback") {
        return None;
    }
    m.get("spoken").and_then(Value::as_str)?;
    Some(m)
}

/// Every spoken record in the file, oldest first.
pub fn read_all(path: &Path) -> Vec<Record> {
    std::fs::read_to_string(path)
        .map(|t| t.lines().filter_map(parse_line).collect())
        .unwrap_or_default()
}

/// `5m`, `2h`, `1d`, `30s` → seconds.
pub fn parse_duration(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_lowercase();
    let (num, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    let mul = match unit.trim() {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        "d" | "day" | "days" => 86_400,
        _ => return None,
    };
    Some(n * mul)
}

/// `YYYY-MM-DDTHH:MM:SSZ` → Unix seconds.
pub fn parse_ts(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // Howard Hinnant's days_from_civil.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

/// Now, Unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The local UTC offset in seconds, asked of `date` once. 0 if that fails.
pub fn utc_offset() -> i64 {
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        let out = std::process::Command::new("date").arg("+%z").output();
        let Ok(out) = out else { return 0 };
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.len() != 5 {
            return 0;
        }
        let sign = if s.starts_with('-') { -1 } else { 1 };
        let h: i64 = s[1..3].parse().unwrap_or(0);
        let m: i64 = s[3..5].parse().unwrap_or(0);
        sign * (h * 3600 + m * 60)
    })
}

/// `HH:MM:SS` in local time; with `date`, `YYYY-MM-DD HH:MM:SS`.
pub fn local_time(ts: &str, with_date: bool) -> String {
    let Some(t) = parse_ts(ts) else {
        return ts.to_string();
    };
    let t = t + utc_offset();
    let days = t.div_euclid(86_400);
    let sod = t.rem_euclid(86_400);
    let hms = format!("{:02}:{:02}:{:02}", sod / 3600, (sod % 3600) / 60, sod % 60);
    if !with_date {
        return hms;
    }
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {hms}")
}

/// Field as str.
pub fn field<'a>(r: &'a Record, k: &str) -> &'a str {
    r.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Who said it: repo name, else the short session id.
pub fn source_label(r: &Record) -> String {
    let repo = field(r, "repo_name");
    if !repo.is_empty() {
        return repo.to_string();
    }
    let sid: String = field(r, "session_id").chars().take(8).collect();
    if sid.is_empty() {
        "heard".into()
    } else {
        sid
    }
}

/// One display line: `14:02:11  repo           text`.
pub fn format_line(r: &Record, with_date: bool) -> String {
    format!(
        "{}  {:<14} {}",
        local_time(field(r, "ts"), with_date),
        source_label(r),
        field(r, "spoken").trim()
    )
}

/// Records newer than `since_secs` ago, the last `limit` of them.
pub fn filter(records: Vec<Record>, since_secs: Option<u64>, limit: usize) -> Vec<Record> {
    let cutoff = since_secs.map(|s| now() - s as i64);
    let mut out: Vec<Record> = records
        .into_iter()
        .filter(|r| match cutoff {
            None => true,
            Some(c) => parse_ts(field(r, "ts")).is_some_and(|t| t >= c),
        })
        .collect();
    if limit > 0 && out.len() > limit {
        out = out.split_off(out.len() - limit);
    }
    out
}

/// Follows the file from its current end, yielding complete new lines.
/// Survives the file appearing later, truncation and rotation.
#[derive(Debug)]
pub struct Tail {
    path: PathBuf,
    offset: u64,
    partial: String,
}

impl Tail {
    /// Start at the end of `path` (or 0 if it does not exist yet).
    pub fn from_end(path: PathBuf) -> Self {
        let offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Tail {
            path,
            offset,
            partial: String::new(),
        }
    }

    /// New spoken records since the last poll.
    pub fn poll(&mut self) -> Vec<Record> {
        let Ok(mut f) = File::open(&self.path) else {
            return Vec::new();
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Truncated or rotated: start over.
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset || f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        self.offset += buf.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&buf));
        let mut out = Vec::new();
        while let Some(i) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=i).collect();
            if let Some(r) = parse_line(&line) {
                out.push(r);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("5m"), Some(300));
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("1d"), Some(86_400));
        assert_eq!(parse_duration("10 s"), Some(10));
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("5y"), None);
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2026-09-23T14:02:11Z"), Some(1_790_172_131));
        assert_eq!(parse_ts("junk"), None);
    }

    #[test]
    fn feedback_and_junk_skipped() {
        assert!(parse_line(r#"{"type":"feedback","ref":"x"}"#).is_none());
        assert!(parse_line("not json").is_none());
        assert!(parse_line(r#"{"spoken":"hi"}"#).is_some());
    }

    #[test]
    fn tail_follows_appends_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("history.jsonl");
        let mut t = Tail::from_end(p.clone());
        assert!(t.poll().is_empty());
        std::fs::write(&p, "{\"spoken\":\"one\"}\n{\"spoken\":\"tw").unwrap();
        let got = t.poll();
        assert_eq!(got.len(), 1);
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"o\"}\n").unwrap();
        assert_eq!(field(&t.poll()[0], "spoken"), "two");
        std::fs::write(&p, "{\"spoken\":\"three\"}\n").unwrap();
        assert_eq!(field(&t.poll()[0], "spoken"), "three");
    }
}
