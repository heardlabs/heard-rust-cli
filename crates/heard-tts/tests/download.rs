//! The Kokoro downloader (`heard_tts::download`) against a loopback fake:
//! fetch, verify, atomic rename, resume with `Range`, and a checksum
//! mismatch deleting the bad file. Never touches the real release.

mod fake_http;

use std::path::PathBuf;

use fake_http::{FakeServer, Reply};
use heard_tts::download::{self, ModelFile, NoProgress, Outcome, Source};

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("heard-tts-dl-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn sha(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn bin(status: u16, body: &[u8]) -> Reply {
    Reply {
        status,
        content_type: "application/octet-stream",
        body: body.to_vec(),
    }
}

fn source(server: &FakeServer, body: &[u8], sha256: String) -> Source {
    Source {
        base_url: String::new(),
        files: vec![ModelFile {
            name: "model.bin".into(),
            size: body.len() as u64,
            sha256,
        }],
        retry: "try again".into(),
    }
    .with_base_url(server.base_url())
}

#[test]
fn a_file_is_fetched_verified_and_moved_into_place() {
    let dir = tmp("fresh");
    let body = b"kokoro-weights".to_vec();
    let server = FakeServer::start(bin(200, &body));
    let src = source(&server, &body, sha(&body));
    let out = download::download(&src, &dir, &mut NoProgress).unwrap();
    assert_eq!(
        out,
        vec![(
            "model.bin".to_string(),
            Outcome::Downloaded {
                fetched: body.len() as u64,
                resumed_from: 0
            }
        )]
    );
    assert_eq!(server.captured().path(), "/model.bin");
    assert_eq!(std::fs::read(dir.join("model.bin")).unwrap(), body);
    assert!(!dir.join("model.bin.part").exists());
    assert!(download::installed(&dir, &src.files));
    // Present and verified: no request at all the second time.
    let again = download::download(&src, &dir, &mut NoProgress).unwrap();
    assert_eq!(again[0].1, Outcome::AlreadyPresent);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_partial_resumes_with_a_range_request() {
    let dir = tmp("resume");
    let body = b"0123456789".to_vec();
    std::fs::write(dir.join("model.bin.part"), &body[..4]).unwrap();
    let rest = body[4..].to_vec();
    let server = FakeServer::start(bin(206, &rest));
    let src = source(&server, &body, sha(&body));
    let out = download::download(&src, &dir, &mut NoProgress).unwrap();
    assert_eq!(
        out[0].1,
        Outcome::Downloaded {
            fetched: 6,
            resumed_from: 4
        }
    );
    assert_eq!(server.captured().header("range"), Some("bytes=4-"));
    assert_eq!(std::fs::read(dir.join("model.bin")).unwrap(), body);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_checksum_mismatch_deletes_the_file_and_names_the_retry() {
    let dir = tmp("badsum");
    let body = b"tampered".to_vec();
    let server = FakeServer::start(bin(200, &body));
    let src = source(&server, &body, "0".repeat(64));
    let err = download::download(&src, &dir, &mut NoProgress).unwrap_err();
    assert!(err.message.contains("checksum mismatch"), "{err:?}");
    assert!(err.fix.starts_with("try again"), "{err:?}");
    assert!(!dir.join("model.bin").exists());
    assert!(!dir.join("model.bin.part").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_pinned_pair_matches_the_selector_sizes() {
    let p = download::pinned();
    assert_eq!(p[0].name, heard_tts::select::MODEL_FILE);
    assert_eq!(p[0].size, heard_tts::select::MODEL_SIZE);
    assert_eq!(p[1].name, heard_tts::select::VOICES_FILE);
    assert_eq!(p[1].size, heard_tts::select::VOICES_SIZE);
    assert!(Source::pinned("x").base_url.ends_with('/'));
}
