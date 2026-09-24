//! The ElevenLabs backend against a fake server, asserting the exact request
//! `engine/heard/tts/elevenlabs.py` sends and the exact failures it raises.
//!
//! Every key here is a fake (`sk_test_fake`). Nothing resolves
//! `api.elevenlabs.io`; the backend is pointed at `127.0.0.1` for all of it.

mod fake_http;

use std::time::Duration;

use fake_http::{FakeServer, Reply};
use heard_tts::{ElevenLabsError, ElevenLabsTts, Tts, TtsError};

/// A couple of bytes standing in for an MP3 frame. The backend never decodes
/// it — `elevenlabs.py`: "no decoding, no in-process audio buffer".
const FAKE_MP3: &[u8] = &[0xFF, 0xFB, 0x90, 0x64, 0x00, 0x11, 0x22];

/// `API_BASE` is `https://api.elevenlabs.io/v1` — the `/v1` is part of the
/// base, not of the path the backend appends. The fake carries it too, so the
/// asserted paths below are the real ones.
fn backend(server: &FakeServer) -> ElevenLabsTts {
    ElevenLabsTts::new("sk_test_fake")
        .with_api_base(&format!("{}/v1", server.base_url()))
        .with_timeout(Duration::from_secs(5))
}

#[test]
fn the_request_matches_the_python_byte_for_byte() {
    let server = FakeServer::start(Reply::audio(FAKE_MP3));
    let audio = backend(&server)
        .synth("Hello from Heard.", "jarvis", 1.0, "en-us")
        .expect("synth should succeed");

    let req = server.captured();

    // POST /v1/text-to-speech/{voice_id}?output_format=mp3_44100_128
    assert_eq!(req.method(), "POST");
    assert_eq!(req.path(), "/v1/text-to-speech/Fahco4VZzobUeiPqni1S");
    assert_eq!(req.query(), "output_format=mp3_44100_128");

    // The three headers the Python sets, and no Authorization.
    assert_eq!(req.header("xi-api-key"), Some("sk_test_fake"));
    assert_eq!(req.header("content-type"), Some("application/json"));
    assert_eq!(req.header("accept"), Some("audio/mpeg"));
    assert_eq!(req.header("authorization"), None);

    // The body, key for key and in the Python's insertion order.
    assert_eq!(
        req.body_str(),
        r#"{"text":"Hello from Heard.","model_id":"eleven_flash_v2_5","voice_settings":{"stability":0.5,"similarity_boost":0.75,"speed":1.0}}"#
    );

    // The MP3 comes back verbatim, undecoded.
    let encoded = match &audio {
        heard_tts::Audio::Encoded(e) => e,
        heard_tts::Audio::Pcm(_) => panic!("ElevenLabs must not return PCM"),
    };
    assert_eq!(encoded.bytes, FAKE_MP3);
    assert_eq!(encoded.ext, ".mp3");
    assert_eq!(audio.ext(), ".mp3");
}

#[test]
fn an_unknown_voice_silently_becomes_george() {
    let server = FakeServer::start(Reply::audio(FAKE_MP3));
    backend(&server)
        .synth("hi", "not-a-voice", 1.0, "en-us")
        .unwrap();
    assert_eq!(
        server.captured().path(),
        "/v1/text-to-speech/JBFqnCBsd6RMkjVDRZzb"
    );
}

#[test]
fn a_raw_voice_id_is_used_as_given() {
    let server = FakeServer::start(Reply::audio(FAKE_MP3));
    backend(&server)
        .synth("hi", "XB0fDUnXU5powFXDhCwa", 1.0, "en-us")
        .unwrap();
    assert_eq!(
        server.captured().path(),
        "/v1/text-to-speech/XB0fDUnXU5powFXDhCwa"
    );
}

#[test]
fn speed_is_clamped_on_the_wire_not_rejected() {
    let server = FakeServer::start(Reply::audio(FAKE_MP3));
    // 1.7 is the real "Brisk" preset; the Python clamps to 1.2 and lets the
    // daemon make up the rest with `afplay -r`.
    backend(&server)
        .synth("hi", "george", 1.7, "en-us")
        .unwrap();
    let body = server.captured().body_json();
    assert_eq!(body["voice_settings"]["speed"], 1.2);
}

#[test]
fn a_401_is_a_sentence_carrying_the_status_and_the_body() {
    let server = FakeServer::start(Reply::json_error(
        401,
        r#"{"detail":{"status":"invalid_api_key"}}"#,
    ));
    let err = backend(&server)
        .synth("hi", "george", 1.0, "en-us")
        .unwrap_err();

    let TtsError::ElevenLabs(ElevenLabsError::Http { status, detail }) = err else {
        panic!("expected an HTTP error, got {err:?}")
    };
    assert_eq!(status, 401);
    assert!(detail.contains("invalid_api_key"));
    assert_eq!(
        TtsError::from(ElevenLabsError::Http {
            status: 401,
            detail: "x".into()
        })
        .to_string(),
        "ElevenLabs HTTP 401: x"
    );
}

#[test]
fn a_429_is_the_same_shape_with_its_own_status() {
    let server = FakeServer::start(Reply::json_error(
        429,
        r#"{"detail":{"status":"too_many_concurrent_requests"}}"#,
    ));
    let err = backend(&server)
        .synth("hi", "george", 1.0, "en-us")
        .unwrap_err();
    let TtsError::ElevenLabs(ElevenLabsError::Http { status, detail }) = err else {
        panic!("expected an HTTP error")
    };
    assert_eq!(status, 429);
    assert!(detail.contains("too_many_concurrent_requests"));
}

#[test]
fn a_500_is_reported_not_retried() {
    // The Python makes exactly one attempt; a retry loop would be a
    // divergence, and the fake only ever accepts one connection, so a second
    // attempt would surface here as a transport error instead.
    let server = FakeServer::start(Reply::text_error(500, "upstream exploded"));
    let err = backend(&server)
        .synth("hi", "george", 1.0, "en-us")
        .unwrap_err();
    let TtsError::ElevenLabs(ElevenLabsError::Http { status, detail }) = err else {
        panic!("expected an HTTP error")
    };
    assert_eq!(status, 500);
    assert_eq!(detail, "upstream exploded");
}

#[test]
fn a_long_error_body_is_truncated_to_two_hundred_characters() {
    let long = "x".repeat(5_000);
    let server = FakeServer::start(Reply::text_error(500, &long));
    let err = backend(&server)
        .synth("hi", "george", 1.0, "en-us")
        .unwrap_err();
    let TtsError::ElevenLabs(ElevenLabsError::Http { detail, .. }) = err else {
        panic!("expected an HTTP error")
    };
    assert_eq!(detail.chars().count(), 200);
}

#[test]
fn a_two_hundred_with_no_body_is_empty_audio_not_a_silent_success() {
    let server = FakeServer::start(Reply::audio(&[]));
    let err = backend(&server)
        .synth("hi", "george", 1.0, "en-us")
        .unwrap_err();
    assert!(matches!(
        err,
        TtsError::ElevenLabs(ElevenLabsError::EmptyAudio)
    ));
    assert_eq!(err.to_string(), "ElevenLabs returned empty audio");
}

#[test]
fn a_dropped_connection_is_a_network_error() {
    let server = FakeServer::start_hanging_up();
    let err = ElevenLabsTts::new("sk_test_fake")
        .with_api_base(&format!("{}/v1", server.base_url()))
        .with_timeout(Duration::from_secs(5))
        .synth("hi", "george", 1.0, "en-us")
        .unwrap_err();
    assert!(
        matches!(err, TtsError::ElevenLabs(ElevenLabsError::Network(_))),
        "got {err:?}"
    );
    assert!(err.to_string().starts_with("ElevenLabs network error:"));
}

#[test]
fn synth_to_file_writes_the_mp3_bytes_unchanged() {
    let server = FakeServer::start(Reply::audio(FAKE_MP3));
    let dir = std::env::temp_dir().join(format!("heard-tts-el-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("nested").join("out.mp3");

    backend(&server)
        .synth_to_file("hi", "george", 1.0, "en-us", &path)
        .unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), FAKE_MP3);
    let _ = std::fs::remove_dir_all(&dir);
}
