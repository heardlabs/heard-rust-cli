//! `fetch_voice_library` against a loopback fake — `GET /v1/voices` with the
//! key header, rows mapped the Python's way, and "an empty list on any
//! failure". Every key here is a fake.

mod fake_http;

use std::time::Duration;

use fake_http::{FakeServer, Reply};
use heard_tts::{ElevenLabsTts, LibraryVoice};

fn backend(server: &FakeServer) -> ElevenLabsTts {
    ElevenLabsTts::new("sk_test_fake")
        .with_api_base(&format!("{}/v1", server.base_url()))
        .with_timeout(Duration::from_secs(5))
}

fn json(body: &str) -> Reply {
    Reply {
        status: 200,
        content_type: "application/json",
        body: body.as_bytes().to_vec(),
    }
}

#[test]
fn the_library_is_read_from_get_voices_with_the_key() {
    let server = FakeServer::start(json(
        r#"{"voices": [
            {"voice_id": " abc ", "name": " Rachel ", "description": "calm", "category": "premade"},
            {"voice_id": "", "name": "no id"},
            {"voice_id": "def", "name": "", "labels": {}},
            {"voice_id": 7, "name": "not a string id"}
        ]}"#,
    ));
    let voices = backend(&server).fetch_voice_library();
    let req = server.captured();
    assert_eq!(req.method(), "GET");
    assert_eq!(req.path(), "/v1/voices");
    assert_eq!(req.header("xi-api-key"), Some("sk_test_fake"));
    assert_eq!(
        voices,
        vec![
            LibraryVoice {
                id: "abc".into(),
                name: "Rachel".into(),
                description: "calm".into(),
                category: "premade".into(),
            },
            LibraryVoice {
                id: "def".into(),
                name: "—".into(),
                description: String::new(),
                category: String::new(),
            },
        ]
    );
}

#[test]
fn any_failure_is_an_empty_list() {
    let server = FakeServer::start(Reply::json_error(401, r#"{"detail": "bad key"}"#));
    assert!(backend(&server).fetch_voice_library().is_empty());
    let server = FakeServer::start(json("not json"));
    assert!(backend(&server).fetch_voice_library().is_empty());
    let server = FakeServer::start_hanging_up();
    assert!(backend(&server).fetch_voice_library().is_empty());
}

#[test]
fn no_key_makes_no_request() {
    // A black-hole address: reaching it would hang the test.
    let t = ElevenLabsTts::new("  ").with_api_base("http://240.0.0.1:1");
    assert!(t.fetch_voice_library().is_empty());
}
