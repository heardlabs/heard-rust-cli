//! ElevenLabs TTS backend — ported from `engine/heard/tts/elevenlabs.py`.
//!
//! # The exact request
//!
//! ```text
//! POST https://api.elevenlabs.io/v1/text-to-speech/{voice_id}?output_format=mp3_44100_128
//! xi-api-key: {key}
//! Content-Type: application/json
//! Accept: audio/mpeg
//!
//! {"text": …, "model_id": "eleven_flash_v2_5",
//!  "voice_settings": {"stability": 0.5, "similarity_boost": 0.75, "speed": …}}
//! ```
//!
//! The response body is an MP3 stream, written to disk verbatim for `afplay` —
//! "no decoding, no in-process audio buffer". Body key order comes from
//! [`SynthBody`]'s field order, because the Python's `json.dumps` over a literal
//! dict emits insertion order and a wire diff against a capture should be
//! empty, not merely equivalent. A `serde_json::Map` would *not* do: without
//! the `preserve_order` feature it is a `BTreeMap` and alphabetises the keys.
//!
//! # Deliberate divergences
//!
//! * **Compact JSON.** Python's `json.dumps` defaults to `", "` and `": "`
//!   separators; `serde_json::to_vec` emits no spaces. Keys, key *order* and
//!   values are identical — verified by capturing what `synth_to_file` puts on
//!   the wire with `urlopen` monkeypatched — so the two bodies differ only in
//!   insignificant whitespace and a few bytes of `Content-Length`. Both are the
//!   same JSON document to any parser. Matching the spacing would mean
//!   hand-rolling a serialiser to reproduce a formatting default, which is not
//!   worth a byte-identical `Content-Length`.
//! * **No certifi.** The Python builds an `ssl` context from certifi's PEM
//!   bundle because py2app's frozen interpreter ships without a CA bundle on
//!   the path `_ssl` was compiled against, so every synth failed with
//!   `CERTIFICATE_VERIFY_FAILED`. `ureq` uses `rustls` with compiled-in roots:
//!   there is no filesystem bundle to miss, so the workaround has nothing to
//!   work around. This is the one place the port is strictly better rather
//!   than equal.
//! * **`fetch_voice_library` is not ported.** It backs `heard voices --all`,
//!   a CLI path, not the daemon's speech path, and it is the one method whose
//!   contract is "return an empty list on any failure" — worth porting with
//!   the CLI, not with the backend.

use std::time::Duration;

use serde::Serialize;

use crate::voices::{alias_names, clamp_speed, resolve_voice_id};
use crate::{Audio, Encoded, Tts, TtsError};

/// `voice_settings` — the three fields the Python sends, in its order.
///
/// `stability` and `similarity_boost` are hard-coded in `elevenlabs.py`; only
/// `speed` varies, and it arrives already clamped to `[0.7, 1.2]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct VoiceSettings {
    /// Always `0.5`.
    pub stability: f64,
    /// Always `0.75`.
    pub similarity_boost: f64,
    /// Clamped to `[0.7, 1.2]` by [`crate::voices::clamp_speed`].
    pub speed: f64,
}

/// The POST body, field order matching `json.dumps` over the Python's dict.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SynthBody<'a> {
    /// The words to speak.
    pub text: &'a str,
    /// `eleven_flash_v2_5` unless overridden.
    pub model_id: &'a str,
    /// Stability, similarity and speed.
    pub voice_settings: VoiceSettings,
}

/// `https://api.elevenlabs.io/v1`.
pub const API_BASE: &str = "https://api.elevenlabs.io/v1";
/// `eleven_flash_v2_5` — the fastest tier, ~75 ms TTFB.
pub const DEFAULT_MODEL_ID: &str = "eleven_flash_v2_5";
/// 8 seconds, matching `DEFAULT_TIMEOUT_S`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);
/// The `output_format` query parameter, fixed by the Python.
pub const OUTPUT_FORMAT: &str = "mp3_44100_128";

/// Anything went wrong synthesising via ElevenLabs.
///
/// The Python raises one `ElevenLabsError` with a formatted message for all
/// three modes. Splitting them into variants loses nothing — `Display` still
/// produces the same sentences — and lets the daemon match on the mode without
/// parsing a string. The HTTP variant keeps `status` because "429 vs 401" is
/// the difference between backing off and re-prompting for a key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ElevenLabsError {
    /// No key. `"no ElevenLabs API key configured"`.
    #[error("no ElevenLabs API key configured")]
    NoKey,
    /// A non-2xx response. Body detail is truncated to 200 characters, as in
    /// the Python.
    #[error("ElevenLabs HTTP {status}: {detail}")]
    Http {
        /// The HTTP status code.
        status: u16,
        /// The first 200 characters of the response body.
        detail: String,
    },
    /// DNS, TCP, TLS or timeout — the Python's `URLError` / `TimeoutError`.
    #[error("ElevenLabs network error: {0}")]
    Network(String),
    /// A 2xx with a zero-length body. `"ElevenLabs returned empty audio"`.
    #[error("ElevenLabs returned empty audio")]
    EmptyAudio,
}

/// Stateless — no model in memory. Same shape as the other backends so the
/// selector can swap them freely.
#[derive(Debug, Clone)]
pub struct ElevenLabsTts {
    api_key: String,
    model_id: String,
    timeout: Duration,
    /// Overridden by tests to point at a fake server. Production always uses
    /// [`API_BASE`]; the Python has no equivalent knob because it monkeypatches
    /// `urlopen` instead, which Rust cannot do.
    api_base: String,
}

impl ElevenLabsTts {
    /// A backend with the production endpoint, model and timeout.
    #[must_use]
    pub fn new(api_key: &str) -> Self {
        Self {
            api_key: api_key.trim().to_string(),
            model_id: DEFAULT_MODEL_ID.to_string(),
            timeout: DEFAULT_TIMEOUT,
            api_base: API_BASE.to_string(),
        }
    }

    /// Override the model id — `model_id` in the Python constructor.
    #[must_use]
    pub fn with_model_id(mut self, model_id: &str) -> Self {
        self.model_id = model_id.to_string();
        self
    }

    /// Override the request timeout — `timeout_s` in the Python constructor.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Point at a different base URL. The tests' seam onto a fake server;
    /// nothing in production calls it.
    #[must_use]
    pub fn with_api_base(mut self, api_base: &str) -> Self {
        self.api_base = api_base.trim_end_matches('/').to_string();
        self
    }

    /// The URL a synth for `voice` would POST to.
    #[must_use]
    pub fn synth_url(&self, voice: &str) -> String {
        format!(
            "{}/text-to-speech/{}?output_format={}",
            self.api_base,
            resolve_voice_id(voice),
            OUTPUT_FORMAT
        )
    }

    /// The JSON body a synth would send, in the Python's key order.
    ///
    /// Built from [`SynthBody`] rather than a [`serde_json::Map`]: `serde_json`
    /// without the `preserve_order` feature backs its maps with a `BTreeMap`,
    /// so a map-built body comes out alphabetised (`model_id` before `text`)
    /// while `json.dumps` over a Python dict emits insertion order. Struct
    /// field order is preserved regardless of that feature, which gets the
    /// byte-for-byte match without turning on a feature that would change key
    /// ordering for every other crate in the workspace.
    #[must_use]
    pub fn synth_body<'a>(&'a self, text: &'a str, speed: f64) -> SynthBody<'a> {
        SynthBody {
            text,
            model_id: &self.model_id,
            voice_settings: VoiceSettings {
                stability: 0.5,
                similarity_boost: 0.75,
                speed: clamp_speed(speed),
            },
        }
    }
}

impl Tts for ElevenLabsTts {
    fn audio_ext(&self) -> &'static str {
        ".mp3"
    }

    /// `1.2` — `voice_settings.speed` caps there, so the daemon layers
    /// `afplay -r` above it (the "Brisk" preset at 1.7× is the reason).
    fn max_native_speed(&self) -> f64 {
        1.2
    }

    fn is_configured(&self) -> bool {
        !self.api_key.is_empty()
    }

    fn list_voices(&self) -> Vec<String> {
        alias_names()
    }

    fn synth(&self, text: &str, voice: &str, speed: f64, _lang: &str) -> Result<Audio, TtsError> {
        if self.api_key.is_empty() {
            return Err(ElevenLabsError::NoKey.into());
        }

        let body = serde_json::to_vec(&self.synth_body(text, speed))
            .map_err(|e| ElevenLabsError::Network(e.to_string()))?;

        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();

        let resp = agent
            .post(&self.synth_url(voice))
            .set("xi-api-key", &self.api_key)
            .set("Content-Type", "application/json")
            .set("Accept", "audio/mpeg")
            .send_bytes(&body);

        let audio = match resp {
            Ok(r) => crate::read_body(r).map_err(|e| ElevenLabsError::Network(e.to_string()))?,
            // `urllib` raises HTTPError for any non-2xx; ureq's Status arm is
            // the same event. The Python reads up to 200 characters of the
            // body for the message and ignores a read failure.
            Err(ureq::Error::Status(code, r)) => {
                let detail = r.into_string().unwrap_or_default();
                let detail: String = detail.chars().take(200).collect();
                return Err(ElevenLabsError::Http {
                    status: code,
                    detail,
                }
                .into());
            }
            Err(e @ ureq::Error::Transport(_)) => {
                return Err(ElevenLabsError::Network(e.to_string()).into())
            }
        };

        if audio.is_empty() {
            return Err(ElevenLabsError::EmptyAudio.into());
        }
        Ok(Audio::Encoded(Encoded {
            bytes: audio,
            ext: ".mp3",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_carries_the_resolved_voice_and_the_fixed_output_format() {
        let t = ElevenLabsTts::new("sk_test_fake");
        assert_eq!(
            t.synth_url("jarvis"),
            "https://api.elevenlabs.io/v1/text-to-speech/Fahco4VZzobUeiPqni1S?output_format=mp3_44100_128"
        );
        // Empty voice → George.
        assert!(t
            .synth_url("")
            .ends_with("/JBFqnCBsd6RMkjVDRZzb?output_format=mp3_44100_128"));
    }

    #[test]
    fn body_matches_python_json_dumps_key_for_key() {
        let t = ElevenLabsTts::new("sk_test_fake");
        let got = serde_json::to_string(&t.synth_body("hi", 1.0)).unwrap();
        assert_eq!(
            got,
            r#"{"text":"hi","model_id":"eleven_flash_v2_5","voice_settings":{"stability":0.5,"similarity_boost":0.75,"speed":1.0}}"#
        );
    }

    #[test]
    fn body_speed_is_clamped_before_it_goes_on_the_wire() {
        let t = ElevenLabsTts::new("sk_test_fake");
        assert_eq!(t.synth_body("hi", 1.7).voice_settings.speed, 1.2);
        assert_eq!(t.synth_body("hi", 0.1).voice_settings.speed, 0.7);
        // And serialises as `1.2`, not `1.2000000476837158`.
        assert!(serde_json::to_string(&t.synth_body("hi", 1.7))
            .unwrap()
            .contains(r#""speed":1.2}"#));
    }

    #[test]
    fn no_key_fails_before_any_socket_is_opened() {
        // api_base is a black-hole address; reaching it would hang, so a fast
        // failure proves the key check comes first.
        let t = ElevenLabsTts::new("   ").with_api_base("http://240.0.0.1:1");
        let err = t.synth("hi", "george", 1.0, "en-us").unwrap_err();
        assert!(matches!(err, TtsError::ElevenLabs(ElevenLabsError::NoKey)));
        assert!(!t.is_configured());
    }

    #[test]
    fn surface_matches_the_python_class_attributes() {
        let t = ElevenLabsTts::new("sk_test_fake");
        assert_eq!(t.audio_ext(), ".mp3");
        assert!((t.max_native_speed() - 1.2).abs() < f64::EPSILON);
        assert!(t.is_configured());
        assert_eq!(t.list_voices().len(), crate::voices::ALIASES.len());
    }
}
