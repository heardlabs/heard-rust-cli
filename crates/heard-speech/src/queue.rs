//! [`QueuedSpeech`] — the speech queue from `engine/heard/daemon.py`, as a
//! [`Speech`] sink over a [`Tts`] backend and a [`Player`].
//!
//! Ported: `_start_speech`'s gates (empty, muted, audio-off, the mic latch's
//! safety ceiling, the mic-active / user-speaking DEFERRAL with its
//! priority-aware cap), `_enqueue_speech` (freshest-session-wins, priority
//! placement, the cap), `_drain_queue` (one consumer, one utterance at a
//! time), `_speak` (chunking, synthesis off the async workers with instant
//! cancellation, the `afplay -r` speed layering, playback, temp-file
//! cleanup), `_cancel_only` (silence: kill the current line AND drop the
//! queue), `_on_mic_active` / `_on_mic_released` (barge-in with rescue of
//! unplayed lines, the 2-second release grace), `_flush_deferred_while_mic`
//! (fresh lines replay in order past the cap; expired ones go to the
//! [`SpeechObserver`]s), `_do_mute` (cancel + clear the held buffer), and
//! `history.append` for every line that was not cancelled — played, failed
//! or skipped alike, exactly as `_drain_queue` does.
//!
//! **Concurrency.** One `tokio` task drains the queue, spawned on demand and
//! retired when the queue empties (the Python spawns its worker thread the
//! same way). Synthesis and playback are blocking, so each runs on the
//! blocking pool while the task waits on it OR on the line's [`Cancel`] —
//! whichever comes first. A cancel during a slow synth therefore takes effect
//! at once; the abandoned synth finishes on its own and deletes its file.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use heard_daemon::dedup::Dedup;
use heard_daemon::events::EventBus;
use heard_daemon::speech::{Hold, LineOptions, Speech, Utterance, VIA_FILLER, VIA_NOTICE};
use heard_state::clock::{Clock, SystemClock};
use heard_state::history::History;
use heard_state::pyjson::PyValue;
use heard_tts::{Tts, TtsError};

use crate::observer::SpeechObserver;
use crate::player::{Cancel, PlayOutcome, Player};
use crate::policy::{self, Queued};

/// `MIC_ACTIVE_MAX_S`.
pub const MIC_ACTIVE_MAX_S: f64 = 90.0;
/// `MIC_ACTIVE_CALL_MAX_S`.
pub const MIC_ACTIVE_CALL_MAX_S: f64 = 4.0 * 60.0 * 60.0;
/// `MIC_RELEASE_GRACE_S`.
pub const MIC_RELEASE_GRACE: Duration = Duration::from_secs(2);
/// `_user_speaking_now`'s lost-release ceiling.
pub const USER_SPEAKING_MAX_S: f64 = 90.0;
/// `_tour_active`'s ceiling: an app that died mid-tour must not mute forever.
pub const TOUR_HOLD_MAX_S: f64 = 1800.0;

/// One line on the queue — the Python's `(text, cfg, persona, session_id,
/// voice_override, history_meta)` tuple plus its two routing flags.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechItem {
    /// The words.
    pub text: String,
    /// Owning session, `""` for none (answers, acks, announcements).
    pub session_id: String,
    /// `history_meta["kind"]`.
    pub kind: String,
    /// `history_meta["tag"]`.
    pub tag: String,
    /// `history_meta["via"]`.
    pub via: String,
    /// `history_meta["repo_name"]`.
    pub project: String,
    /// A per-agent voice that wins over the persona / config voice.
    pub voice_override: Option<String>,
    /// Results / decisions / errors — see [`policy::enqueue`].
    pub priority: bool,
    /// Scheduler flushes that must not clear each other.
    pub coexists: bool,
}

impl SpeechItem {
    /// A plain line with no metadata.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            session_id: String::new(),
            kind: String::new(),
            tag: String::new(),
            via: String::new(),
            project: String::new(),
            voice_override: None,
            priority: false,
            coexists: false,
        }
    }

    /// Whether the line carries history meta — the Python's `if hmeta:`.
    /// Every routed line does (the daemon always passes kind / tag / via /
    /// repo_name); a bare [`SpeechItem::new`] line does not, and is never
    /// written to `history.jsonl`.
    #[must_use]
    pub fn has_history_meta(&self) -> bool {
        [&self.kind, &self.tag, &self.via, &self.project]
            .iter()
            .any(|s| !s.is_empty())
    }

    /// Builder: owning session.
    #[must_use]
    pub fn session(mut self, id: &str) -> Self {
        self.session_id = id.into();
        self
    }
    /// Builder: event kind.
    #[must_use]
    pub fn kind(mut self, kind: &str) -> Self {
        self.kind = kind.into();
        self
    }
    /// Builder: priority.
    #[must_use]
    pub fn priority(mut self, p: bool) -> Self {
        self.priority = p;
        self
    }
    /// Builder: coexists.
    #[must_use]
    pub fn coexists(mut self, c: bool) -> Self {
        self.coexists = c;
        self
    }
}

impl Queued for SpeechItem {
    fn session(&self) -> &str {
        &self.session_id
    }
    fn kind(&self) -> &str {
        &self.kind
    }
}

/// The slice of config the queue reads per utterance.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechSettings {
    /// "Pause Heard".
    pub muted: bool,
    /// The notch speaker-off: record, don't voice.
    pub audio_off: bool,
    /// `cfg["speed"]`.
    pub speed: f64,
    /// `persona.voice or cfg["voice"]`, already resolved.
    pub voice: String,
    /// `persona.kokoro_voice or cfg["kokoro_voice"] or "bm_george"`.
    pub kokoro_voice: String,
    /// Whether the active backend is Kokoro (its voice ids are a different
    /// namespace — `Daemon._voice`).
    pub use_kokoro_voice: bool,
    /// `config.tts_lang_for(cfg)`.
    pub lang: String,
    /// `persona.name`, for the history record.
    pub persona: String,
    /// `cfg["voice_mode"]` — `ambient` never latches the mic.
    pub voice_mode: String,
}

impl Default for SpeechSettings {
    fn default() -> Self {
        Self {
            muted: false,
            audio_off: false,
            speed: 1.0,
            voice: String::new(),
            kokoro_voice: "bm_george".into(),
            use_kokoro_voice: false,
            lang: "en-us".into(),
            persona: String::new(),
            voice_mode: String::new(),
        }
    }
}

/// Knobs the Python keeps as class attributes. Defaults are the Python's.
#[derive(Debug, Clone)]
pub struct SpeechLimits {
    /// `_queue_max`.
    pub queue_max: usize,
    /// `_DEFERRED_MIC_MAX`.
    pub deferred_mic_max: usize,
    /// `_DEFERRED_MAX_AGE_S`.
    pub deferred_max_age_s: f64,
    /// `MIC_RELEASE_GRACE_S`.
    pub mic_release_grace: Duration,
    /// `MIC_ACTIVE_MAX_S`.
    pub mic_active_max_s: f64,
    /// `MIC_ACTIVE_CALL_MAX_S`.
    pub mic_active_call_max_s: f64,
}

impl Default for SpeechLimits {
    fn default() -> Self {
        Self {
            queue_max: policy::QUEUE_MAX,
            deferred_mic_max: policy::DEFERRED_MIC_MAX,
            deferred_max_age_s: policy::DEFERRED_MAX_AGE_S,
            mic_release_grace: MIC_RELEASE_GRACE,
            mic_active_max_s: MIC_ACTIVE_MAX_S,
            mic_active_call_max_s: MIC_ACTIVE_CALL_MAX_S,
        }
    }
}

/// How one utterance ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Every chunk played to the end.
    Played,
    /// Cancelled — silence, mute, barge-in, or superseded.
    Cancelled,
    /// Never synthesised: muted / audio-off / mic active / no voice.
    Skipped(&'static str),
    /// A chunk failed to synthesise or play.
    Failed,
}

/// What [`QueuedSpeech::start_speech`] did with a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// On the queue.
    Queued,
    /// Held until the mic frees up.
    Deferred,
    /// Dropped: `empty_text`, `muted`, `audio_off`, `dup_tool_line`.
    Dropped(&'static str),
}

struct State {
    queue: Vec<SpeechItem>,
    deferred: Vec<(SpeechItem, bool, f64)>,
    current: Option<Cancel>,
    /// The session of the line in `current` (for [`Speech::cut_session`]).
    current_session: Option<String>,
    worker_running: bool,
    mic_active: bool,
    mic_active_at: f64,
    user_speaking: bool,
    user_speaking_at: f64,
    release_generation: u64,
    settings: SpeechSettings,
    delivered: Vec<(String, Delivery)>,
    /// `_tour_hold_at` (wall clock, 0 = no tour).
    tour_hold_at: f64,
    /// `_last_utterance_id`.
    last_id: Option<String>,
}

struct Inner {
    state: Mutex<State>,
    tts: RwLock<Arc<dyn Tts>>,
    player: Arc<dyn Player>,
    history: Option<History>,
    observers: Vec<Arc<dyn SpeechObserver>>,
    dedup: Dedup,
    clock: Arc<dyn Clock>,
    handle: tokio::runtime::Handle,
    tmp_dir: PathBuf,
    call_app_running: Box<dyn Fn() -> bool + Send + Sync>,
    limits: SpeechLimits,
    hold_exempt: Vec<String>,
    events: Option<EventBus>,
    settings_source: Option<SettingsSource>,
}

/// A per-utterance settings reader: given the queue's current settings,
/// the settings this line is spoken with. See
/// [`QueuedSpeechBuilder::settings_source`].
pub type SettingsSource = Arc<dyn Fn(&SpeechSettings) -> SpeechSettings + Send + Sync>;

/// The speech queue. Cheap to clone; clones are the same queue.
#[derive(Clone)]
pub struct QueuedSpeech {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for QueuedSpeech {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.lock();
        f.debug_struct("QueuedSpeech")
            .field("queued", &st.queue.len())
            .field("deferred", &st.deferred.len())
            .field("speaking", &st.current.is_some())
            .finish_non_exhaustive()
    }
}

/// Builds a [`QueuedSpeech`].
pub struct QueuedSpeechBuilder {
    tts: Arc<dyn Tts>,
    player: Arc<dyn Player>,
    handle: tokio::runtime::Handle,
    history: Option<History>,
    observers: Vec<Arc<dyn SpeechObserver>>,
    clock: Arc<dyn Clock>,
    tmp_dir: PathBuf,
    call_app_running: Box<dyn Fn() -> bool + Send + Sync>,
    limits: SpeechLimits,
    settings: SpeechSettings,
    hold_exempt: Vec<String>,
    events: Option<EventBus>,
    settings_source: Option<SettingsSource>,
    history_policy: Option<heard_state::HistoryPolicySource>,
}

impl QueuedSpeechBuilder {
    /// Append `history.jsonl` records under `config_dir` for every line that
    /// was not cancelled (see [`policy::appends_history`]).
    #[must_use]
    pub fn history(mut self, config_dir: impl Into<PathBuf>) -> Self {
        self.history = Some(History::new(config_dir));
        self
    }
    /// What `history.jsonl` may keep of the user's words, read per append
    /// (default: everything; see `heard_state::history_policy`). Order with
    /// [`QueuedSpeechBuilder::history`] does not matter.
    #[must_use]
    pub fn history_policy(mut self, policy: heard_state::HistoryPolicySource) -> Self {
        self.history_policy = Some(policy);
        self
    }
    /// Add a [`SpeechObserver`] (default: none). Observers are told in the
    /// order they were added.
    #[must_use]
    pub fn observer(mut self, observer: Arc<dyn SpeechObserver>) -> Self {
        self.observers.push(observer);
        self
    }
    /// The monotonic clock the deferral ages and mic ceilings read.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
    /// Where synthesised audio is written before playback (default: the
    /// system temp dir, like `tempfile.mkstemp`).
    #[must_use]
    pub fn tmp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.tmp_dir = dir.into();
        self
    }
    /// `audio_monitor.call_app_running` — buys the long mic ceiling.
    #[must_use]
    pub fn call_app_running(mut self, f: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.call_app_running = Box::new(f);
        self
    }
    /// Override the Python's class-attribute limits.
    #[must_use]
    pub fn limits(mut self, limits: SpeechLimits) -> Self {
        self.limits = limits;
        self
    }
    /// Initial settings.
    #[must_use]
    pub fn settings(mut self, settings: SpeechSettings) -> Self {
        self.settings = settings;
        self
    }
    /// Sessions whose lines the TOUR hold does not defer: lines the user
    /// asked for directly (an answer to their own question), not agent
    /// narration. Default: none.
    #[must_use]
    pub fn hold_exempt_sessions(mut self, sessions: &[&str]) -> Self {
        self.hold_exempt = sessions.iter().map(|s| (*s).to_owned()).collect();
        self
    }
    /// Where `speech_started` / `speech_finished` go (the daemon's
    /// `subscribe` stream). Default: nowhere.
    #[must_use]
    pub fn events(mut self, bus: EventBus) -> Self {
        self.events = Some(bus);
        self
    }
    /// Read the settings per utterance (the Python reads `cfg` and the
    /// persona for every line it queues): called once per line, off the
    /// queue lock, just before synthesis, with the queue's current settings.
    /// Its answer is what the line is spoken with and becomes the queue's
    /// settings. It must keep `muted` as given — mute is the queue's own
    /// state. Default: none (the settings change only through
    /// [`QueuedSpeech::set_settings`]).
    #[must_use]
    pub fn settings_source(
        mut self,
        f: impl Fn(&SpeechSettings) -> SpeechSettings + Send + Sync + 'static,
    ) -> Self {
        self.settings_source = Some(Arc::new(f));
        self
    }
    /// Finish.
    #[must_use]
    pub fn build(self) -> QueuedSpeech {
        QueuedSpeech {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    queue: Vec::new(),
                    deferred: Vec::new(),
                    current: None,
                    current_session: None,
                    worker_running: false,
                    mic_active: false,
                    mic_active_at: 0.0,
                    user_speaking: false,
                    user_speaking_at: 0.0,
                    release_generation: 0,
                    settings: self.settings,
                    delivered: Vec::new(),
                    tour_hold_at: 0.0,
                    last_id: None,
                }),
                tts: RwLock::new(self.tts),
                player: self.player,
                history: match (self.history, self.history_policy) {
                    (Some(h), Some(p)) => Some(h.with_policy(p)),
                    (h, _) => h,
                },
                observers: self.observers,
                dedup: Dedup::new(),
                clock: self.clock,
                handle: self.handle,
                tmp_dir: self.tmp_dir,
                call_app_running: self.call_app_running,
                limits: self.limits,
                hold_exempt: self.hold_exempt,
                events: self.events,
                settings_source: self.settings_source,
            }),
        }
    }
}

fn head8(s: &str) -> String {
    s.chars().take(8).collect()
}

fn tmp_name(ext: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    format!(
        "heard-{}-{}{ext}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

impl QueuedSpeech {
    /// Start building. `handle` is the daemon's one runtime.
    #[must_use]
    pub fn builder(
        tts: Arc<dyn Tts>,
        player: Arc<dyn Player>,
        handle: tokio::runtime::Handle,
    ) -> QueuedSpeechBuilder {
        QueuedSpeechBuilder {
            tts,
            player,
            handle,
            history: None,
            observers: Vec::new(),
            clock: Arc::new(SystemClock::new()),
            tmp_dir: std::env::temp_dir(),
            call_app_running: Box::new(|| false),
            limits: SpeechLimits::default(),
            settings: SpeechSettings::default(),
            hold_exempt: Vec::new(),
            events: None,
            settings_source: None,
            history_policy: None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> f64 {
        self.inner.clock.monotonic()
    }

    // --- settings ------------------------------------------------------------

    /// Replace the settings (a config reload).
    pub fn set_settings(&self, settings: SpeechSettings) {
        self.lock().settings = settings;
    }

    /// Current settings.
    #[must_use]
    pub fn settings(&self) -> SpeechSettings {
        self.lock().settings.clone()
    }

    /// Swap the TTS backend (`self.tts = self._make_tts()`).
    pub fn set_tts(&self, tts: Arc<dyn Tts>) {
        *self.inner.tts.write().unwrap_or_else(|e| e.into_inner()) = tts;
    }

    /// `_do_mute` — silence now, drop the held-while-dictating buffer too, and
    /// refuse new lines until [`Self::unmute`]. Persisting `muted: true` to
    /// `config.yaml` stays with the caller that owns the config.
    pub fn mute(&self) {
        self.cancel_only();
        let mut st = self.lock();
        st.deferred.clear();
        st.settings.muted = true;
        drop(st);
        heard_daemon::dlog!("muted", source = "speech");
    }

    /// Clear `muted`.
    pub fn unmute(&self) {
        self.lock().settings.muted = false;
    }

    // --- introspection (tests, `status`) ------------------------------------

    /// Texts on the queue, in play order.
    #[must_use]
    pub fn queued(&self) -> Vec<String> {
        self.lock().queue.iter().map(|i| i.text.clone()).collect()
    }

    /// Held lines as `(text, priority)`.
    #[must_use]
    pub fn deferred(&self) -> Vec<(String, bool)> {
        self.lock()
            .deferred
            .iter()
            .map(|(i, p, _)| (i.text.clone(), *p))
            .collect()
    }

    /// Whether a line is currently being synthesised or played.
    #[must_use]
    pub fn is_speaking(&self) -> bool {
        self.lock().current.is_some()
    }

    /// Whether the mic latch is set.
    #[must_use]
    pub fn mic_active(&self) -> bool {
        self.lock().mic_active
    }

    /// Every finished line and how it ended, oldest first.
    #[must_use]
    pub fn deliveries(&self) -> Vec<(String, Delivery)> {
        self.lock().delivered.clone()
    }

    /// Wait until the queue is empty and the worker has retired.
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let st = self.lock();
                if st.queue.is_empty() && !st.worker_running {
                    return true;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    // --- mic -----------------------------------------------------------------

    fn mic_stale(&self, st: &State) -> bool {
        if st.mic_active_at == 0.0 {
            return false;
        }
        let held = self.now() - st.mic_active_at;
        let ceiling = if (self.inner.call_app_running)() {
            self.inner.limits.mic_active_call_max_s
        } else {
            self.inner.limits.mic_active_max_s
        };
        held > ceiling
    }

    fn user_speaking_now(&self, st: &mut State) -> bool {
        if !st.user_speaking {
            return false;
        }
        if self.now() - st.user_speaking_at > USER_SPEAKING_MAX_S && !st.mic_active {
            st.user_speaking = false;
            heard_daemon::dlog!("user_speaking_timeout");
            return false;
        }
        true
    }

    /// `_tour_active` — a tour hold older than [`TOUR_HOLD_MAX_S`] lapses.
    fn tour_active(&self, st: &mut State) -> bool {
        if st.tour_hold_at == 0.0 {
            return false;
        }
        if self.inner.clock.wall() - st.tour_hold_at > TOUR_HOLD_MAX_S {
            st.tour_hold_at = 0.0;
            return false;
        }
        true
    }

    fn exempt(&self, session_id: &str) -> bool {
        self.inner.hold_exempt.iter().any(|s| s == session_id)
    }

    /// `voice_hold` — CHANNEL B: `_cancel_only(preserve_answer=True)` (the
    /// playing line is cut and the queue dropped), then hold new lines.
    pub fn hold_user(&self) {
        self.cancel_only();
        self.set_user_speaking(true);
    }

    /// `voice_release` — clear channel B and replay the held batch.
    pub fn release_user(&self) {
        self.lock().user_speaking = false;
        self.flush_deferred();
    }

    /// `tour_hold` — rescue the unplayed queue into the FRONT of the held
    /// buffer, cut what is playing, hold new lines.
    pub fn hold_tour(&self) {
        let now = self.now();
        let wall = self.inner.clock.wall();
        {
            let mut st = self.lock();
            st.tour_hold_at = wall;
            if !st.queue.is_empty() {
                let rescued: Vec<(SpeechItem, bool, f64)> =
                    st.queue.drain(..).map(|item| (item, false, now)).collect();
                let rest = std::mem::take(&mut st.deferred);
                st.deferred = rescued;
                st.deferred.extend(rest);
            }
        }
        self.cancel_only();
    }

    /// `tour_release`.
    pub fn release_tour(&self) {
        self.lock().tour_hold_at = 0.0;
        self.flush_deferred();
    }

    /// Force-clear a latch that outlived its ceiling; `true` when it did (the
    /// caller then replays the backlog).
    fn clear_stale_latch(&self, st: &mut State, at: &str) -> bool {
        if st.mic_active && self.mic_stale(st) {
            let held = self.now() - st.mic_active_at;
            st.mic_active = false;
            st.mic_active_at = 0.0;
            heard_daemon::dlog!(
                "mic_suppress_ceiling",
                held_s = format!("{held:.1}"),
                at = at
            );
            return true;
        }
        false
    }

    /// Set the channel-A latch directly (the Python tests poke `_mic_active`;
    /// the audio monitor calls [`Self::on_mic_active`]).
    pub fn set_mic_active(&self, active: bool) {
        let now = self.now();
        let mut st = self.lock();
        st.mic_active = active;
        st.mic_active_at = if active { now } else { 0.0 };
    }

    /// Channel B: the user's own push-to-talk hold (`voice_hold` /
    /// `voice_release`).
    pub fn set_user_speaking(&self, speaking: bool) {
        let now = self.now();
        let mut st = self.lock();
        st.user_speaking = speaking;
        st.user_speaking_at = now;
    }

    /// `_on_mic_active` — barge-in. Kill what is playing, flip the latch,
    /// and RESCUE the unplayed queue into the held buffer so it replays on
    /// release rather than dying with the cancel. Ambient mode is our own
    /// always-on capture and never latches.
    pub fn on_mic_active(&self) {
        let now = self.now();
        let mut st = self.lock();
        st.release_generation += 1;
        if st
            .settings
            .voice_mode
            .trim()
            .eq_ignore_ascii_case("ambient")
        {
            let was = st.mic_active;
            st.mic_active = false;
            drop(st);
            if was {
                self.flush_deferred();
            }
            return;
        }
        st.mic_active = true;
        st.mic_active_at = now;
        let rescued: Vec<(SpeechItem, bool, f64)> =
            st.queue.drain(..).map(|item| (item, false, now)).collect();
        if !rescued.is_empty() {
            let rest = std::mem::take(&mut st.deferred);
            st.deferred = rescued;
            st.deferred.extend(rest);
        }
        drop(st);
        self.cancel_only();
        heard_daemon::dlog!("mic_active");
    }

    /// `_on_mic_released` — clear the latch after the release grace, unless
    /// the mic re-trips first (inter-phrase pauses in dictation).
    pub fn on_mic_released(&self) {
        heard_daemon::dlog!("mic_released_pending");
        let generation = {
            let mut st = self.lock();
            st.release_generation += 1;
            st.release_generation
        };
        let me = self.clone();
        let grace = self.inner.limits.mic_release_grace;
        self.inner.handle.spawn(async move {
            tokio::time::sleep(grace).await;
            {
                let mut st = me.lock();
                if st.release_generation != generation {
                    return;
                }
                st.mic_active = false;
                st.mic_active_at = 0.0;
            }
            heard_daemon::dlog!("mic_released");
            me.flush_deferred();
        });
    }

    /// `_flush_deferred_while_mic` — replay the held batch in order, straight
    /// onto the queue (bypassing the cap: the batch is already bounded), if
    /// neither channel still holds the floor. Lines older than the age limit
    /// are dropped from speech and handed to every
    /// [`SpeechObserver::on_expired`], so a record of the session still
    /// covers them.
    pub fn flush_deferred(&self) {
        let now = self.now();
        let mut st = self.lock();
        if st.mic_active || self.user_speaking_now(&mut st) || self.tour_active(&mut st) {
            drop(st);
            heard_daemon::dlog!("mic_deferred_flush_held");
            return;
        }
        let deferred = std::mem::take(&mut st.deferred);
        if deferred.is_empty() {
            return;
        }
        let (fresh, expired) =
            policy::split_deferred(deferred, now, self.inner.limits.deferred_max_age_s);
        if !expired.is_empty() {
            heard_daemon::dlog!("mic_deferred_expired", dropped = expired.len());
            for observer in &self.inner.observers {
                for item in &expired {
                    observer.on_expired(item);
                }
            }
        }
        if fresh.is_empty() {
            return;
        }
        let count = fresh.len();
        st.queue.extend(fresh);
        self.ensure_worker(&mut st);
        drop(st);
        heard_daemon::dlog!("mic_deferred_flush", count = count);
    }

    // --- the front door ------------------------------------------------------

    /// `_start_speech` — queue a line behind whatever is playing, or hold it
    /// while the listener is talking, or drop it.
    pub fn start_speech(&self, mut item: SpeechItem) -> Admission {
        let trimmed = heard_state::py::strip(&item.text);
        if trimmed.is_empty() {
            return Admission::Dropped("empty_text");
        }
        item.text = trimmed.to_owned();
        let mut st = self.lock();
        if st.settings.muted {
            drop(st);
            heard_daemon::dlog!(
                "speech_skipped",
                reason = "muted",
                session = head8(&item.session_id)
            );
            return Admission::Dropped("muted");
        }
        if st.settings.audio_off {
            drop(st);
            heard_daemon::dlog!(
                "speech_skipped",
                reason = "audio_off",
                session = head8(&item.session_id)
            );
            return Admission::Dropped("audio_off");
        }
        if self.clear_stale_latch(&mut st, "start_speech") {
            drop(st);
            self.flush_deferred();
            st = self.lock();
        }
        let tour_holds = !self.exempt(&item.session_id) && self.tour_active(&mut st);
        if st.mic_active || self.user_speaking_now(&mut st) || tour_holds {
            let now = self.now();
            let priority = item.priority;
            let session = head8(&item.session_id);
            st.deferred.push((item, priority, now));
            let max = self.inner.limits.deferred_mic_max;
            policy::cap_deferred(&mut st.deferred, max);
            let held = st.deferred.len();
            drop(st);
            heard_daemon::dlog!("speech_deferred_mic", session = session, held = held);
            return Admission::Deferred;
        }
        self.enqueue_locked(&mut st, item);
        Admission::Queued
    }

    /// `_enqueue_speech` — put an already-admitted line on the queue.
    pub fn enqueue(&self, item: SpeechItem) {
        let mut st = self.lock();
        self.enqueue_locked(&mut st, item);
    }

    fn enqueue_locked(&self, st: &mut State, item: SpeechItem) {
        let session = head8(&item.session_id);
        let (priority, coexists) = (item.priority, item.coexists);
        let max = self.inner.limits.queue_max;
        let dropped = policy::enqueue(&mut st.queue, item, priority, coexists, max);
        if dropped.other_session > 0 {
            heard_daemon::dlog!(
                "queue_drop_other_session",
                dropped = dropped.other_session,
                session = session.clone()
            );
        }
        if dropped.superseded > 0 {
            heard_daemon::dlog!(
                "queue_drop",
                dropped = dropped.superseded,
                session = session,
                reason = "final_supersedes_routine"
            );
        }
        if dropped.cap > 0 {
            heard_daemon::dlog!("queue_drop", dropped = dropped.cap);
        }
        self.ensure_worker(st);
    }

    fn ensure_worker(&self, st: &mut State) {
        if st.worker_running || st.queue.is_empty() {
            return;
        }
        st.worker_running = true;
        let me = self.clone();
        self.inner.handle.spawn(async move { me.drain().await });
    }

    /// `_cancel_only` — silence: kill the current line AND drop everything
    /// queued behind it.
    pub fn cancel_only(&self) {
        let mut st = self.lock();
        if let Some(c) = &st.current {
            c.set();
        }
        st.queue.clear();
    }

    // --- the worker ----------------------------------------------------------

    async fn drain(&self) {
        loop {
            let (item, cancel) = {
                let mut st = self.lock();
                if st.queue.is_empty() {
                    st.worker_running = false;
                    return;
                }
                let item = st.queue.remove(0);
                let cancel = Cancel::new();
                st.current = Some(cancel.clone());
                st.current_session = Some(item.session_id.clone());
                (item, cancel)
            };
            let outcome = self.speak_one(&item, &cancel).await;
            let settings = {
                let mut st = self.lock();
                if st.current.as_ref().is_some_and(|c| c.same(&cancel)) {
                    st.current = None;
                    st.current_session = None;
                }
                st.delivered.push((item.text.clone(), outcome));
                st.settings.clone()
            };
            // Python's rule, not "only if it played": see
            // `policy::appends_history`.
            if policy::appends_history(cancel.is_set(), item.has_history_meta()) {
                self.append_history(&item, &settings);
            }
            for observer in &self.inner.observers {
                observer.on_spoken(&item, outcome);
            }
        }
    }

    fn voice_for(&self, item: &SpeechItem, settings: &SpeechSettings) -> String {
        if let Some(v) = &item.voice_override {
            return v.clone();
        }
        if settings.use_kokoro_voice {
            settings.kokoro_voice.clone()
        } else {
            settings.voice.clone()
        }
    }

    /// `_speak` — synthesise and play one line, chunk by chunk.
    async fn speak_one(&self, item: &SpeechItem, cancel: &Cancel) -> Delivery {
        let settings = {
            let mut st = self.lock();
            if st.settings.muted || st.settings.audio_off {
                let reason = if st.settings.muted {
                    "muted"
                } else {
                    "audio_off"
                };
                drop(st);
                heard_daemon::dlog!("synth_skipped", reason = reason);
                return Delivery::Skipped(reason);
            }
            if self.clear_stale_latch(&mut st, "speak") {
                drop(st);
                self.flush_deferred();
                st = self.lock();
            }
            if st.mic_active {
                drop(st);
                heard_daemon::dlog!("synth_skipped", reason = "mic_active");
                return Delivery::Skipped("mic_active");
            }
            st.settings.clone()
        };
        let settings = match &self.inner.settings_source {
            Some(source) => {
                let mut fresh = source(&settings);
                fresh.muted = settings.muted;
                self.lock().settings.clone_from(&fresh);
                fresh
            }
            None => settings,
        };
        let tts = Arc::clone(&self.inner.tts.read().unwrap_or_else(|e| e.into_inner()));
        if !tts.is_configured() {
            heard_daemon::dlog!("synth_skipped", reason = "no_voice_configured");
            return Delivery::Skipped("no_voice_configured");
        }
        let voice = self.voice_for(item, &settings);
        let chunks = policy::split(&item.text);
        let mut all_ok = !chunks.is_empty();
        for chunk in chunks {
            if cancel.is_set() {
                return Delivery::Cancelled;
            }
            let path = self.inner.tmp_dir.join(tmp_name(tts.audio_ext()));
            let job = {
                let (tts, path, cancel) = (Arc::clone(&tts), path.clone(), cancel.clone());
                let (voice, lang, speed) = (voice.clone(), settings.lang.clone(), settings.speed);
                move || -> Result<(), TtsError> {
                    let r = tts
                        .synth(&chunk, &voice, speed, &lang)
                        .and_then(|audio| audio.write(&path).map_err(TtsError::from));
                    if cancel.is_set() || r.is_err() {
                        let _ = std::fs::remove_file(&path);
                    }
                    r
                }
            };
            let synth = self.inner.handle.spawn_blocking(job);
            let result = tokio::select! {
                r = synth => r,
                () = cancel.cancelled() => {
                    heard_daemon::dlog!("synth_abandoned", reason = "cancel_during_synth");
                    return Delivery::Cancelled;
                }
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    heard_daemon::dlog!("synth_failed", err = e.to_string());
                    all_ok = false;
                    continue;
                }
                Err(e) => {
                    heard_daemon::dlog!("synth_failed", err = e.to_string());
                    let _ = std::fs::remove_file(&path);
                    all_ok = false;
                    continue;
                }
            }
            if cancel.is_set() {
                let _ = std::fs::remove_file(&path);
                return Delivery::Cancelled;
            }
            let rate = policy::play_rate(settings.speed, tts.max_native_speed());
            let player = Arc::clone(&self.inner.player);
            let (play_path, play_cancel) = (path.clone(), cancel.clone());
            self.emit("speech_started", &item.kind);
            let played = self
                .inner
                .handle
                .spawn_blocking(move || player.play(&play_path, rate, &play_cancel))
                .await
                .unwrap_or_else(|e| PlayOutcome::Failed(e.to_string()));
            self.emit("speech_finished", &item.kind);
            let _ = std::fs::remove_file(&path);
            match played {
                PlayOutcome::Finished => {}
                PlayOutcome::Cancelled => return Delivery::Cancelled,
                PlayOutcome::Failed(err) => {
                    heard_daemon::dlog!("afplay_nonzero", err = err);
                    all_ok = false;
                }
            }
        }
        if all_ok {
            Delivery::Played
        } else {
            Delivery::Failed
        }
    }

    fn emit(&self, name: &str, kind: &str) {
        if let Some(bus) = &self.inner.events {
            bus.emit(name, serde_json::json!({ "kind": kind }));
        }
    }

    fn append_history(&self, item: &SpeechItem, settings: &SpeechSettings) {
        let Some(history) = &self.inner.history else {
            return;
        };
        let id = heard_state::history::new_utterance_id();
        self.lock().last_id = Some(id.clone());
        let mut rec: Vec<(String, PyValue)> = Vec::new();
        for (k, v) in [
            ("kind", &item.kind),
            ("tag", &item.tag),
            ("via", &item.via),
            ("repo_name", &item.project),
        ] {
            rec.push((k.into(), PyValue::Str(v.clone())));
        }
        rec.push(("id".into(), PyValue::Str(id)));
        rec.push(("session_id".into(), PyValue::Str(item.session_id.clone())));
        rec.push(("spoken".into(), PyValue::Str(item.text.clone())));
        rec.push(("voice".into(), PyValue::Str(self.voice_for(item, settings))));
        rec.push(("persona".into(), PyValue::Str(settings.persona.clone())));
        history.append(&rec);
    }
}

impl QueuedSpeech {
    fn is_dup_tool_line(&self, u: &Utterance<'_>) -> bool {
        if matches!(u.kind, "tool_pre" | "tool_post")
            && u.via == "fastpath"
            && self
                .inner
                .dedup
                .is_duplicate_tool_line(u.session_id, u.text)
        {
            heard_daemon::dlog!(
                "event_drop",
                kind = u.kind,
                tag = u.tag,
                reason = "fastpath_dup_tool_line"
            );
            return true;
        }
        false
    }
}

/// The daemon's sink: every routed utterance lands here.
///
/// `priority` is `kind == "final"` (the Python's `priority=(kind == "final")`
/// on the brain and floor paths), `coexists` is a scheduler `digest` flush
/// or a canned filler / notice line (which also carry no history meta), and
/// a fast-path tool line repeated inside 25 s is dropped
/// (`_is_duplicate_tool_line`).
impl Speech for QueuedSpeech {
    fn speak(&self, u: &Utterance<'_>) {
        let plain = u.via == VIA_FILLER || u.via == VIA_NOTICE;
        self.speak_with(
            u,
            LineOptions {
                priority: u.kind == "final",
                coexists: u.via == "digest" || plain,
                history: !plain,
            },
        );
    }

    fn speak_with(&self, u: &Utterance<'_>, opts: LineOptions) {
        if self.is_dup_tool_line(u) {
            return;
        }
        let meta = |s: &str| {
            if opts.history {
                s.to_owned()
            } else {
                String::new()
            }
        };
        self.start_speech(SpeechItem {
            text: u.text.to_owned(),
            session_id: u.session_id.to_owned(),
            kind: meta(u.kind),
            tag: meta(u.tag),
            via: meta(u.via),
            project: meta(u.project),
            voice_override: None,
            priority: opts.priority,
            coexists: opts.coexists,
        });
    }

    /// The daemon's `stop` / `cancel`: `_cancel_only`.
    fn cancel(&self) {
        self.cancel_only();
    }

    fn is_speaking(&self) -> bool {
        let inner = self.lock();
        inner.current.is_some() || !inner.queue.is_empty()
    }

    fn queue_state(&self) -> (bool, usize) {
        let st = self.lock();
        (st.current.is_some(), st.queue.len())
    }

    fn hold(&self, hold: Hold) {
        match hold {
            Hold::User => self.hold_user(),
            Hold::Tour => self.hold_tour(),
        }
    }

    fn release(&self, hold: Hold) {
        match hold {
            Hold::User => self.release_user(),
            Hold::Tour => self.release_tour(),
        }
    }

    fn discard_held(&self) {
        self.lock().deferred.clear();
    }

    fn clear_mic_latch(&self) {
        self.set_mic_active(false);
        self.flush_deferred();
    }

    fn mic_active(&self) -> bool {
        self.lock().mic_active
    }

    fn is_holding(&self) -> bool {
        let mut st = self.lock();
        st.mic_active || self.user_speaking_now(&mut st) || self.tour_active(&mut st)
    }

    fn drop_session(&self, session_id: &str) -> usize {
        let mut st = self.lock();
        let before = st.queue.len();
        st.queue.retain(|i| i.session_id != session_id);
        before - st.queue.len()
    }

    fn cut_session(&self, session_id: &str) -> usize {
        let mut st = self.lock();
        let before = st.queue.len();
        st.queue.retain(|i| i.session_id != session_id);
        if st.current_session.as_deref() == Some(session_id) {
            if let Some(c) = &st.current {
                c.set();
            }
        }
        before - st.queue.len()
    }

    fn last_utterance_id(&self) -> Option<String> {
        self.lock().last_id.clone()
    }
}
