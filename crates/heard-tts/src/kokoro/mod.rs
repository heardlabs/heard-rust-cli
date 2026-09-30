//! Kokoro ONNX TTS backend — ported from `engine/heard/tts/kokoro.py` and the
//! `kokoro-onnx` package it drives.
//!
//! **Behind the `kokoro` feature.** `ort` downloads a prebuilt ONNX Runtime at
//! build time (see the crate README), and
//! the cloud path must not pay for that — neither in build time nor in supply
//! chain. Nothing here compiles unless the feature is on.
//!
//! # What `kokoro-onnx` does, and what this reproduces
//!
//! The Python's `KokoroTTS.synth_to_file` is one line into `kokoro_onnx`, so
//! the real port target is that package's `create`:
//!
//! 1. phonemise the text — `kokoro-onnx` uses espeak via `phonemizer`; this
//!    port uses misaki in process by default ([`g2p::MisakiG2p`], Apache-2.0),
//!    with the espeak subprocess available behind the opt-in `espeak`
//!    feature. See [`g2p`] for the licence reasoning;
//! 2. drop every phoneme outside the 114-entry vocabulary and map the rest to
//!    token ids ([`vocab`]);
//! 3. pick the voice's style vector for that token count — row `n - 1` of a
//!    `[510, 1, 256]` array in the `.npz` ([`voices_npz`]);
//! 4. run the graph on `[[0, ...tokens, 0]]`, the style and the speed;
//! 5. the first output, flattened, is `f32` PCM at **24 000 Hz**.
//!
//! Step 4's `0` padding either side is not decoration — it is what the graph
//! was exported expecting, and omitting it changes the output.
//!
//! # Deliberate divergences
//!
//! * **No downloading.** `kokoro.py` has `ensure_downloaded` with a retrying
//!   streaming fetch. This crate only ever *reads* model files, and
//!   [`KokoroTts::new`] fails with a sentence if they are absent. The download
//!   is opt-in through the app's "Options → Download voice", and a library
//!   that can quietly pull 325 MB is how that became a bug in the first place.
//! * **Long text is batched, as `kokoro-onnx` 0.6.1 does.** [`Tts::synth`]
//!   phonemises the whole text, splits it into ≤510-phoneme batches at
//!   sentence, then clause, then word boundaries ([`crate::chunk`]),
//!   synthesises and trims each batch, follows every batch but the last with
//!   the pause its final mark calls for (0.25 s after a sentence, 0.1 s after
//!   a clause), and concatenates. [`KokoroTts::synth_pcm`] stays the raw
//!   single-window pass the spike compares sample for sample.

pub mod g2p;
pub mod lazy;
pub mod vocab;
pub mod voices_npz;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ndarray::{Array1, Array2};
use ort::session::Session;
use ort::value::Value;

#[cfg(feature = "espeak")]
pub use g2p::EspeakCliG2p;
pub use g2p::{G2p, MisakiG2p, PhonemesAsGiven};
pub use lazy::{KokoroFactory, LazyKokoro};
pub use voices_npz::Voices;

use crate::{Audio, Pcm, Tts, TtsError};

/// Kokoro's output rate. `SAMPLE_RATE` in `kokoro_onnx/config.py`.
pub const SAMPLE_RATE: u32 = 24_000;

/// The speed range `kokoro-onnx` accepts before it raises.
pub const MIN_SPEED: f64 = 0.5;
/// See [`MIN_SPEED`].
pub const MAX_SPEED: f64 = 2.0;

/// Everything the local voice can fail at, as a value.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum KokoroError {
    /// A model file is missing or the wrong size.
    #[error("{0}")]
    NotDownloaded(String),
    /// The `.npz` of voices could not be read.
    #[error("Kokoro voices: {0}")]
    Voices(String),
    /// The requested voice is not in the file.
    #[error("unknown Kokoro voice {voice:?}; did you mean one of: {near}")]
    UnknownVoice {
        /// What was asked for.
        voice: String,
        /// Some names that start the same way.
        near: String,
    },
    /// Phonemisation failed (only the opt-in espeak subprocess can fail).
    #[error("{0}")]
    G2p(String),
    /// The text phonemised to nothing the model knows.
    #[error("no phonemes of {0:?} are in the model vocabulary")]
    NoPhonemes(String),
    /// Longer than the graph's 510-phoneme window.
    #[error("text is too long: {got} phonemes, the model takes {max}")]
    TooLong {
        /// Phoneme count after filtering.
        got: usize,
        /// [`vocab::MAX_PHONEME_LENGTH`].
        max: usize,
    },
    /// Speed outside `[0.5, 2.0]`.
    #[error("speed should be between {MIN_SPEED} and {MAX_SPEED}, got {0}")]
    BadSpeed(f64),
    /// ONNX Runtime said no.
    #[error("Kokoro inference: {0}")]
    Onnx(String),
}

/// The local Kokoro voice.
///
/// The session is built on first use and then held, because loading a 325 MB
/// graph takes seconds and the Python is lazy for the same reason ("keep heavy
/// deps out of module load"). The [`Mutex`] is not for speed — ONNX Runtime
/// sessions are internally thread-safe — but because `Session::run` needs
/// `&mut self` in `ort` 2.0-rc.
pub struct KokoroTts {
    model_path: PathBuf,
    voices_path: PathBuf,
    g2p: Box<dyn G2p>,
    state: Mutex<Option<Loaded>>,
}

struct Loaded {
    session: Session,
    voices: Voices,
}

impl std::fmt::Debug for KokoroTts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KokoroTts")
            .field("model_path", &self.model_path)
            .field("voices_path", &self.voices_path)
            .field(
                "loaded",
                &self.state.lock().map(|s| s.is_some()).unwrap_or(false),
            )
            .finish()
    }
}

impl KokoroTts {
    /// A backend over the model files in `models_dir`, phonemising with
    /// misaki in process ([`MisakiG2p`]).
    ///
    /// Checks the files are present and the right size now rather than at
    /// first utterance — the same `is_downloaded` gate the selector uses, so
    /// the failure lands where the user can still pick another voice.
    ///
    /// # Errors
    /// [`KokoroError::NotDownloaded`] if either file is missing or truncated.
    pub fn new(models_dir: &Path) -> Result<Self, KokoroError> {
        Self::with_g2p(models_dir, Box::new(MisakiG2p::new()))
    }

    /// As [`KokoroTts::new`], with a phonemiser of your choosing. The spike
    /// uses [`PhonemesAsGiven`] to test the graph without G2P in the way.
    ///
    /// # Errors
    /// [`KokoroError::NotDownloaded`] if either file is missing or truncated.
    pub fn with_g2p(models_dir: &Path, g2p: Box<dyn G2p>) -> Result<Self, KokoroError> {
        let model_path = models_dir.join(crate::select::MODEL_FILE);
        let voices_path = models_dir.join(crate::select::VOICES_FILE);

        if !crate::select::kokoro_is_downloaded(models_dir) {
            return Err(KokoroError::NotDownloaded(format!(
                "the local voice is not downloaded — expected {} ({} bytes) and {} ({} bytes) \
                 in {}; use Options → Download voice",
                crate::select::MODEL_FILE,
                crate::select::MODEL_SIZE,
                crate::select::VOICES_FILE,
                crate::select::VOICES_SIZE,
                models_dir.display(),
            )));
        }

        Ok(Self {
            model_path,
            voices_path,
            g2p,
            state: Mutex::new(None),
        })
    }

    /// Load the graph and the voices if they are not loaded, then run `f`.
    fn with_loaded<T>(
        &self,
        f: impl FnOnce(&mut Session, &Voices) -> Result<T, KokoroError>,
    ) -> Result<T, KokoroError> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| KokoroError::Onnx("the Kokoro session lock was poisoned".into()))?;

        if guard.is_none() {
            let session = Session::builder()
                .map_err(|e| KokoroError::Onnx(format!("building the session: {e}")))?
                .commit_from_file(&self.model_path)
                .map_err(|e| {
                    KokoroError::Onnx(format!("loading {}: {e}", self.model_path.display()))
                })?;
            let voices = Voices::load(&self.voices_path)?;
            *guard = Some(Loaded { session, voices });
        }

        let loaded = guard.as_mut().expect("just filled");
        f(&mut loaded.session, &loaded.voices)
    }

    /// Voice names in the `.npz`, sorted. `KokoroTTS.list_voices`.
    ///
    /// # Errors
    /// Anything loading the model or voices file.
    pub fn voice_names(&self) -> Result<Vec<String>, KokoroError> {
        self.with_loaded(|_, voices| Ok(voices.names()))
    }

    /// Phonemise, tokenise, and synthesise — the whole of `create` for one
    /// window.
    ///
    /// # Errors
    /// See [`KokoroError`].
    pub fn synth_pcm(&self, text: &str, voice: &str, speed: f64) -> Result<Pcm, KokoroError> {
        if !(MIN_SPEED..=MAX_SPEED).contains(&speed) {
            return Err(KokoroError::BadSpeed(speed));
        }
        let phonemes = self.g2p.phonemize(text, "en-us")?;
        self.synth_phonemes(&phonemes, voice, speed, text)
    }

    /// Synthesise from phonemes that are already in Kokoro's alphabet.
    ///
    /// Exposed because it is what makes the spike honest: feeding the graph
    /// phonemes captured from the Python proves the ONNX side on its own,
    /// with no G2P in the comparison.
    ///
    /// # Errors
    /// See [`KokoroError`].
    pub fn synth_phonemes(
        &self,
        phonemes: &str,
        voice: &str,
        speed: f64,
        source_text: &str,
    ) -> Result<Pcm, KokoroError> {
        if !(MIN_SPEED..=MAX_SPEED).contains(&speed) {
            return Err(KokoroError::BadSpeed(speed));
        }

        let kept = vocab::known(phonemes);
        let tokens = vocab::tokenize(&kept).ok_or(KokoroError::TooLong {
            got: kept.chars().count(),
            max: vocab::MAX_PHONEME_LENGTH,
        })?;
        if tokens.is_empty() {
            return Err(KokoroError::NoPhonemes(source_text.to_string()));
        }

        self.with_loaded(|session, voices| {
            let style = voices.style_for(voice, tokens.len())?;
            run(session, &tokens, style, speed)
        })
    }
}

/// One forward pass. `Kokoro._infer`.
fn run(
    session: &mut Session,
    tokens: &[i64],
    style: &[f32],
    speed: f64,
) -> Result<Pcm, KokoroError> {
    // `[[0, *tokens, 0]]` — the graph was exported expecting the padding.
    let mut padded = Vec::with_capacity(tokens.len() + 2);
    padded.push(0i64);
    padded.extend_from_slice(tokens);
    padded.push(0i64);

    let ids = Array2::from_shape_vec((1, padded.len()), padded)
        .map_err(|e| KokoroError::Onnx(format!("shaping input_ids: {e}")))?;
    let style_arr = Array2::from_shape_vec((1, voices_npz::STYLE_DIM), style.to_vec())
        .map_err(|e| KokoroError::Onnx(format!("shaping style: {e}")))?;
    #[allow(clippy::cast_possible_truncation)]
    let speed_arr = Array1::from_vec(vec![speed as f32]);

    // Older exports name the token input "tokens", newer ones "input_ids" —
    // the Python sniffs the graph for this and so does the port, because the
    // v1.0 file people already have on disk could be either.
    let token_input = session
        .inputs()
        .iter()
        .map(|i| i.name().to_string())
        .find(|n| n == "input_ids" || n == "tokens")
        .ok_or_else(|| {
            KokoroError::Onnx(format!(
                "the graph has no 'input_ids' or 'tokens' input; it has: {}",
                session
                    .inputs()
                    .iter()
                    .map(|i| i.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;

    let ids_v =
        Value::from_array(ids).map_err(|e| KokoroError::Onnx(format!("input_ids tensor: {e}")))?;
    let style_v = Value::from_array(style_arr)
        .map_err(|e| KokoroError::Onnx(format!("style tensor: {e}")))?;
    let speed_v = Value::from_array(speed_arr)
        .map_err(|e| KokoroError::Onnx(format!("speed tensor: {e}")))?;

    let outputs = session
        .run(ort::inputs![
            token_input.as_str() => ids_v,
            "style" => style_v,
            "speed" => speed_v,
        ])
        .map_err(|e| KokoroError::Onnx(format!("running the graph: {e}")))?;

    let first = outputs
        .iter()
        .next()
        .ok_or_else(|| KokoroError::Onnx("the graph returned no outputs".into()))?
        .1;
    let (_shape, data) = first
        .try_extract_tensor::<f32>()
        .map_err(|e| KokoroError::Onnx(format!("reading the audio output: {e}")))?;

    Ok(Pcm {
        samples: data.to_vec(),
        sample_rate: SAMPLE_RATE,
    })
}

pub use crate::chunk::split_phonemes;

impl KokoroTts {
    /// `create(text, voice, speed)` with its defaults (`trim=True`,
    /// `sentence_pause=0.25`, `clause_pause=0.1`): phonemise, normalise the
    /// whitespace, batch at ≤510 phonemes, synthesise + trim each batch, pad
    /// the pause after it, concatenate.
    ///
    /// # Errors
    /// See [`KokoroError`]; a text that phonemises to nothing is
    /// [`KokoroError::NoPhonemes`].
    pub fn synth_chunked(&self, text: &str, voice: &str, speed: f64) -> Result<Pcm, KokoroError> {
        if !(MIN_SPEED..=MAX_SPEED).contains(&speed) {
            return Err(KokoroError::BadSpeed(speed));
        }
        let phonemes = self.g2p.phonemize(text, "en-us")?;
        let batches = crate::chunk::batches(&phonemes);
        if batches.is_empty() {
            return Err(KokoroError::NoPhonemes(text.to_string()));
        }
        let mut samples = Vec::new();
        for (batch, pause) in batches {
            let mut pcm = self.synth_phonemes(&batch, voice, speed, &batch)?;
            let (start, end) = crate::trim::trim_interval(&pcm.samples);
            pcm.samples.truncate(end);
            pcm.samples.drain(..start);
            samples.extend_from_slice(&pcm.samples);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let silence = (pause * f64::from(SAMPLE_RATE)) as usize;
            samples.extend(std::iter::repeat_n(0.0f32, silence));
        }
        Ok(Pcm {
            samples,
            sample_rate: SAMPLE_RATE,
        })
    }
}

impl Tts for KokoroTts {
    /// `.wav` — Kokoro writes WAV via `soundfile`; `afplay` handles it
    /// natively, so nothing is ever re-encoded.
    fn audio_ext(&self) -> &'static str {
        ".wav"
    }

    /// `4.0` — the Python's `MAX_NATIVE_SPEED`. Kokoro resamples its own
    /// output, so the daemon never layers `afplay -r` on this backend.
    ///
    /// Note this is the *daemon's* threshold and is deliberately wider than
    /// `kokoro-onnx`'s own `[0.5, 2.0]` guard, which [`KokoroTts::synth_pcm`]
    /// enforces. The Python has the same gap; a speed between 2.0 and 4.0 is
    /// an error there too, and porting it faithfully means keeping both
    /// numbers rather than quietly reconciling them.
    fn max_native_speed(&self) -> f64 {
        4.0
    }

    fn list_voices(&self) -> Vec<String> {
        self.voice_names().unwrap_or_default()
    }

    fn synth(&self, text: &str, voice: &str, speed: f64, _lang: &str) -> Result<Audio, TtsError> {
        // `Kokoro.create(..., trim=True)` is the Python default and what
        // `kokoro.py` calls: batched at 510 phonemes, each batch trimmed,
        // the punctuation's pause between batches. `synth_pcm` /
        // `synth_phonemes` stay raw: they are the graph's output, which is
        // what the spike's sample-for-sample test compares.
        Ok(Audio::Pcm(self.synth_chunked(text, voice, speed)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_models_dir_is_a_sentence_naming_the_way_out() {
        let err = KokoroTts::new(Path::new("/nonexistent/models")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not downloaded"), "{msg}");
        assert!(msg.contains("Options → Download voice"), "{msg}");
        assert!(msg.contains("kokoro-v1.0.onnx"), "{msg}");
    }

    #[test]
    fn every_batch_fits_the_graph() {
        let long = "ðə bˈɪld ɪz ɡɹˈin, ".repeat(80);
        for part in split_phonemes(&long) {
            assert!(part.chars().count() <= vocab::MAX_PHONEME_LENGTH);
        }
        assert_eq!(crate::chunk::MAX_PHONEME_LENGTH, vocab::MAX_PHONEME_LENGTH);
    }

    #[test]
    fn speed_outside_the_kokoro_range_is_rejected_before_any_model_load() {
        // Path does not exist, so reaching a load would error differently —
        // BadSpeed proves the guard runs first.
        let tts = KokoroTts {
            model_path: PathBuf::from("/nonexistent"),
            voices_path: PathBuf::from("/nonexistent"),
            g2p: Box::new(PhonemesAsGiven),
            state: Mutex::new(None),
        };
        assert!(matches!(
            tts.synth_pcm("hi", "af_heart", 0.4),
            Err(KokoroError::BadSpeed(_))
        ));
        assert!(matches!(
            tts.synth_pcm("hi", "af_heart", 2.1),
            Err(KokoroError::BadSpeed(_))
        ));
    }

    #[test]
    fn text_with_no_known_phonemes_is_a_sentence_not_an_empty_wav() {
        let tts = KokoroTts {
            model_path: PathBuf::from("/nonexistent"),
            voices_path: PathBuf::from("/nonexistent"),
            g2p: Box::new(PhonemesAsGiven),
            state: Mutex::new(None),
        };
        let err = tts
            .synth_phonemes("###", "af_heart", 1.0, "###")
            .unwrap_err();
        assert!(matches!(err, KokoroError::NoPhonemes(_)));
        assert!(err.to_string().contains("vocabulary"));
    }
}
