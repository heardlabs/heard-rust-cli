//! [`LazyKokoro`] — Kokoro loaded on first use, so a daemon's socket opens
//! before the 325 MB graph is read — and [`KokoroFactory`], the local voice
//! as a registered ladder rung for a composition root that wants Kokoro to
//! be a real backend (not the [`crate::NullTts`] fall-through
//! [`crate::select_backend`] gives without one).
//!
//! The factory claims [`Claim::Fallback`] exactly when the model files are
//! present, which is the built-in rung's position: after a `Preferred`
//! registered backend and the user's own key, and — registered after any
//! other `Fallback` backend — after those too.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::select::{BackendFactory, Claim, ConfigView};
use crate::{Audio, Tts, TtsError};

use super::KokoroTts;

/// Kokoro, loaded on first use (or ahead of it with [`LazyKokoro::warm`]).
#[derive(Debug)]
pub struct LazyKokoro {
    dir: PathBuf,
    cell: OnceLock<Result<KokoroTts, String>>,
}

impl LazyKokoro {
    /// Over the model files in `models_dir`.
    #[must_use]
    pub fn new(models_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: models_dir.into(),
            cell: OnceLock::new(),
        }
    }

    /// The loaded backend, or why it could not load.
    pub fn get(&self) -> &Result<KokoroTts, String> {
        self.cell
            .get_or_init(|| KokoroTts::new(&self.dir).map_err(|e| e.to_string()))
    }

    /// Start loading now, on a thread of its own.
    pub fn warm(self: &Arc<Self>) {
        let me = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("heard-kokoro-load".into())
            .spawn(move || {
                let _ = me.get();
            });
    }
}

impl Tts for LazyKokoro {
    fn audio_ext(&self) -> &'static str {
        ".wav"
    }
    fn max_native_speed(&self) -> f64 {
        4.0
    }
    fn list_voices(&self) -> Vec<String> {
        self.get()
            .as_ref()
            .map(Tts::list_voices)
            .unwrap_or_default()
    }
    fn synth(&self, text: &str, voice: &str, speed: f64, lang: &str) -> Result<Audio, TtsError> {
        match self.get() {
            Ok(k) => k.synth(text, voice, speed, lang),
            Err(e) => Err(TtsError::Backend {
                name: "kokoro",
                source: e.clone().into(),
            }),
        }
    }
}

/// The local voice as a registered rung (`name() == "kokoro"`).
#[derive(Debug, Clone)]
pub struct KokoroFactory {
    models_dir: PathBuf,
}

impl KokoroFactory {
    /// Over `models_dir`.
    #[must_use]
    pub fn new(models_dir: impl Into<PathBuf>) -> Self {
        Self {
            models_dir: models_dir.into(),
        }
    }
}

impl BackendFactory for KokoroFactory {
    fn name(&self) -> &'static str {
        "kokoro"
    }
    fn claim(&self, cfg: &ConfigView<'_>) -> Claim {
        if cfg.kokoro_downloaded {
            Claim::Fallback
        } else {
            Claim::No
        }
    }
    fn build(&self, _cfg: &ConfigView<'_>) -> Box<dyn Tts> {
        Box::new(LazyKokoro::new(self.models_dir.clone()))
    }
}
