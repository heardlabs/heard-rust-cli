//! The Rust half of the `heard.config` golden corpus.
//!
//! `fixtures/config/*.json` was produced by running the real Python module,
//! then cut down to the core's keys (the full app's keys and cases live with
//! the full app, which registers them as a config layer). This file replays
//! the cases here, which is what turns "did I port `config.py` correctly?"
//! from a reading exercise into a failing test.
//!
//! A case marked `"python_only": true` is a documented divergence — read its
//! `why` before deleting the skip.

use heard_config::{
    defaults, find_project_config, Config, ConfigError, PathEnv, Paths, ALWAYS_PERSIST,
    CRITICAL_KEYS, NARRATION_LANGS,
};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

// ── corpus plumbing ─────────────────────────────────────────────────────────

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("config")
}

fn cases(name: &str) -> Vec<Map<String, Value>> {
    let path = fixture_dir().join(format!("{name}.json"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} — regenerate with the fixture generator",
            path.display()
        )
    });
    serde_json::from_str::<Vec<Map<String, Value>>>(&text)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn name_of(case: &Map<String, Value>) -> &str {
    case["name"].as_str().expect("every case has a name")
}

fn skipped(case: &Map<String, Value>) -> bool {
    case.get("python_only").and_then(Value::as_bool) == Some(true)
}

/// A throwaway tree per case, under Cargo's own test tmp dir.
fn scratch(label: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("heard-config")
        .join(format!("{label}-{}", N.fetch_add(1, Ordering::Relaxed)));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("scratch dir");
    root
}

/// `yaml.safe_load(path.read_text())`, or `null` when the file is absent —
/// the shape `fixtures_config._read_yaml_file` records.
fn read_yaml_file(path: &Path) -> Value {
    match fs::read_to_string(path) {
        Ok(text) => heard_config::pyyaml::safe_load(&text).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

fn has_broken_sibling(dir: &Path, prefix: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(std::result::Result::ok).any(|e| {
        e.file_name()
            .to_string_lossy()
            .starts_with(&format!("{prefix}.broken-"))
    })
}

// ── defaults.json ───────────────────────────────────────────────────────────

#[test]
fn defaults_match_the_corpus() {
    let by_name: Map<String, Value> = cases("defaults")
        .into_iter()
        .map(|c| (name_of(&c).to_string(), c["output"].clone()))
        .collect();

    assert_eq!(by_name["DEFAULTS"], Value::Object(defaults().clone()));

    let mut always: Vec<&str> = ALWAYS_PERSIST.to_vec();
    always.sort_unstable();
    assert_eq!(by_name["ALWAYS_PERSIST"], serde_json::json!(always));
    assert_eq!(by_name["CRITICAL_KEYS"], serde_json::json!(CRITICAL_KEYS));

    let langs: Map<String, Value> = NARRATION_LANGS
        .iter()
        .map(|(code, name, tts)| ((*code).to_string(), serde_json::json!([name, tts])))
        .collect();
    assert_eq!(by_name["NARRATION_LANGS"], Value::Object(langs));
}

// ── paths.json ──────────────────────────────────────────────────────────────

#[test]
fn path_constants_match_the_corpus() {
    for case in cases("paths") {
        let name = name_of(&case).to_string();
        let input = case["input"].as_object().expect("input is an object");
        let var = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
        let env = PathEnv {
            home: var("HOME"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            xdg_data_home: var("XDG_DATA_HOME"),
            heard_ledger_path: var("HEARD_LEDGER_PATH"),
        };
        let p = Paths::resolve(&env).unwrap_or_else(|e| panic!("{name}: {e}"));
        let got: Map<String, Value> = [
            ("CONFIG_DIR", &p.config_dir),
            ("DATA_DIR", &p.data_dir),
            ("CONFIG_PATH", &p.config_path),
            ("MODELS_DIR", &p.models_dir),
            ("SOCKET_PATH", &p.socket_path),
            ("LOG_PATH", &p.log_path),
            ("PID_PATH", &p.pid_path),
            ("HEARD_DIR", &p.heard_dir),
            ("TURNS_PATH", &p.turns_path),
            ("LEDGER_PATH", &p.ledger_path),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.display().to_string())))
        .collect();
        assert_eq!(Value::Object(got), case["output"], "{name}");
    }
}

#[test]
fn no_home_is_an_error_not_a_guess() {
    let err = Paths::resolve(&PathEnv::default()).unwrap_err();
    assert!(matches!(err, ConfigError::NoHome));
}

// ── load.json ───────────────────────────────────────────────────────────────

#[test]
fn load_matches_the_corpus() {
    for case in cases("load") {
        let name = name_of(&case).to_string();
        if skipped(&case) {
            continue;
        }
        let input = case["input"].as_object().expect("input is an object");
        let root = scratch(&name);
        let paths = Paths::under(&root);
        let cfg = Config::new(paths.clone());
        cfg.ensure_dirs().expect("ensure_dirs");
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("sub")).expect("repo/sub");

        if let Some(text) = input["config_yaml"].as_str() {
            fs::write(&paths.config_path, text).expect("global layer");
        }
        let project_path = repo.join(".heard.yaml");
        let project_yaml = input["project_yaml"].as_str();
        if let Some(text) = project_yaml {
            fs::write(&project_path, text).expect("project layer");
        }
        let cwd = repo.join("sub");
        let cwd = input["use_cwd"].as_bool().unwrap_or(false).then_some(cwd);

        match cfg.load(cwd.as_deref()) {
            Ok(loaded) => {
                assert!(
                    case["raises"].is_null(),
                    "{name}: expected {} but it loaded",
                    case["raises"]
                );
                assert_eq!(
                    Value::Object(loaded),
                    case["output"],
                    "{name}: {}",
                    case["why"]
                );
            }
            Err(e) => {
                assert!(
                    !case["raises"].is_null(),
                    "{name}: expected a load, got {e}"
                );
                assert!(
                    matches!(e, ConfigError::NotAMapping { .. }),
                    "{name}: expected NotAMapping, got {e}"
                );
            }
        }

        let renamed = has_broken_sibling(&paths.config_dir, "config.yaml");
        assert_eq!(
            renamed,
            case["global_renamed"].as_bool().unwrap(),
            "{name}: the corrupt-global rename"
        );
        assert_eq!(
            paths.config_path.exists(),
            case["global_exists"].as_bool().unwrap(),
            "{name}: whether the global file survived"
        );
        // A per-project .heard.yaml lives in the user's own repo and is never
        // renamed, however broken it is.
        let untouched = project_yaml.is_none_or(|text| {
            project_path.exists()
                && fs::read_to_string(&project_path).unwrap_or_default() == text
                && !has_broken_sibling(&repo, ".heard.yaml")
        });
        assert_eq!(
            untouched,
            case["project_untouched"].as_bool().unwrap(),
            "{name}: the project file must be left alone"
        );
    }
}

#[test]
fn a_corrupt_global_is_renamed_with_its_contents_intact() {
    // test_config.py asserts the backup keeps the original bytes, which is the
    // whole point of renaming instead of deleting.
    let root = scratch("corrupt-backup");
    let paths = Paths::under(&root);
    let cfg = Config::new(paths.clone());
    cfg.ensure_dirs().unwrap();
    fs::write(&paths.config_path, "{}\nkey: value\ngreeted: true\n").unwrap();

    cfg.load(None).expect("recovers with defaults");

    assert!(!paths.config_path.exists());
    let broken: Vec<_> = fs::read_dir(&paths.config_dir)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("config.yaml.broken-")
        })
        .collect();
    assert_eq!(broken.len(), 1);
    let kept = fs::read_to_string(broken[0].path()).unwrap();
    assert!(kept.contains("greeted: true"), "{kept:?}");
}

#[test]
fn find_project_config_walks_up_and_gives_up() {
    let root = scratch("find-project");
    let nested = root.join("workspace").join("a").join("b").join("c");
    fs::create_dir_all(&nested).unwrap();
    let marker = root.join("workspace").join(".heard.yaml");
    fs::write(&marker, "persona: jarvis\n").unwrap();

    let found = find_project_config(Some(&nested)).expect("walks up");
    assert_eq!(
        fs::canonicalize(found).unwrap(),
        fs::canonicalize(&marker).unwrap()
    );

    // A file argument is treated as its directory, as in Python.
    let file = nested.join("main.rs");
    fs::write(&file, "").unwrap();
    assert!(find_project_config(Some(&file)).is_some());

    // No cwd means no project layer — never "use the process cwd".
    assert!(find_project_config(None).is_none());
    assert!(find_project_config(Some(&scratch("bare"))).is_none());
}

#[test]
fn project_label_reads_only_the_project_file() {
    let root = scratch("label");
    let paths = Paths::under(&root);
    let cfg = Config::new(paths.clone());
    cfg.ensure_dirs().unwrap();
    // A stray global `label` must not leak in.
    fs::write(&paths.config_path, "label: Global Leak\n").unwrap();
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();

    assert_eq!(cfg.project_label(Some(&repo)), "");

    fs::write(repo.join(".heard.yaml"), "label:   Heard analytics  \n").unwrap();
    assert_eq!(cfg.project_label(Some(&repo)), "Heard analytics");
}

// ── write.json ──────────────────────────────────────────────────────────────

#[test]
fn the_write_path_matches_the_corpus() {
    for case in cases("write") {
        let name = name_of(&case).to_string();
        if skipped(&case) {
            continue;
        }
        let input = case["input"].as_object().expect("input is an object");
        let root = scratch(&name);
        let paths = Paths::under(&root);
        let cfg = Config::new(paths.clone());
        cfg.ensure_dirs().expect("ensure_dirs");

        if let Some(text) = input.get("disk_yaml").and_then(Value::as_str) {
            fs::write(&paths.config_path, text).expect("seed disk");
        }

        let returned = Value::Null;
        match input["op"].as_str().expect("op") {
            "save" => {
                let clear: Vec<&str> = input
                    .get("intentional_clear")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let body = input["cfg"].as_object().expect("cfg");
                cfg.save(body, &clear).expect("save");
            }
            "set_value" => cfg
                .set_value(input["key"].as_str().unwrap(), input["value"].clone())
                .expect("set_value"),
            "apply_preset" => cfg
                .apply_preset(input["preset"].as_object().expect("preset"))
                .expect("apply_preset"),
            other => panic!("{name}: unknown op {other}"),
        }

        assert_eq!(returned, case["returned"], "{name}: return value");
        assert_eq!(
            read_yaml_file(&paths.config_path),
            case["output"],
            "{name}: {}",
            case["why"]
        );
        assert_eq!(
            read_yaml_file(&paths.backup_path()),
            case["backup"],
            "{name}: the .bak self-heal"
        );
        assert_eq!(
            paths.signed_out_marker().exists(),
            case["signed_out_marker"].as_bool().unwrap(),
            "{name}: the signed-out marker"
        );
    }
}

#[test]
fn a_save_never_leaves_a_partial_file_behind() {
    // The atomic write's reason for existing: a reader must see the old or the
    // new COMPLETE file, and no .config-*.tmp may survive the write.
    let root = scratch("atomic");
    let paths = Paths::under(&root);
    let cfg = Config::new(paths.clone());
    let mut body = Map::new();
    body.insert("voice".into(), Value::String("am_onyx".into()));
    cfg.save(&body, &[]).unwrap();

    let leftovers: Vec<_> = fs::read_dir(&paths.config_dir)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".config-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

#[test]
fn the_config_lock_is_exclusive_and_released_on_drop() {
    let root = scratch("lock");
    let cfg = Config::new(Paths::under(&root));
    {
        let _held = cfg.config_lock().expect("first lock");
        // A second lock from THIS process would deadlock on flock, so only the
        // lifecycle is asserted here; cross-process exclusion is flock's.
        assert!(cfg.paths().lock_path().exists());
    }
    cfg.config_lock().expect("relocks after drop");
}

// ── legacy.json ─────────────────────────────────────────────────────────────

#[test]
fn legacy_mappings_match_the_corpus() {
    use heard_config::legacy;

    for case in cases("legacy") {
        let name = name_of(&case).to_string();
        let input = case["input"].as_object().expect("input is an object");
        let got = match input["fn"].as_str().expect("fn") {
            "normalize_verbosity" => {
                Value::String(legacy::normalize_verbosity(input["name"].as_str()))
            }
            "legacy_mode" => {
                Value::String(legacy::legacy_mode(input["cfg"].as_object().expect("cfg")))
            }
            "dotted_or_underscore" => legacy::dotted_or_underscore(
                input["cfg"].as_object().expect("cfg"),
                input["dotted"].as_str().expect("dotted"),
                &input["default"],
            )
            .clone(),
            other => panic!("{name}: unknown fn {other}"),
        };
        assert_eq!(got, case["output"], "{name}");
    }
}

// ── the corpus itself ───────────────────────────────────────────────────────

#[test]
fn the_corpus_is_present_and_covers_what_it_claims_to() {
    for (name, least) in [
        ("defaults", 4),
        ("load", 15),
        ("paths", 8),
        ("write", 5),
        ("legacy", 15),
    ] {
        let found = cases(name);
        assert!(
            found.len() >= least,
            "{name}.json shrank to {} cases (expected at least {least}) — a corpus \
             that loses its cases passes vacuously",
            found.len()
        );
        for case in &found {
            assert!(
                case.contains_key("input"),
                "{name}: a case without an input"
            );
        }
    }
    // Exactly one documented divergence, and it says so.
    let divergent: Vec<String> = cases("load")
        .iter()
        .filter(|c| skipped(c))
        .map(|c| name_of(c).to_string())
        .collect();
    assert_eq!(divergent, vec!["sequence_of_pairs_is_read_as_a_mapping"]);
}

// ── the core alone ──────────────────────────────────────────────────────────

#[test]
fn with_no_layer_registered_a_save_drops_every_undeclared_key() {
    // What a layer's key looks like to a core that never registered it:
    // undeclared, and dropped by the strict save like any other stray key.
    let root = scratch("core-strict");
    let cfg = Config::new(Paths::under(&root));
    let mut body = Map::new();
    body.insert("extra_model".into(), Value::String("model-b".into()));
    body.insert("voice".into(), Value::String("am_onyx".into()));
    cfg.save(&body, &[]).unwrap();
    assert_eq!(
        read_yaml_file(&cfg.paths().config_path),
        serde_json::json!({"voice": "am_onyx"})
    );
    assert_eq!(heard_config::all_defaults().as_ref(), defaults());
    assert_eq!(heard_config::critical_keys(), CRITICAL_KEYS.to_vec());
}

#[test]
fn the_core_wipe_guard_keeps_a_critical_key_the_writer_lost() {
    let root = scratch("core-wipe-guard");
    let cfg = Config::new(Paths::under(&root));
    cfg.ensure_dirs().unwrap();
    fs::write(
        &cfg.paths().config_path,
        "voice: am_onyx\nonboarded: true\n",
    )
    .unwrap();
    let mut body = Map::new();
    body.insert("mode".into(), Value::String("focus".into()));
    cfg.save(&body, &[]).unwrap();
    assert_eq!(
        read_yaml_file(&cfg.paths().config_path),
        serde_json::json!({"mode": "focus", "onboarded": true, "voice": "am_onyx"})
    );
    cfg.save(&body, &["voice", "onboarded"]).unwrap();
    assert_eq!(
        read_yaml_file(&cfg.paths().config_path),
        serde_json::json!({"mode": "focus"})
    );
    // The core never writes the backup or the marker.
    assert!(!cfg.paths().backup_path().exists());
    assert!(!cfg.paths().signed_out_marker().exists());
}
