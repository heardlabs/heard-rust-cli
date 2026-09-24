//! The Rust half of the PyYAML golden corpus.
//!
//! The fixture generator (not included) ran the real `yaml.safe_dump` /
//! `yaml.safe_load` over every `config.DEFAULTS` value, every `config.yaml`
//! the engine-api corpus touches and a long adversarial list, and recorded
//! the exact text and the loaded value. Here the port must produce the SAME
//! bytes and the SAME values.

use heard_config::pyyaml::{safe_dump, safe_load, ErrorKind};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> Vec<Map<String, Value>> {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/config")
        .join(format!("{name}.json"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} — regenerate with the fixture generator",
            path.display()
        )
    });
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// NaN never equals itself, and the corpus spells it as a sentinel string, so
/// plain `==` is the right comparison everywhere.
fn same(a: &Value, b: &Value) -> bool {
    a == b
}

#[test]
fn every_dump_is_byte_identical_to_pyyaml() {
    let cases = fixture("yaml_dump");
    assert!(cases.len() >= 1000, "only {} dump cases", cases.len());
    let mut failures = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let want = case["output"].as_str().unwrap();
        let got = safe_dump(&case["input"]);
        if got != want {
            failures.push(format!(
                "  {name}\n    python: {want:?}\n    rust:   {got:?}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} dumps differ from PyYAML:\n{}",
        failures.len(),
        cases.len(),
        failures
            .iter()
            .take(60)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn every_load_matches_pyyaml() {
    let cases = fixture("yaml_load");
    assert!(cases.len() >= 1000, "only {} load cases", cases.len());
    let mut failures = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let text = case["input"].as_str().unwrap();
        let want = &case["output"];
        let got = safe_load(text);
        let ok = match (
            want.get("value"),
            want.get("error"),
            want.get("divergence"),
            &got,
        ) {
            (Some(v), _, _, Ok(g)) => same(v, g),
            (_, Some(e), _, Err(g)) => match e.as_str().unwrap() {
                "yaml" => g.kind == ErrorKind::Yaml,
                _ => g.kind == ErrorKind::Python,
            },
            // A lone surrogate: Python loads it, a Rust String cannot hold it.
            (_, _, Some(_), Err(g)) => g.kind == ErrorKind::Python,
            _ => false,
        };
        if !ok {
            let got = match &got {
                Ok(v) => format!("value {v}"),
                Err(e) => format!("{:?} error: {}", e.kind, e.message.replace('\n', " | ")),
            };
            failures.push(format!(
                "  {name}\n    input:  {text:?}\n    python: {want}\n    rust:   {got}"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} loads differ from PyYAML:\n{}",
        failures.len(),
        cases.len(),
        failures
            .iter()
            .take(60)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_brief_regressions() {
    // 1e+16 is a STRING to PyYAML (its float regex requires a dot) and is
    // written unquoted; both directions must agree.
    let v = safe_load("heard_token: 1e+16\n").unwrap();
    assert_eq!(v["heard_token"], Value::String("1e+16".into()));
    let mut m = Map::new();
    m.insert("heard_token".into(), Value::String("1e+16".into()));
    assert_eq!(safe_dump(&Value::Object(m)), "heard_token: 1e+16\n");
    // .inf survives as the float sentinel, not null.
    let v = safe_load("narration_volume: .inf\n").unwrap();
    assert_eq!(
        heard_config::pyyaml::non_finite_float(&v["narration_volume"]),
        Some(f64::INFINITY)
    );
    assert_eq!(safe_dump(&v), "narration_volume: .inf\n");
}

/// `config._read` catches only `yaml.YAMLError`. A YAML error is a corrupt
/// file (renamed, defaults used); any OTHER exception out of `safe_load` —
/// here `datetime.date(2026, 2, 30)`'s ValueError — escapes, so `load()`
/// raises and the file is left alone.
#[test]
fn load_splits_yaml_errors_from_other_python_errors() {
    use heard_config::{Config, ConfigError, Paths};
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("heard-config-yaml-errors");
    let _ = fs::remove_dir_all(&root);
    let paths = Paths::under(&root);
    fs::create_dir_all(&paths.config_dir).unwrap();
    let cfg = Config::new(paths.clone());

    fs::write(&paths.config_path, "narration_volume: 2026-02-30\n").unwrap();
    assert!(matches!(cfg.load(None), Err(ConfigError::Load { .. })));
    assert!(
        paths.config_path.exists(),
        "a non-YAML error must not rename the file"
    );

    fs::write(&paths.config_path, "narration_volume: [1, 2\n").unwrap();
    let loaded = cfg
        .load(None)
        .expect("a corrupt file falls back to defaults");
    assert_eq!(
        loaded["narration_volume"],
        heard_config::defaults()["narration_volume"]
    );
    assert!(
        !paths.config_path.exists(),
        "a corrupt global file is moved aside"
    );
    let _ = fs::remove_dir_all(&root);
}
