//! The default build — and the `kokoro` build that ships the local voice —
//! contains no espeak-ng and no GPL-only crate.
//!
//! espeak-ng is GPL-3.0-or-later. The only espeak route in this crate is the
//! opt-in `espeak` feature, which runs a binary as a subprocess and adds no
//! dependency. This test keeps it that way: it asks `cargo tree` for the
//! resolved dependency graph of the builds that matter and fails if a crate
//! whose name says espeak/piper appears, or a crate whose licence is GPL with
//! no permissive alternative.

use std::process::Command;

fn tree(features: &[&str]) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let mut cmd = Command::new(cargo);
    cmd.args([
        "tree",
        "--offline",
        "--manifest-path",
        manifest,
        "-p",
        "heard-tts",
        "-e",
        "normal,build",
        "--prefix",
        "none",
        "--format",
        "{p}|{l}",
    ]);
    if !features.is_empty() {
        cmd.args(["--features", &features.join(",")]);
    }
    let out = cmd.output().expect("cargo tree runs");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8")
}

fn assert_clean(features: &[&str]) {
    let tree = tree(features);
    assert!(
        tree.contains("heard-tts"),
        "unexpected cargo tree output:\n{tree}"
    );
    for line in tree.lines() {
        let (pkg, licence) = line.split_once('|').unwrap_or((line, ""));
        let name = pkg.to_ascii_lowercase();
        assert!(
            !name.contains("espeak") && !name.contains("piper"),
            "features {features:?} pull in {pkg} — the default G2P must not touch espeak-ng"
        );
        // "MIT OR Apache-2.0 OR LGPL-2.1-or-later" offers a permissive choice;
        // a bare GPL does not.
        assert!(
            !(licence.contains("GPL") && !licence.contains(" OR ")),
            "features {features:?} pull in {pkg} under {licence}"
        );
    }
}

#[test]
fn the_default_build_has_no_espeak_dependency() {
    assert_clean(&[]);
}

#[test]
fn the_kokoro_build_has_no_espeak_dependency() {
    let tree = tree(&["kokoro"]);
    assert!(
        tree.contains("sayd-misaki-en"),
        "the kokoro build should phonemise with sayd-misaki-en:\n{tree}"
    );
    assert_clean(&["kokoro"]);
}

/// The opt-in feature is a subprocess, not a dependency: even with it on,
/// no espeak crate is linked.
#[test]
fn even_the_espeak_feature_links_no_espeak_crate() {
    assert_clean(&["espeak"]);
}
