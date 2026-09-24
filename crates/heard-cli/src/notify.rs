//! The `notify` extension: a macOS notification for the lines that need you,
//! and the speech gate that keeps them quiet when `alerts` says so.
//!
//! # What counts as needs-you
//!
//! Derived from what the core daemon actually emits (heard-narrate's
//! templates, `policy::is_critical_template_event` and the Focus alert):
//!
//! | spoken line | [`NeedsYou`] |
//! |---|---|
//! | tag `tool_post_needs_you` (a tool result that says it is blocked on you: approval, sign-in, a permission) | `Permission` |
//! | tag `tool_question` (AskUserQuestion), or a Focus alert (`via = focus_alert`), whose text asks for permission (`allow`, `approve`, `approval`, `access`, `permission`) | `Permission` |
//! | the same without a permission word | `Question` |
//! | a tag containing `failure` or `failed` (`tool_post_failure`, `tool_post_command_failed`) | `Failure` |
//! | kind `final` whose text ends the turn on a decision question for you (`policy::focus_prompt_text` non-empty: "Should I…?", "Want me to…?"; routine sign-offs like "Anything else?" do not count) | `Waiting` |
//! | anything else (tool lines, prose, plain finals, digests, `heard say`) | not needs-you |
//!
//! # The `alerts` setting
//!
//! `alerts` only changes how NEEDS-YOU lines reach you. Everything else is
//! narrated by mode, whatever `alerts` says.
//!
//! | `alerts` | needs-you line spoken | notification |
//! |---|---|---|
//! | `both` (default) | yes | yes |
//! | `voice` | yes | no |
//! | `notify` | no | yes |
//! | `off` | no | no |
//!
//! The speech half is [`AlertsGate`], a [`Speech`] wrapper in front of the
//! real sink; the notification half is [`NotifyExtension::on_spoken`]. The
//! daemon calls `on_spoken` for every line it handed to the sink, including
//! the ones the gate then kept quiet, so `notify` still notifies.
//! `alerts` is read from the daemon's config snapshot on every line, so
//! `heard alerts …` (which writes config and sends `reload`) applies at once.
//!
//! # Posting
//!
//! Fire-and-forget on one worker thread, never on the daemon's lanes. Bursts
//! are coalesced ([`Coalescer`]): at most one notification per
//! [`DEFAULT_GAP`]; what arrives in between is folded into one "N alerts"
//! notification when the gap ends, and an identical line within
//! [`DEFAULT_DEDUP`] is dropped. [`OsascriptNotifier`] runs
//! `/usr/bin/osascript` with a CONSTANT script and passes title, subtitle
//! and body as argv to its `on run argv` handler — no user text is ever
//! interpolated into AppleScript source, so quotes, backslashes, newlines and
//! unicode cannot break out of the string.

use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use heard_daemon::policy::{focus_prompt_text, EventView};
use heard_daemon::{Daemon, Extension, Speech, SpokenLine, Utterance};

use crate::settings::Alerts;

/// At most one notification per this long.
pub const DEFAULT_GAP: Duration = Duration::from_secs(5);
/// The same line again within this long is not re-posted.
pub const DEFAULT_DEDUP: Duration = Duration::from_secs(60);
/// The notification title.
pub const TITLE: &str = "Heard";
/// Longest body, in characters.
pub const MAX_BODY: usize = 220;
/// Longest subtitle, in characters.
pub const MAX_SUBTITLE: usize = 80;

// ── classification ──────────────────────────────────────────────────────

/// Why a line needs you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedsYou {
    /// An approval or permission is waiting.
    Permission,
    /// The agent asked you a question.
    Question,
    /// Something failed.
    Failure,
    /// The turn ended on a decision only you can make.
    Waiting,
}

impl NeedsYou {
    /// The body's lead-in.
    pub fn label(self) -> &'static str {
        match self {
            NeedsYou::Permission => "Needs your approval",
            NeedsYou::Question => "Question",
            NeedsYou::Failure => "Failed",
            NeedsYou::Waiting => "Waiting on you",
        }
    }
}

const PERMISSION_WORDS: &[&str] = &["allow", "approve", "approval", "access", "permission"];

/// Classify one spoken line (see the module table).
pub fn classify(line: &SpokenLine<'_>) -> Option<NeedsYou> {
    let tag = line.tag.to_lowercase();
    if tag == "tool_post_needs_you" {
        return Some(NeedsYou::Permission);
    }
    if tag.contains("failure") || tag.contains("failed") {
        return Some(NeedsYou::Failure);
    }
    if tag == "tool_question" || line.via == "focus_alert" {
        let low = line.text.to_lowercase();
        return Some(if PERMISSION_WORDS.iter().any(|w| low.contains(w)) {
            NeedsYou::Permission
        } else {
            NeedsYou::Question
        });
    }
    if line.kind == "final" {
        let view = EventView {
            kind: "final",
            tag: line.tag,
            neutral: line.text,
            session_id: line.session_id,
            abs_path: "",
        };
        if !focus_prompt_text(&view).is_empty() {
            return Some(NeedsYou::Waiting);
        }
    }
    None
}

// ── the daemon link ─────────────────────────────────────────────────────

/// A weak handle on the daemon, attached after it is built (the daemon owns
/// its extensions, so they cannot hold it strongly). Unattached, alerts read
/// as the default ([`Alerts::Both`]) and no agent names are known.
#[derive(Debug, Default)]
pub struct DaemonLink {
    daemon: OnceLock<Weak<Daemon>>,
}

impl DaemonLink {
    /// A fresh, unattached link.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Point at the built daemon. Later calls are ignored.
    pub fn attach(&self, daemon: &Arc<Daemon>) {
        let _ = self.daemon.set(Arc::downgrade(daemon));
    }

    fn get(&self) -> Option<Arc<Daemon>> {
        self.daemon.get().and_then(Weak::upgrade)
    }

    /// `alerts` from the daemon's config snapshot.
    pub fn alerts(&self) -> Alerts {
        Alerts::from_value(self.get().and_then(|d| d.cfg_get("alerts")).as_ref())
    }

    /// The session's host (terminal / editor) when the hook binding named
    /// one with confidence.
    pub fn host(&self, session_id: &str) -> Option<String> {
        let d = self.get()?;
        let id = d.session_identities.get(session_id)?;
        let name = id.display_name();
        (name != heard_state::session_identity::UNKNOWN_LABEL && !name.trim().is_empty())
            .then(|| name.to_string())
    }

    /// The session's repo name from the router.
    pub fn project(&self, session_id: &str) -> Option<String> {
        let d = self.get()?;
        d.router
            .session_info(session_id)
            .map(|i| i.repo_name)
            .filter(|r| !r.trim().is_empty())
    }
}

// ── the speech gate ─────────────────────────────────────────────────────

/// The speech sink in front of the real one: drops needs-you lines when
/// `alerts` is `notify` or `off`, passes everything else through.
pub struct AlertsGate {
    inner: Arc<dyn Speech>,
    link: Arc<DaemonLink>,
}

impl AlertsGate {
    /// Gate `inner` by the linked daemon's `alerts`.
    pub fn new(inner: Arc<dyn Speech>, link: Arc<DaemonLink>) -> Self {
        Self { inner, link }
    }
}

impl Speech for AlertsGate {
    fn speak(&self, u: &Utterance<'_>) {
        if let Some(why) = classify(u) {
            let alerts = self.link.alerts();
            if !alerts.speaks() {
                heard_daemon::dlog!(
                    "speech_skipped",
                    reason = "alerts_quiet",
                    alerts = alerts.key(),
                    kind = u.kind,
                    tag = u.tag,
                    needs_you = why.label()
                );
                return;
            }
        }
        self.inner.speak(u);
    }

    fn speak_with(&self, u: &Utterance<'_>, opts: heard_daemon::LineOptions) {
        if classify(u).is_some() && !self.link.alerts().speaks() {
            return;
        }
        self.inner.speak_with(u, opts);
    }

    fn cancel(&self) {
        self.inner.cancel();
    }

    fn is_speaking(&self) -> bool {
        self.inner.is_speaking()
    }

    fn queue_state(&self) -> (bool, usize) {
        self.inner.queue_state()
    }

    fn hold(&self, hold: heard_daemon::Hold) {
        self.inner.hold(hold);
    }

    fn release(&self, hold: heard_daemon::Hold) {
        self.inner.release(hold);
    }

    fn discard_held(&self) {
        self.inner.discard_held();
    }

    fn clear_mic_latch(&self) {
        self.inner.clear_mic_latch();
    }

    fn mic_active(&self) -> bool {
        self.inner.mic_active()
    }

    fn is_holding(&self) -> bool {
        self.inner.is_holding()
    }

    fn drop_session(&self, session_id: &str) -> usize {
        self.inner.drop_session(session_id)
    }

    fn last_utterance_id(&self) -> Option<String> {
        self.inner.last_utterance_id()
    }
}

// ── notifications ───────────────────────────────────────────────────────

/// One notification, already sanitised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Always [`TITLE`].
    pub title: String,
    /// `<agent · project>`.
    pub subtitle: String,
    /// What needs you.
    pub body: String,
}

/// Something that shows a [`Notification`]. Called on the notify worker
/// thread, never on a daemon lane.
pub trait Notifier: Send + Sync {
    /// Show it. Best effort.
    fn post(&self, n: &Notification);
}

/// The real notifier: `/usr/bin/osascript` with a constant script.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsascriptNotifier;

/// The AppleScript, one `-e` per line. Constant: the text arrives as argv.
pub const SCRIPT: [&str; 3] = [
    "on run argv",
    "display notification (item 3 of argv) with title (item 1 of argv) subtitle (item 2 of argv)",
    "end run",
];

impl OsascriptNotifier {
    /// The program.
    pub const PROGRAM: &'static str = "/usr/bin/osascript";

    /// The argv after the program. The title (never user text, never a
    /// leading `-`) comes first, so `osascript`'s option parsing stops before
    /// any user-supplied argument.
    pub fn argv(n: &Notification) -> Vec<String> {
        let mut v = Vec::with_capacity(9);
        for line in SCRIPT {
            v.push("-e".to_string());
            v.push(line.to_string());
        }
        v.push(n.title.clone());
        v.push(n.subtitle.clone());
        v.push(n.body.clone());
        v
    }
}

impl Notifier for OsascriptNotifier {
    fn post(&self, n: &Notification) {
        let child = Command::new(Self::PROGRAM)
            .args(Self::argv(n))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match child {
            // Reaped here, on the worker, so no zombie outlives it.
            Ok(mut c) => {
                let _ = c.wait();
            }
            Err(e) => heard_daemon::dlog!("notify_failed", err = e.kind().to_string()),
        }
    }
}

/// Printable, one line, at most `max` characters: control characters
/// (newlines, tabs, NUL, bidi overrides) become spaces, whitespace collapses.
pub fn sanitize(text: &str, max: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let head: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

/// One needs-you line on its way to a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Why.
    pub why: NeedsYou,
    /// `<agent · project>`, sanitised.
    pub subtitle: String,
    /// The spoken text, sanitised.
    pub text: String,
}

impl Note {
    /// Build one, sanitising both fields.
    pub fn new(why: NeedsYou, subtitle: &str, text: &str) -> Self {
        Self {
            why,
            subtitle: sanitize(subtitle, MAX_SUBTITLE),
            text: sanitize(text, MAX_BODY),
        }
    }

    fn single(&self) -> Notification {
        Notification {
            title: TITLE.into(),
            subtitle: self.subtitle.clone(),
            body: sanitize(&format!("{}: {}", self.why.label(), self.text), MAX_BODY),
        }
    }
}

/// The rate limit, as a pure state machine over an injected clock.
#[derive(Debug)]
pub struct Coalescer {
    gap: Duration,
    dedup: Duration,
    last_post: Option<Instant>,
    pending: Vec<Note>,
    recent: VecDeque<(Note, Instant)>,
}

impl Coalescer {
    /// A limiter with these windows.
    pub fn new(gap: Duration, dedup: Duration) -> Self {
        Self {
            gap,
            dedup,
            last_post: None,
            pending: Vec::new(),
            recent: VecDeque::new(),
        }
    }

    fn is_repeat(&mut self, note: &Note, now: Instant) -> bool {
        let dedup = self.dedup;
        self.recent
            .retain(|(_, t)| now.saturating_duration_since(*t) < dedup);
        self.recent.iter().any(|(n, _)| n == note) || self.pending.contains(note)
    }

    /// A line arrived: `Some` = post this now; `None` = dropped as a repeat
    /// or held for the end of the gap ([`Coalescer::due`]).
    pub fn offer(&mut self, note: Note, now: Instant) -> Option<Notification> {
        if self.is_repeat(&note, now) {
            return None;
        }
        self.recent.push_back((note.clone(), now));
        let open = self
            .last_post
            .is_none_or(|t| now.saturating_duration_since(t) >= self.gap);
        if open && self.pending.is_empty() {
            self.last_post = Some(now);
            return Some(note.single());
        }
        self.pending.push(note);
        None
    }

    /// When the held lines are due, if any are held.
    pub fn due(&self) -> Option<Instant> {
        if self.pending.is_empty() {
            return None;
        }
        Some(self.last_post.map_or_else(Instant::now, |t| t + self.gap))
    }

    /// Post what is held, folded into one notification, if it is due.
    pub fn flush(&mut self, now: Instant) -> Option<Notification> {
        let due = self.due()?;
        if now < due {
            return None;
        }
        self.last_post = Some(now);
        let held = std::mem::take(&mut self.pending);
        let latest = held.last()?.clone();
        if held.len() == 1 {
            return Some(latest.single());
        }
        let subtitle = if held.iter().all(|n| n.subtitle == latest.subtitle) {
            latest.subtitle.clone()
        } else {
            "several agents".to_string()
        };
        Some(Notification {
            title: TITLE.into(),
            subtitle,
            body: sanitize(
                &format!(
                    "{} alerts. Latest, {}: {}",
                    held.len(),
                    latest.why.label().to_lowercase(),
                    latest.text
                ),
                MAX_BODY,
            ),
        })
    }
}

/// The worker: owns the limiter and the notifier.
fn spawn_worker(
    notifier: Arc<dyn Notifier>,
    gap: Duration,
    dedup: Duration,
) -> Option<Sender<Note>> {
    let (tx, rx) = mpsc::channel::<Note>();
    let spawned = std::thread::Builder::new()
        .name("heard-notify".into())
        .spawn(move || {
            let mut c = Coalescer::new(gap, dedup);
            loop {
                let got = match c.due() {
                    Some(due) => rx.recv_timeout(due.saturating_duration_since(Instant::now())),
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                };
                let now = Instant::now();
                match got {
                    Ok(note) => {
                        if let Some(n) = c.offer(note, now) {
                            notifier.post(&n);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if let Some(n) = c.flush(now) {
                            notifier.post(&n);
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            heard_daemon::dlog!("notify_worker_failed", err = e.to_string());
            None
        }
    }
}

// ── the extension ───────────────────────────────────────────────────────

/// The core's `notify` extension.
pub struct NotifyExtension {
    link: Arc<DaemonLink>,
    tx: Mutex<Option<Sender<Note>>>,
}

impl NotifyExtension {
    /// With the real notifier and the default windows.
    pub fn new(link: Arc<DaemonLink>) -> Self {
        Self::with(
            link,
            Arc::new(OsascriptNotifier),
            DEFAULT_GAP,
            DEFAULT_DEDUP,
        )
    }

    /// With an injected notifier and windows (tests).
    pub fn with(
        link: Arc<DaemonLink>,
        notifier: Arc<dyn Notifier>,
        gap: Duration,
        dedup: Duration,
    ) -> Self {
        Self {
            link,
            tx: Mutex::new(spawn_worker(notifier, gap, dedup)),
        }
    }

    /// `<agent · project>` for a line.
    fn subtitle(&self, line: &SpokenLine<'_>) -> String {
        let project = Some(line.project.trim().to_string())
            .filter(|p| !p.is_empty())
            .or_else(|| self.link.project(line.session_id));
        let host = self.link.host(line.session_id);
        match (host, project) {
            (Some(h), Some(p)) => format!("{h} · {p}"),
            (None, Some(p)) => format!("agent · {p}"),
            (Some(h), None) => h,
            (None, None) => "an agent".to_string(),
        }
    }
}

impl Extension for NotifyExtension {
    fn name(&self) -> &'static str {
        "notify"
    }

    fn on_spoken(&self, line: &SpokenLine<'_>) {
        let Some(why) = classify(line) else {
            return;
        };
        if !self.link.alerts().notifies() {
            return;
        }
        let note = Note::new(why, &self.subtitle(line), line.text);
        if note.text.is_empty() {
            return;
        }
        let guard = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(note);
        }
    }
}

/// A notifier that appends one JSON line per notification to a file
/// instead of showing it (`heard daemon --notifier log`: tests and the e2e
/// replay, which must never post a real notification).
#[derive(Debug)]
pub struct LogNotifier {
    path: std::path::PathBuf,
}

impl LogNotifier {
    /// Append to `path`.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl Notifier for LogNotifier {
    fn post(&self, n: &Notification) {
        use std::io::Write as _;
        let line = serde_json::json!({
            "ts": heard_daemon::log::now_epoch(),
            "title": n.title,
            "subtitle": n.subtitle,
            "body": n.body,
        });
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(format!("{line}\n").as_bytes()));
    }
}

// ── tests ───────────────────────────────────────────────────────────────

/// A notifier that records instead of showing (tests only).
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    posted: Mutex<Vec<Notification>>,
}

impl RecordingNotifier {
    /// Everything posted so far.
    pub fn posted(&self) -> Vec<Notification> {
        self.posted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Notifier for RecordingNotifier {
    fn post(&self, n: &Notification) {
        self.posted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(n.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heard_config::Paths;
    use heard_daemon::DaemonBuilder;

    fn line<'a>(kind: &'a str, tag: &'a str, via: &'a str, text: &'a str) -> Utterance<'a> {
        Utterance {
            text,
            tag,
            kind,
            session_id: "s1",
            via,
            project: "api",
        }
    }

    #[test]
    fn classification_follows_the_tags_the_core_emits() {
        let c = |k, t, v, x| classify(&line(k, t, v, x));
        assert_eq!(
            c(
                "tool_post",
                "tool_post_needs_you",
                "fastpath",
                "Needs your approval."
            ),
            Some(NeedsYou::Permission)
        );
        assert_eq!(
            c("tool_post", "tool_post_failure", "fastpath", "Error: boom."),
            Some(NeedsYou::Failure)
        );
        assert_eq!(
            c(
                "tool_post",
                "tool_post_command_failed",
                "fastpath",
                "Tests failed."
            ),
            Some(NeedsYou::Failure)
        );
        assert_eq!(
            c(
                "tool_pre",
                "tool_question",
                "fastpath",
                "Which database should I use?"
            ),
            Some(NeedsYou::Question)
        );
        assert_eq!(
            c(
                "tool_pre",
                "tool_question",
                "fastpath",
                "May I have access to the repo?"
            ),
            Some(NeedsYou::Permission)
        );
        assert_eq!(
            c(
                "final",
                "final_short",
                "focus_alert",
                "Sir, I need your call: ship it?"
            ),
            Some(NeedsYou::Question)
        );
        assert_eq!(
            c(
                "final",
                "final_long",
                "floor",
                "Done. Should I push the branch?"
            ),
            Some(NeedsYou::Waiting)
        );
        // Routine sign-offs and plain results are not needs-you.
        assert_eq!(
            c("final", "final_long", "floor", "Done. Anything else?"),
            None
        );
        assert_eq!(c("final", "final_short", "floor", "Tests pass."), None);
        assert_eq!(
            c("tool_pre", "tool_edit", "fastpath", "Editing auth."),
            None
        );
        assert_eq!(c("speak", "", "direct", "Should I?"), None);
        assert_eq!(c("digest", "project_flush", "digest", "3 edits."), None);
    }

    #[test]
    fn argv_carries_user_text_verbatim_and_the_script_never_does() {
        let hostile = "He said \"hi\" \\ and then\n`rm -rf` — 日本語 ✓ '; do shell script \"x\"";
        let n = Note::new(NeedsYou::Question, "-e bad · repo", hostile).single();
        let argv = OsascriptNotifier::argv(&n);
        assert_eq!(
            &argv[..6],
            &["-e", SCRIPT[0], "-e", SCRIPT[1], "-e", SCRIPT[2]]
        );
        assert_eq!(argv[6], "Heard");
        assert_eq!(argv[7], "-e bad · repo");
        assert!(argv[8].starts_with("Question: He said \"hi\" \\ and then `rm -rf`"));
        assert!(argv[8].contains("日本語 ✓"));
        assert!(!argv[8].contains('\n'));
        for s in SCRIPT {
            assert!(!s.contains("hi"), "{s}");
        }
    }

    #[test]
    fn sanitize_strips_controls_and_caps_on_char_boundaries() {
        assert_eq!(sanitize("a\nb\t\u{0}c\u{202e}d", 50), "a b c d");
        let long = "é".repeat(300);
        let s = sanitize(&long, 10);
        assert_eq!(s.chars().count(), 10);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn a_burst_is_coalesced_and_repeats_are_dropped() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(Duration::from_secs(5), Duration::from_secs(60));
        let a = Note::new(NeedsYou::Failure, "agent · api", "Tests failed.");
        let b = Note::new(NeedsYou::Question, "agent · api", "Which one?");
        let d = Note::new(NeedsYou::Permission, "agent · web", "Approve the push?");
        let first = c.offer(a.clone(), t0).expect("first posts at once");
        assert_eq!(first.body, "Failed: Tests failed.");
        assert!(c.offer(a.clone(), t0 + Duration::from_secs(1)).is_none());
        assert!(c.offer(b, t0 + Duration::from_secs(1)).is_none());
        assert!(c.offer(d, t0 + Duration::from_secs(2)).is_none());
        assert_eq!(c.due(), Some(t0 + Duration::from_secs(5)));
        assert!(c.flush(t0 + Duration::from_secs(3)).is_none());
        let folded = c.flush(t0 + Duration::from_secs(5)).expect("due");
        assert_eq!(folded.subtitle, "several agents");
        assert_eq!(
            folded.body,
            "2 alerts. Latest, needs your approval: Approve the push?"
        );
        assert!(c.due().is_none());
        // The repeat stays dropped inside the dedup window…
        assert!(c.offer(a.clone(), t0 + Duration::from_secs(30)).is_none());
        // …and posts again after it.
        assert!(c.offer(a, t0 + Duration::from_secs(70)).is_some());
    }

    fn daemon_with_alerts(dir: &std::path::Path, alerts: &str) -> Arc<Daemon> {
        crate::edition::register();
        let paths = Paths::under(dir);
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(&paths.config_path, format!("alerts: {alerts}\n")).unwrap();
        DaemonBuilder::new(paths).build()
    }

    /// A sink that records what reached it.
    #[derive(Default)]
    struct Sink(Mutex<Vec<String>>);
    impl Speech for Sink {
        fn speak(&self, u: &Utterance<'_>) {
            self.0.lock().unwrap().push(u.text.to_string());
        }
    }

    fn wait_for(rec: &RecordingNotifier, n: usize) -> Vec<Notification> {
        let t0 = Instant::now();
        while rec.posted().len() < n && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(10));
        }
        rec.posted()
    }

    #[test]
    fn alerts_semantics_speech_and_notification() {
        let needs = line("tool_post", "tool_post_failure", "fastpath", "Error: boom.");
        let routine = line("final", "final_short", "floor", "Tests pass.");
        for (alerts, spoken, notified) in [
            ("both", true, true),
            ("voice", true, false),
            ("notify", false, true),
            ("off", false, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let daemon = daemon_with_alerts(dir.path(), alerts);
            let link = DaemonLink::new();
            link.attach(&daemon);
            assert_eq!(link.alerts().key(), alerts);
            let sink = Arc::new(Sink::default());
            let gate = AlertsGate::new(sink.clone(), Arc::clone(&link));
            let rec = Arc::new(RecordingNotifier::default());
            let ext = NotifyExtension::with(
                Arc::clone(&link),
                rec.clone(),
                Duration::from_millis(1),
                Duration::from_secs(60),
            );
            for u in [&needs, &routine] {
                gate.speak(u);
                ext.on_spoken(u);
            }
            let heard = sink.0.lock().unwrap().clone();
            // Routine narration is never touched by `alerts`.
            assert!(heard.contains(&"Tests pass.".to_string()), "{alerts}");
            assert_eq!(
                heard.contains(&"Error: boom.".to_string()),
                spoken,
                "{alerts}"
            );
            let posted = if notified {
                wait_for(&rec, 1)
            } else {
                std::thread::sleep(Duration::from_millis(50));
                rec.posted()
            };
            assert_eq!(posted.len(), usize::from(notified), "{alerts}: {posted:?}");
            if notified {
                assert_eq!(posted[0].title, "Heard");
                assert_eq!(posted[0].subtitle, "agent · api");
                assert_eq!(posted[0].body, "Failed: Error: boom.");
            }
        }
    }

    #[test]
    fn an_unattached_link_defaults_to_both() {
        let link = DaemonLink::new();
        assert_eq!(link.alerts(), Alerts::Both);
        assert_eq!(link.host("s1"), None);
    }
}
