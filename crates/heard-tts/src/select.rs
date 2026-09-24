//! Backend selection — ported from `Daemon._make_tts` in
//! `engine/heard/daemon.py`, plus a registry for backends that live outside
//! this crate.
//!
//! # The ladder
//!
//! 1. A **preferred** registered backend (see below), first registered wins.
//! 2. `elevenlabs_api_key` set → [`ElevenLabsTts`] — the user's own
//!    ElevenLabs key. Off unless the user sets one.
//! 3. A **fallback** registered backend, first registered wins.
//! 4. Local Kokoro, **only if already downloaded** — never auto-pulled.
//! 5. Otherwise [`NullTts`].
//!
//! With nothing registered this is the free edition's whole ladder:
//! your own key, else Kokoro, else silence.
//!
//! # The registry
//!
//! A crate outside this one adds a backend by implementing
//! [`BackendFactory`] and calling [`register_backend`] once at startup. On
//! every selection the factory answers [`BackendFactory::claim`] from the
//! [`ConfigView`] (which carries the whole merged config in
//! [`ConfigView::config`]):
//!
//! * [`Claim::Preferred`] — outranks a plain ElevenLabs key;
//! * [`Claim::Fallback`] — used only when there is no key;
//! * [`Claim::No`] — not usable right now.
//!
//! Registering a second factory with the same [`BackendFactory::name`]
//! replaces the first. [`BackendRegistry`] is the same thing as a value, for
//! tests and for callers that do not want process-global state.
//!
//! # Why this is a pure function
//!
//! The Python reads `self.cfg` inline, so testing the ladder means building a
//! daemon. [`decide`] takes a [`ConfigView`] — a borrowed read-only slice of
//! the config plus one filesystem fact — and returns a [`Backend`] without
//! constructing anything or touching the network. Every branch is then a
//! two-line test. [`select_backend`] is the thin layer that turns the decision
//! into a live `Box<dyn Tts>`.

use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::{Map, Value};

use crate::{ElevenLabsTts, NullTts, Tts};

/// Model file sizes pinned upstream, from `engine/heard/tts/kokoro.py`.
///
/// Existence alone is not enough: a truncated download leaves a partial file
/// that ONNX Runtime explodes on at load time (`InvalidProtobuf`), and the
/// daemon would otherwise never re-pull it. Checking the byte size against the
/// pinned value catches truncation for free, without a SHA-256 scan of 325 MB
/// at every daemon start.
pub const MODEL_SIZE: u64 = 325_532_387;
/// Pinned size of `voices-v1.0.bin`.
pub const VOICES_SIZE: u64 = 28_214_398;

/// `kokoro-v1.0.onnx`.
pub const MODEL_FILE: &str = "kokoro-v1.0.onnx";
/// `voices-v1.0.bin`.
pub const VOICES_FILE: &str = "voices-v1.0.bin";

/// The config values the ladder reads, borrowed.
///
/// Deliberately not `heard_config::Config`: the core ladder needs one value
/// and one filesystem fact, and naming exactly those is what makes every
/// branch testable without a config file. The daemon fills this in from its
/// loaded config at the one call site.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigView<'a> {
    /// `cfg["elevenlabs_api_key"]` — the user's own EL key.
    pub elevenlabs_api_key: &'a str,
    /// Whether both Kokoro files are present at their pinned sizes. See
    /// [`kokoro_is_downloaded`].
    pub kokoro_downloaded: bool,
    /// The whole merged config, for registered backends to read their own
    /// keys from. `None` means "nothing beyond the fields above".
    pub config: Option<&'a Map<String, Value>>,
}

/// Which backend the ladder picks.
///
/// A decision, not an instance: `decide` never constructs a client, so the
/// tests assert on the choice rather than on whatever the constructor happened
/// to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The user's own ElevenLabs key.
    ElevenLabs,
    /// A registered backend, by its [`BackendFactory::name`].
    Registered(&'static str),
    /// The local Kokoro model, already downloaded.
    Kokoro,
    /// No voice configured.
    Null,
}

impl Backend {
    /// The backend's name, for logs.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Backend::ElevenLabs => "elevenlabs",
            Backend::Registered(name) => name,
            Backend::Kokoro => "kokoro",
            Backend::Null => "null",
        }
    }
}

/// How a registered backend ranks for one selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Not usable right now.
    No,
    /// Usable, and outranks a plain ElevenLabs key.
    Preferred,
    /// Usable, but only when there is no ElevenLabs key.
    Fallback,
}

/// A backend that lives outside this crate.
pub trait BackendFactory: Send + Sync {
    /// Its name: the [`Backend::Registered`] value and the log name. Unique.
    fn name(&self) -> &'static str;
    /// Where it ranks for this config. Called once per selection; must be
    /// cheap and must not touch the network.
    fn claim(&self, cfg: &ConfigView<'_>) -> Claim;
    /// Build the client. Called only after a claim won.
    fn build(&self, cfg: &ConfigView<'_>) -> Box<dyn Tts>;
}

/// An ordered set of registered backends.
#[derive(Clone, Default)]
pub struct BackendRegistry {
    factories: Vec<Arc<dyn BackendFactory>>,
}

impl std::fmt::Debug for BackendRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.factories.iter().map(|b| b.name()))
            .finish()
    }
}

impl BackendRegistry {
    /// No registered backends: the core ladder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `factory`, replacing any with the same name (in place, so its
    /// rank among the others is kept).
    pub fn register(&mut self, factory: Arc<dyn BackendFactory>) {
        if let Some(slot) = self
            .factories
            .iter_mut()
            .find(|f| f.name() == factory.name())
        {
            *slot = factory;
        } else {
            self.factories.push(factory);
        }
    }

    /// The registered names, in rank order.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.factories.iter().map(|f| f.name()).collect()
    }

    fn pick(&self, cfg: &ConfigView<'_>) -> (Backend, Option<&Arc<dyn BackendFactory>>) {
        let claims: Vec<(Claim, &Arc<dyn BackendFactory>)> =
            self.factories.iter().map(|f| (f.claim(cfg), f)).collect();
        // 1. A preferred registered backend.
        if let Some((_, f)) = claims.iter().find(|(c, _)| *c == Claim::Preferred) {
            return (Backend::Registered(f.name()), Some(*f));
        }
        // 2. The user's own key.
        if !cfg.elevenlabs_api_key.trim().is_empty() {
            return (Backend::ElevenLabs, None);
        }
        // 3. A fallback registered backend.
        if let Some((_, f)) = claims.iter().find(|(c, _)| *c == Claim::Fallback) {
            return (Backend::Registered(f.name()), Some(*f));
        }
        // 4. Local Kokoro — opt-in only; never auto-downloaded.
        if cfg.kokoro_downloaded {
            return (Backend::Kokoro, None);
        }
        // 5. Silence, plus the daemon's one-time "add a voice" nudge.
        (Backend::Null, None)
    }

    /// The ladder over this registry. Pure apart from the factories' own
    /// [`BackendFactory::claim`]s.
    #[must_use]
    pub fn decide(&self, cfg: &ConfigView<'_>) -> Backend {
        self.pick(cfg).0
    }

    /// Build the backend the ladder picked. See [`select_backend`].
    #[must_use]
    pub fn select(&self, cfg: &ConfigView<'_>) -> Box<dyn Tts> {
        match self.pick(cfg) {
            (Backend::Registered(_), Some(factory)) => factory.build(cfg),
            (Backend::ElevenLabs, _) => Box::new(ElevenLabsTts::new(cfg.elevenlabs_api_key)),
            _ => Box::new(NullTts),
        }
    }
}

fn global() -> &'static RwLock<BackendRegistry> {
    static GLOBAL: OnceLock<RwLock<BackendRegistry>> = OnceLock::new();
    GLOBAL.get_or_init(|| RwLock::new(BackendRegistry::new()))
}

/// Register a backend for the whole process ([`decide`] and
/// [`select_backend`] consult it). Call once at startup, before the first
/// selection.
pub fn register_backend(factory: Arc<dyn BackendFactory>) {
    global()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .register(factory);
}

/// A snapshot of the process-wide registry.
#[must_use]
pub fn registered_backends() -> BackendRegistry {
    global().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The ladder over the process-wide registry. Pure when nothing is
/// registered: no I/O, no clock, no network.
#[must_use]
pub fn decide(cfg: &ConfigView<'_>) -> Backend {
    registered_backends().decide(cfg)
}

/// Both Kokoro files present at exactly their pinned sizes.
///
/// `KokoroTTS.is_downloaded` / `_has_full`. Any `stat` failure is `false`, as
/// the Python's `except OSError: return False`.
#[must_use]
pub fn kokoro_is_downloaded(models_dir: &Path) -> bool {
    let full = |name: &str, want: u64| {
        std::fs::metadata(models_dir.join(name))
            .map(|m| m.is_file() && m.len() == want)
            .unwrap_or(false)
    };
    full(MODEL_FILE, MODEL_SIZE) && full(VOICES_FILE, VOICES_SIZE)
}

/// Build the backend the ladder picked, over the process-wide registry.
///
/// Kokoro is behind this crate's `kokoro` feature, because `ort` downloads an
/// ONNX Runtime binary at build time and the cloud path must not pay for that.
/// A Kokoro decision falls through to [`NullTts`] here — the composition root
/// builds Kokoro itself, with its model paths — which is the same
/// silence-plus-nudge the user would get with no model, the safest outcome
/// to land on by accident. [`decide`] still reports [`Backend::Kokoro`]
/// either way, so a log or a test sees the truth.
#[must_use]
pub fn select_backend(cfg: &ConfigView<'_>) -> Box<dyn Tts> {
    registered_backends().select(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Audio, TtsError};

    fn cfg<'a>() -> ConfigView<'a> {
        ConfigView::default()
    }

    // ---- branch 5: nothing configured -----------------------------------

    #[test]
    fn nothing_configured_is_null() {
        assert_eq!(decide(&cfg()), Backend::Null);
    }

    #[test]
    fn whitespace_only_credentials_count_as_absent() {
        let c = ConfigView {
            elevenlabs_api_key: "   ",
            ..cfg()
        };
        assert_eq!(decide(&c), Backend::Null);
    }

    // ---- branch 2: the user's own key ------------------------------------

    #[test]
    fn a_key_with_no_token_is_byok() {
        let c = ConfigView {
            elevenlabs_api_key: "sk_test_fake",
            ..cfg()
        };
        assert_eq!(decide(&c), Backend::ElevenLabs);
    }

    // ---- branch 4: Kokoro ------------------------------------------------

    #[test]
    fn a_downloaded_model_beats_null_but_nothing_else() {
        let c = ConfigView {
            kokoro_downloaded: true,
            ..cfg()
        };
        assert_eq!(decide(&c), Backend::Kokoro);

        // …and loses to the user's own key.
        assert_eq!(
            decide(&ConfigView {
                elevenlabs_api_key: "sk_test_fake",
                kokoro_downloaded: true,
                ..cfg()
            }),
            Backend::ElevenLabs
        );
    }

    // ---- is_downloaded ---------------------------------------------------

    #[test]
    fn a_missing_models_dir_is_not_downloaded() {
        assert!(!kokoro_is_downloaded(Path::new(
            "/nonexistent/heard/models/dir"
        )));
    }

    #[test]
    fn a_truncated_model_is_not_downloaded() {
        let dir = std::env::temp_dir().join(format!("heard-tts-sel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MODEL_FILE), b"not 325 MB").unwrap();
        std::fs::write(dir.join(VOICES_FILE), b"not 28 MB").unwrap();
        assert!(!kokoro_is_downloaded(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_named_like_the_model_is_not_a_file() {
        let dir = std::env::temp_dir().join(format!("heard-tts-seld-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(MODEL_FILE)).unwrap();
        assert!(!kokoro_is_downloaded(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- select_backend --------------------------------------------------

    #[test]
    fn select_builds_a_configured_client_for_the_key() {
        let el = select_backend(&ConfigView {
            elevenlabs_api_key: "sk_test_fake",
            ..cfg()
        });
        assert!(el.is_configured());
        assert!((el.max_native_speed() - 1.2).abs() < f64::EPSILON);
    }

    #[test]
    fn select_builds_an_unconfigured_null_for_the_empty_config() {
        let t = select_backend(&cfg());
        assert!(!t.is_configured());
        assert!(t.synth("hi", "", 1.0, "en-us").is_err());
    }

    #[test]
    fn names_are_stable_for_logs() {
        assert_eq!(Backend::ElevenLabs.name(), "elevenlabs");
        assert_eq!(Backend::Registered("cloud").name(), "cloud");
        assert_eq!(Backend::Kokoro.name(), "kokoro");
        assert_eq!(Backend::Null.name(), "null");
    }

    // ---- the registry ----------------------------------------------------

    /// A registered backend that reads its own keys out of the merged
    /// config: `fake_token` set → usable; `fake_prefer_key` → it yields to
    /// the user's key.
    struct FakeCloud;

    struct FakeCloudTts;

    impl Tts for FakeCloudTts {
        fn audio_ext(&self) -> &'static str {
            ".mp3"
        }
        fn max_native_speed(&self) -> f64 {
            1.0
        }
        fn synth(&self, _: &str, _: &str, _: f64, _: &str) -> Result<Audio, TtsError> {
            Err(TtsError::Backend {
                name: "fake_cloud",
                source: "offline".into(),
            })
        }
    }

    impl BackendFactory for FakeCloud {
        fn name(&self) -> &'static str {
            "fake_cloud"
        }
        fn claim(&self, cfg: &ConfigView<'_>) -> Claim {
            let Some(c) = cfg.config else {
                return Claim::No;
            };
            let has = |k: &str| {
                c.get(k)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty())
            };
            if !has("fake_token") {
                Claim::No
            } else if c.get("fake_prefer_key") == Some(&Value::Bool(true)) {
                Claim::Fallback
            } else {
                Claim::Preferred
            }
        }
        fn build(&self, _cfg: &ConfigView<'_>) -> Box<dyn Tts> {
            Box::new(FakeCloudTts)
        }
    }

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn an_empty_registry_is_the_core_ladder() {
        let r = BackendRegistry::new();
        assert_eq!(r.decide(&cfg()), Backend::Null);
        assert!(r.names().is_empty());
    }

    #[test]
    fn a_preferred_backend_outranks_the_key_and_a_fallback_does_not() {
        let mut r = BackendRegistry::new();
        r.register(Arc::new(FakeCloud));
        let token = map(serde_json::json!({"fake_token": "t"}));
        let yield_to_key = map(serde_json::json!({"fake_token": "t", "fake_prefer_key": true}));
        let nothing = map(serde_json::json!({}));

        fn with<'a>(c: &'a Map<String, Value>, key: &'a str) -> ConfigView<'a> {
            ConfigView {
                elevenlabs_api_key: key,
                kokoro_downloaded: true,
                config: Some(c),
            }
        }
        // Preferred: beats the key, and Kokoro.
        assert_eq!(
            r.decide(&with(&token, "sk_test_fake")),
            Backend::Registered("fake_cloud")
        );
        // Fallback: the key wins; without a key the backend beats Kokoro.
        assert_eq!(
            r.decide(&with(&yield_to_key, "sk_test_fake")),
            Backend::ElevenLabs
        );
        assert_eq!(
            r.decide(&with(&yield_to_key, "")),
            Backend::Registered("fake_cloud")
        );
        // No claim: the core ladder.
        assert_eq!(r.decide(&with(&nothing, "")), Backend::Kokoro);
        assert_eq!(
            r.decide(&with(&nothing, "sk_test_fake")),
            Backend::ElevenLabs
        );
        // And the pick builds the registered client.
        let t = r.select(&with(&token, ""));
        assert_eq!(t.audio_ext(), ".mp3");
        match t.synth("hi", "", 1.0, "en-us") {
            Err(TtsError::Backend { name, .. }) => assert_eq!(name, "fake_cloud"),
            other => panic!("expected the registered backend's error, got {other:?}"),
        }
    }

    #[test]
    fn registering_a_name_twice_replaces_it_in_place() {
        struct Named(&'static str);
        impl BackendFactory for Named {
            fn name(&self) -> &'static str {
                self.0
            }
            fn claim(&self, _: &ConfigView<'_>) -> Claim {
                Claim::Preferred
            }
            fn build(&self, _: &ConfigView<'_>) -> Box<dyn Tts> {
                Box::new(NullTts)
            }
        }
        let mut r = BackendRegistry::new();
        r.register(Arc::new(Named("a")));
        r.register(Arc::new(Named("b")));
        r.register(Arc::new(Named("a")));
        assert_eq!(r.names(), vec!["a", "b"]);
        // First registered preferred wins.
        assert_eq!(r.decide(&cfg()), Backend::Registered("a"));
    }
}
