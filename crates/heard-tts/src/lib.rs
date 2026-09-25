//! The TTS backends, behind one trait.
//!
//! Ported from `engine/heard/tts/`: [`null`] ← `null.py`, [`elevenlabs`] ←
//! `elevenlabs.py`, [`kokoro`] ← `kokoro.py`, and [`select`] ←
//! `Daemon._make_tts` in `engine/heard/daemon.py`. [`select`] also holds a
//! registry, so a crate outside this one can add a backend by name.
//!
//! # What a backend is
//!
//! The Python surface is `synth_to_file(text, voice, speed, lang, path)` plus
//! two class attributes, `AUDIO_EXT` and `MAX_NATIVE_SPEED`. The daemon leans
//! on all three: it mints a tempfile with the backend's native extension so
//! nothing is ever re-encoded, and it compares the requested speed against
//! `MAX_NATIVE_SPEED` to decide whether to layer `afplay -r` on top.
//!
//! [`Tts`] keeps all three, but returns the audio instead of writing it:
//! `synth(text, voice, speed, lang) -> Result<Audio, TtsError>`. Writing is
//! then [`Audio::write`], which is a method on the value rather than a
//! responsibility of every backend. Two reasons. A returned value is testable
//! without a filesystem — every HTTP test in this crate asserts on bytes, not
//! on a temp directory. And it is what the eventual speech queue wants: the
//! Python already reads the file straight back for playback, so the file was
//! never the point.
//!
//! # Why [`Audio`] is an enum
//!
//! It is tempting to describe `Audio` as PCM samples plus a sample
//! rate. That is true of exactly one backend. ElevenLabs returns
//! **MP3** (`AUDIO_EXT = ".mp3"`, `Accept: audio/mpeg`), and the Python never
//! decodes it — it writes the bytes to disk and hands the path to `afplay`.
//! Forcing those into PCM would mean shipping an MP3 decoder this crate does
//! not need and the Python does not have, and it would make the Rust port
//! do measurably more work than the thing it is replacing.
//!
//! So [`Audio`] is either [`Audio::Pcm`] (Kokoro: `f32` samples at 24 kHz,
//! which is what `soundfile.write` receives on the Python side) or
//! [`Audio::Encoded`] (a container the backend chose, carried verbatim).
//! [`Audio::write`] writes the native bytes for both — the port-faithful
//! operation. `write_wav` lives on [`Pcm`], where a WAV is actually
//! constructible, and [`Audio::write_wav`] is a convenience that fails on
//! encoded audio with a sentence rather than a panic.
//!
//! # Errors
//!
//! "A failure is a sentence, never a crash." Every backend error is a value:
//! [`TtsError`] is the crate-wide sum, and each backend keeps its own
//! `thiserror` enum ([`elevenlabs::ElevenLabsError`],
//! [`null::NullTtsError`], [`kokoro::KokoroError`]) so a caller that cares
//! about one backend's failure modes can match on them without
//! stringly-typed parsing. A registered backend's error travels as
//! [`TtsError::Backend`], and a caller that knows the backend downcasts it.
//!
//! # Features and licences
//!
//! | feature | adds | licence of what it adds |
//! |---|---|---|
//! | *(default)* | null, ElevenLabs | Apache/MIT crates only |
//! | `kokoro` | the local voice: `ort` (ONNX Runtime), and **misaki G2P** via `sayd-misaki-en` with its embedded lexicons | `sayd-misaki-en` Apache-2.0; lexicons Apache-2.0 (verbatim misaki 0.9.4) |
//! | `espeak` | opt-in, **off by default**: an espeak-ng *subprocess* phonemiser for side-by-side comparison with the Python | no new crate; espeak-ng itself is GPL-3.0 and is neither linked nor shipped |
//!
//! Neither the default build nor the `kokoro` build compiles, links or
//! invokes espeak-ng; `tests/no_espeak_by_default.rs` asserts it on the
//! resolved dependency graph. See `kokoro::g2p` (with the `kokoro`
//! feature) and `THIRD-PARTY-NOTICES.md`.

#![deny(unsafe_code)]
#![warn(missing_docs)]

use std::fs;
use std::io;
use std::path::Path;

pub mod chunk;
pub mod download;
pub mod elevenlabs;
#[cfg(feature = "kokoro")]
pub mod kokoro;
pub mod null;
pub mod select;
pub mod trim;
pub mod voices;

pub use elevenlabs::{ElevenLabsError, ElevenLabsTts, LibraryVoice};
pub use null::{NullTts, NullTtsError};
pub use select::{
    register_backend, registered_backends, select_backend, Backend, BackendFactory,
    BackendRegistry, Claim, ConfigView,
};

/// PCM audio: interleaved `f32` samples in `[-1.0, 1.0]`, plus the rate they
/// were produced at.
///
/// Mono in practice — Kokoro's graph emits a single channel, and the Python
/// hands `soundfile.write` a 1-D array — so [`Pcm::write_wav`] writes one
/// channel. The field is public because a test that wants to assert on
/// amplitude should not have to go through an accessor.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    /// The samples, mono.
    pub samples: Vec<f32>,
    /// Samples per second (24 000 for Kokoro).
    pub sample_rate: u32,
}

impl Pcm {
    /// How long this audio plays for, in seconds.
    #[must_use]
    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        self.samples.len() as f64 / f64::from(self.sample_rate)
    }

    /// Encode as a 16-bit mono PCM WAV.
    ///
    /// 16-bit rather than 32-bit float because that is what every consumer in
    /// the chain reads without complaint (`afplay`, QuickTime, `soxi`), and
    /// because the Python's `soundfile.write` to a `.wav` path also defaults
    /// to `PCM_16`. Samples are clamped before scaling, so a model that
    /// overshoots `1.0` clips rather than wrapping into noise.
    #[must_use]
    pub fn to_wav_bytes(&self) -> Vec<u8> {
        let bits_per_sample: u16 = 16;
        let channels: u16 = 1;
        let byte_rate = self.sample_rate * u32::from(channels) * u32::from(bits_per_sample / 8);
        let block_align = channels * (bits_per_sample / 8);
        let data_len = (self.samples.len() * 2) as u32;

        let mut out = Vec::with_capacity(44 + self.samples.len() * 2);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
        out.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&bits_per_sample.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for &s in &self.samples {
            let clamped = s.clamp(-1.0, 1.0);
            #[allow(clippy::cast_possible_truncation)]
            let v = (clamped * f32::from(i16::MAX)) as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    /// Write a 16-bit mono WAV to `path`, creating parent directories.
    ///
    /// # Errors
    /// Any I/O failure creating the parent directory or writing the file.
    pub fn write_wav(&self, path: &Path) -> Result<(), io::Error> {
        write_all_with_parents(path, &self.to_wav_bytes())
    }
}

/// Audio a backend produced in a container it chose, carried verbatim.
///
/// The Python writes these bytes straight to disk for `afplay` — "no decoding,
/// no in-process audio buffer", as `elevenlabs.py` puts it. This crate does
/// the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    /// The container bytes exactly as the backend returned them.
    pub bytes: Vec<u8>,
    /// The file extension this container wants, including the dot (`".mp3"`).
    pub ext: &'static str,
}

/// What a backend hands back.
#[derive(Debug, Clone, PartialEq)]
pub enum Audio {
    /// Decoded samples — Kokoro.
    Pcm(Pcm),
    /// An encoded container — ElevenLabs' MP3.
    Encoded(Encoded),
}

impl Audio {
    /// The extension the native bytes want, including the dot.
    #[must_use]
    pub fn ext(&self) -> &'static str {
        match self {
            Audio::Pcm(_) => ".wav",
            Audio::Encoded(e) => e.ext,
        }
    }

    /// The samples, if this is PCM.
    #[must_use]
    pub fn as_pcm(&self) -> Option<&Pcm> {
        match self {
            Audio::Pcm(p) => Some(p),
            Audio::Encoded(_) => None,
        }
    }

    /// How many bytes writing this natively would produce.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Audio::Pcm(p) => 44 + p.samples.len() * 2,
            Audio::Encoded(e) => e.bytes.len(),
        }
    }

    /// Write the audio in its native container, creating parent directories.
    ///
    /// This is the port of `synth_to_file`'s tail: PCM becomes a WAV (what
    /// `soundfile.write` does for Kokoro), encoded audio is written verbatim
    /// (what `out_path.write_bytes(audio)` does for the two HTTP backends).
    ///
    /// # Errors
    /// Any I/O failure creating the parent directory or writing the file.
    pub fn write(&self, path: &Path) -> Result<(), io::Error> {
        match self {
            Audio::Pcm(p) => p.write_wav(path),
            Audio::Encoded(e) => write_all_with_parents(path, &e.bytes),
        }
    }

    /// Write a WAV, if this audio is PCM.
    ///
    /// # Errors
    /// [`TtsError::NotPcm`] when the audio is an encoded container this crate
    /// deliberately does not decode; otherwise any I/O failure.
    pub fn write_wav(&self, path: &Path) -> Result<(), TtsError> {
        match self {
            Audio::Pcm(p) => p.write_wav(path).map_err(TtsError::from),
            Audio::Encoded(e) => Err(TtsError::NotPcm(e.ext)),
        }
    }
}

/// Read an HTTP response body, capped at 64 MiB so a misbehaving server
/// cannot exhaust memory.
pub(crate) fn read_body(resp: ureq::Response) -> Result<Vec<u8>, std::io::Error> {
    use std::io::Read;
    const CAP: u64 = 64 * 1024 * 1024;
    let mut buf = Vec::new();
    resp.into_reader().take(CAP).read_to_end(&mut buf)?;
    Ok(buf)
}

/// `mkdir -p` the parent, then write — the Python's
/// `out_path.parent.mkdir(parents=True, exist_ok=True)` followed by
/// `write_bytes`.
fn write_all_with_parents(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, bytes)
}

/// Every way synthesis can fail, as a value.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TtsError {
    /// No voice backend is configured — `NullTTS.synth_to_file`.
    #[error(transparent)]
    Null(#[from] NullTtsError),
    /// The ElevenLabs (BYOK) path failed.
    #[error(transparent)]
    ElevenLabs(#[from] ElevenLabsError),
    /// A registered backend (see [`select::register_backend`]) failed.
    /// `name` is the backend's [`select::BackendFactory::name`]; downcast
    /// `source` to the backend's own error type to route on it.
    #[error("{source}")]
    Backend {
        /// The registered backend's name.
        name: &'static str,
        /// The backend's own error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The local Kokoro path failed.
    #[cfg(feature = "kokoro")]
    #[error(transparent)]
    Kokoro(#[from] kokoro::KokoroError),
    /// `write_wav` was called on audio in a container this crate does not
    /// decode.
    #[error("audio is {0}, not PCM — use Audio::write to keep the native container")]
    NotPcm(&'static str),
    /// Writing the audio out failed.
    #[error("writing audio: {0}")]
    Io(#[from] io::Error),
}

/// One text-to-speech backend.
///
/// Object-safe on purpose: the daemon holds exactly one in a field, the way
/// the Python holds `self.tts`, and [`select_backend`] returns a
/// `Box<dyn Tts>`.
pub trait Tts: Send + Sync {
    /// The extension the daemon should mint a tempfile with, including the
    /// dot. `AUDIO_EXT` in the Python.
    fn audio_ext(&self) -> &'static str;

    /// The fastest speed this backend renders natively. Above it, the daemon
    /// layers `afplay -r`. `MAX_NATIVE_SPEED` in the Python.
    fn max_native_speed(&self) -> f64;

    /// Whether this backend has what it needs to synthesise — the Python's
    /// `is_configured`. [`NullTts`] is the one that answers `false`.
    fn is_configured(&self) -> bool {
        true
    }

    /// Voice names this backend accepts, for `heard voices`.
    fn list_voices(&self) -> Vec<String> {
        Vec::new()
    }

    /// Synthesise. `lang` is carried for parity with the Python signature;
    /// only Kokoro reads it (the two HTTP backends ignore it, as they do in
    /// the Python, because the model infers language from the text).
    ///
    /// # Errors
    /// Backend-specific — see [`TtsError`]. Never panics on a bad response,
    /// a timeout or a missing credential.
    fn synth(&self, text: &str, voice: &str, speed: f64, lang: &str) -> Result<Audio, TtsError>;

    /// Synthesise straight to a file, in the backend's native container.
    /// The exact shape of the Python's `synth_to_file`, kept so the daemon
    /// port is a transcription.
    ///
    /// # Errors
    /// Whatever [`Tts::synth`] returns, plus any I/O failure writing.
    fn synth_to_file(
        &self,
        text: &str,
        voice: &str,
        speed: f64,
        lang: &str,
        out_path: &Path,
    ) -> Result<(), TtsError> {
        self.synth(text, voice, speed, lang)?
            .write(out_path)
            .map_err(TtsError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: Vec<f32>) -> Pcm {
        Pcm {
            samples,
            sample_rate: 24_000,
        }
    }

    #[test]
    fn wav_header_is_a_riff_wave_of_the_right_length() {
        let bytes = pcm(vec![0.0; 100]).to_wav_bytes();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");
        // 44-byte header + 100 samples × 2 bytes.
        assert_eq!(bytes.len(), 244);
        // RIFF size is everything after the first 8 bytes.
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            (bytes.len() - 8) as u32
        );
        // 24 kHz, mono, 16-bit.
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            24_000
        );
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
    }

    #[test]
    fn samples_are_clamped_not_wrapped() {
        let bytes = pcm(vec![2.0, -2.0]).to_wav_bytes();
        assert_eq!(
            i16::from_le_bytes(bytes[44..46].try_into().unwrap()),
            i16::MAX
        );
        assert_eq!(
            i16::from_le_bytes(bytes[46..48].try_into().unwrap()),
            -i16::MAX
        );
    }

    #[test]
    fn duration_is_samples_over_rate() {
        assert!((pcm(vec![0.0; 24_000]).duration_secs() - 1.0).abs() < f64::EPSILON);
        assert_eq!(
            Pcm {
                samples: vec![0.0; 10],
                sample_rate: 0
            }
            .duration_secs(),
            0.0
        );
    }

    #[test]
    fn write_wav_on_encoded_audio_is_a_sentence_not_a_panic() {
        let audio = Audio::Encoded(Encoded {
            bytes: vec![0xFF, 0xFB],
            ext: ".mp3",
        });
        let err = audio
            .write_wav(Path::new("/nonexistent/x.wav"))
            .unwrap_err();
        assert!(matches!(err, TtsError::NotPcm(".mp3")));
        assert!(err.to_string().contains("not PCM"));
    }

    #[test]
    fn ext_and_byte_len_follow_the_container() {
        let p = Audio::Pcm(pcm(vec![0.0; 10]));
        assert_eq!(p.ext(), ".wav");
        assert_eq!(p.byte_len(), 64);
        assert!(p.as_pcm().is_some());

        let e = Audio::Encoded(Encoded {
            bytes: vec![1, 2, 3],
            ext: ".mp3",
        });
        assert_eq!(e.ext(), ".mp3");
        assert_eq!(e.byte_len(), 3);
        assert!(e.as_pcm().is_none());
    }

    #[test]
    fn write_creates_missing_parent_directories() {
        let dir = std::env::temp_dir().join(format!("heard-tts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("deep").join("nested").join("a.mp3");
        Audio::Encoded(Encoded {
            bytes: b"idat".to_vec(),
            ext: ".mp3",
        })
        .write(&path)
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"idat");
        let _ = fs::remove_dir_all(&dir);
    }
}
