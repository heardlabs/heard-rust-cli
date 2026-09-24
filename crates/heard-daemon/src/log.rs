//! `daemon.py`'s `_log` — one structured line per event, grepable, no prose.
//!
//! The Python format, verbatim:
//!
//! ```text
//! t=2026-09-22 14:03:11 ev=event_speak kind=tool_pre chars=18 via=fastpath
//! ```
//!
//! * the timestamp is LOCAL time with the date (`time.strftime`), so
//!   cross-day debugging needs no second clock;
//! * a field whose value is `None` or `""` is omitted entirely;
//! * newlines become spaces;
//! * a value over 120 characters is cut to 117 plus `…`;
//! * a value containing a space or an `=` is wrapped in double quotes, with
//!   any `"` inside it turned into `'`.
//!
//! The line is emitted through [`tracing`] at INFO on the `heard` target with
//! the whole rendered line as the message, so a subscriber prints exactly
//! these bytes and nothing is re-formatted into prose. A daemon started with
//! no subscriber installed writes the line to stdout itself, which is what
//! the Python `print(..., flush=True)` does.

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// One `k=v` pair. Values are rendered with [`std::fmt::Display`] and then
/// put through the Python escaping rules above.
pub enum Field<'a> {
    /// A string value. Empty means "omit this field", exactly as Python's
    /// `if v is None or v == "": continue`.
    Str(&'a str),
    /// An owned string, for values built at the call site.
    Owned(String),
    /// An integer. `0` is NOT omitted — Python only skips `None` and `""`,
    /// and `0 == ""` is false.
    Int(i64),
    /// A boolean, rendered Python-style (`True` / `False`).
    Bool(bool),
}

impl Field<'_> {
    fn render(&self) -> Option<String> {
        let raw = match self {
            Field::Str(s) => {
                if s.is_empty() {
                    return None;
                }
                (*s).to_owned()
            }
            Field::Owned(s) => {
                if s.is_empty() {
                    return None;
                }
                s.clone()
            }
            Field::Int(v) => v.to_string(),
            Field::Bool(b) => if *b { "True" } else { "False" }.to_owned(),
        };
        Some(escape(&raw))
    }
}

/// Python's value mangling: newlines flattened, then truncated, then quoted.
fn escape(value: &str) -> String {
    let mut s = value.replace('\n', " ");
    // Python slices by CHARACTER, not by byte.
    if s.chars().count() > 120 {
        let head: String = s.chars().take(117).collect();
        s = format!("{head}…");
    }
    if s.contains(' ') || s.contains('=') {
        s = format!("\"{}\"", s.replace('"', "'"));
    }
    s
}

/// Render one log line without emitting it. Exposed so tests can assert the
/// shape against the Python format directly.
pub fn render(event: &str, fields: &[(&str, Field<'_>)]) -> String {
    let mut line = String::with_capacity(64);
    let _ = write!(line, "t={} ev={}", local_timestamp(), event);
    for (key, value) in fields {
        if let Some(rendered) = value.render() {
            let _ = write!(line, " {key}={rendered}");
        }
    }
    line
}

/// Emit one structured line.
pub fn log(event: &str, fields: &[(&str, Field<'_>)]) {
    let line = render(event, fields);
    if tracing::event_enabled!(target: "heard", tracing::Level::INFO) {
        tracing::info!(target: "heard", "{line}");
    } else {
        // No subscriber is installed (the plain `heard-daemon` binary, and
        // every test). Python prints; so do we, with the same flush.
        println!("{line}");
    }
}

/// `_log("ev", k = v, …)` with the Python call's shape.
///
/// ```ignore
/// dlog!("event_drop", kind = kind, tag = tag, reason = "duplicate_event");
/// ```
#[macro_export]
macro_rules! dlog {
    ($event:expr $(, $key:ident = $value:expr )* $(,)?) => {
        $crate::log::log($event, &[ $( (stringify!($key), $value.into()) ),* ])
    };
}

impl<'a> From<&'a str> for Field<'a> {
    fn from(s: &'a str) -> Self {
        Field::Str(s)
    }
}

impl From<String> for Field<'_> {
    fn from(s: String) -> Self {
        Field::Owned(s)
    }
}

impl From<i64> for Field<'_> {
    fn from(v: i64) -> Self {
        Field::Int(v)
    }
}

impl From<usize> for Field<'_> {
    fn from(v: usize) -> Self {
        Field::Int(v as i64)
    }
}

impl From<u64> for Field<'_> {
    fn from(v: u64) -> Self {
        Field::Int(v as i64)
    }
}

impl From<bool> for Field<'_> {
    fn from(b: bool) -> Self {
        Field::Bool(b)
    }
}

// ---------------------------------------------------------------------------
// local time
// ---------------------------------------------------------------------------

/// Seconds east of UTC for this machine's current local time.
///
/// There is no dependency-free way to read the platform's timezone database
/// from `std` (no `libc`, no `chrono` in this workspace), so this asks the
/// system once, at first use, and caches the answer for the process's life.
/// `date +%z` is POSIX and present on every macOS install. A failure means
/// UTC, which makes log timestamps wrong by the offset and nothing else.
///
/// Public so anything that must agree with the log's idea of "local" (a
/// spoken "today" or "yesterday", say) reads the same offset.
pub fn utc_offset_seconds() -> i64 {
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        let Ok(out) = std::process::Command::new("/bin/date").arg("+%z").output() else {
            return 0;
        };
        parse_offset(String::from_utf8_lossy(&out.stdout).trim())
    })
}

/// `+HHMM` / `-HHMM` → seconds. Anything else is 0.
fn parse_offset(text: &str) -> i64 {
    let bytes = text.as_bytes();
    if bytes.len() < 5 {
        return 0;
    }
    let sign = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return 0,
    };
    let Ok(hours) = text[1..3].parse::<i64>() else {
        return 0;
    };
    let Ok(minutes) = text[3..5].parse::<i64>() else {
        return 0;
    };
    sign * (hours * 3600 + minutes * 60)
}

/// Seconds since the epoch, right now.
pub fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `time.strftime("%Y-%m-%d %H:%M:%S")` in local time.
fn local_timestamp() -> String {
    format_civil(now_epoch() as i64 + utc_offset_seconds())
}

/// Howard Hinnant's civil-from-days, the same arithmetic `localtime` does.
/// `heard-state::history` has the UTC twin of this; it is private there and
/// the two formats differ (`T`/`Z` vs a space), so it is not worth widening
/// that crate's API to share nine lines of integer division.
fn format_civil(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs_of_day = epoch_secs.rem_euclid(86_400);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02} {hour:02}:{minute:02}:{second:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_has_the_python_shape() {
        let line = render(
            "event_speak",
            &[("kind", "tool_pre".into()), ("chars", 18i64.into())],
        );
        assert!(line.starts_with("t="), "{line}");
        assert!(
            line.contains(" ev=event_speak kind=tool_pre chars=18"),
            "{line}"
        );
    }

    #[test]
    fn empty_values_are_omitted_but_zero_is_not() {
        let line = render("x", &[("a", "".into()), ("b", 0i64.into())]);
        assert!(!line.contains("a="), "{line}");
        assert!(line.contains("b=0"), "{line}");
    }

    #[test]
    fn values_with_spaces_are_quoted_and_inner_quotes_become_apostrophes() {
        let line = render("x", &[("v", "a \"b\" c".into())]);
        assert!(line.ends_with("v=\"a 'b' c\""), "{line}");
    }

    #[test]
    fn long_values_are_cut_to_117_plus_an_ellipsis() {
        let line = render("x", &[("v", "z".repeat(200).into())]);
        let value = line.rsplit_once(" v=").expect("value").1;
        assert_eq!(value.chars().count(), 118, "{value}");
        assert!(value.ends_with('…'));
    }

    #[test]
    fn newlines_are_flattened_not_dropped() {
        let line = render("x", &[("v", "a\nb".into())]);
        assert!(line.ends_with("v=\"a b\""), "{line}");
    }

    #[test]
    fn offsets_parse_both_ways_and_fail_closed() {
        assert_eq!(parse_offset("+0530"), 19_800);
        assert_eq!(parse_offset("-0700"), -25_200);
        assert_eq!(parse_offset("nope"), 0);
        assert_eq!(parse_offset(""), 0);
    }

    #[test]
    fn the_civil_calendar_matches_a_known_instant() {
        // 2026-09-22T00:00:00Z
        assert_eq!(format_civil(1_790_380_800), "2026-09-26 00:00:00");
    }
}
