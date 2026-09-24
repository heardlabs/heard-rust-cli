//! Registered config layers: an edition's own defaults on top of the core's.
//!
//! Its own test binary, because a layer is process-global: every test here
//! runs with [`LAYER`] registered, and no test in `fixtures.rs` ever sees it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, Once, OnceLock};

use heard_config::{
    all_defaults, critical_keys, defaults, pyyaml, register_critical_keys, register_layer,
    register_save_hook, Config, Paths,
};
use serde_json::{json, Map, Value};

fn layer() -> &'static Map<String, Value> {
    static LAYER: OnceLock<Map<String, Value>> = OnceLock::new();
    LAYER.get_or_init(|| {
        json!({
            "extra_feature_off": false,
            "extra_model": "model-a",
            "extra_account_token": "",
        })
        .as_object()
        .cloned()
        .unwrap()
    })
}

static LAYER_CRITICAL: &[&str] = &["extra_account_token"];

/// Every body a save wrote, in order, as the save hook saw it.
static SAVED: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());
static HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);

fn hook(cfg: &Config, written: &Map<String, Value>, body: &str) {
    HOOK_CALLS.fetch_add(1, Ordering::SeqCst);
    assert_eq!(pyyaml::safe_dump(&Value::Object(written.clone())), body);
    SAVED
        .lock()
        .unwrap()
        .push((cfg.paths().config_path.clone(), body.to_owned()));
}

fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        register_layer(layer());
        register_critical_keys(LAYER_CRITICAL);
        register_save_hook(hook);
    });
}

fn scratch(label: &str) -> Config {
    static N: AtomicU32 = AtomicU32::new(0);
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("heard-config-layers")
        .join(format!("{label}-{}", N.fetch_add(1, Ordering::Relaxed)));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let cfg = Config::new(Paths::under(&root));
    cfg.ensure_dirs().unwrap();
    cfg
}

fn on_disk(cfg: &Config) -> Value {
    pyyaml::safe_load(&fs::read_to_string(&cfg.paths().config_path).unwrap()).unwrap()
}

#[test]
fn the_merged_defaults_are_the_core_plus_the_layer() {
    setup();
    let all = all_defaults();
    for (k, v) in defaults() {
        assert_eq!(all.get(k), Some(v), "core key {k}");
    }
    for (k, v) in layer() {
        assert_eq!(all.get(k), Some(v), "layer key {k}");
        assert!(!defaults().contains_key(k), "{k} must not be a core key");
    }
    assert_eq!(all.len(), defaults().len() + layer().len());
}

#[test]
fn load_starts_from_the_layered_defaults() {
    setup();
    let cfg = scratch("load");
    let loaded = cfg.load(None).unwrap();
    assert_eq!(loaded["extra_model"], "model-a");
    assert_eq!(loaded["voice"], defaults()["voice"]);
}

#[test]
fn a_strict_save_keeps_layered_keys_and_drops_undeclared_ones() {
    setup();
    let cfg = scratch("strict");
    let mut body = Map::new();
    body.insert("extra_model".into(), json!("model-b")); // layered, non-default
    body.insert("extra_feature_off".into(), json!(false)); // layered, == default
    body.insert("voice".into(), json!("am_onyx")); // core, non-default
    body.insert("not_declared_anywhere".into(), json!("x")); // nobody's
    cfg.save(&body, &[]).unwrap();
    assert_eq!(
        on_disk(&cfg),
        json!({"extra_model": "model-b", "voice": "am_onyx"})
    );

    // A set_value round trip keeps the layered choice too.
    cfg.set_value("speed", json!(1.3)).unwrap();
    let disk = on_disk(&cfg);
    assert_eq!(disk["extra_model"], "model-b");
    assert_eq!(disk["speed"], 1.3);
    assert!(disk.get("not_declared_anywhere").is_none());
}

#[test]
fn a_layered_critical_key_survives_a_stale_writer() {
    setup();
    assert!(critical_keys().contains(&"extra_account_token"));
    assert!(critical_keys().contains(&"voice"));
    let cfg = scratch("wipe-guard");
    fs::write(
        &cfg.paths().config_path,
        "extra_account_token: tok\nvoice: am_onyx\n",
    )
    .unwrap();
    // A writer whose in-memory config has neither key.
    let mut body = Map::new();
    body.insert("mode".into(), json!("companion"));
    cfg.save(&body, &[]).unwrap();
    assert_eq!(
        on_disk(&cfg),
        json!({"extra_account_token": "tok", "mode": "companion", "voice": "am_onyx"})
    );
    // ...unless the loss is intentional.
    cfg.save(&body, &["extra_account_token", "voice"]).unwrap();
    assert_eq!(on_disk(&cfg), json!({"mode": "companion"}));
}

#[test]
fn the_save_hook_sees_every_successful_save() {
    setup();
    let cfg = scratch("hook");
    let before = HOOK_CALLS.load(Ordering::SeqCst);
    let mut body = Map::new();
    body.insert("extra_model".into(), json!("model-c"));
    cfg.save(&body, &[]).unwrap();
    assert!(HOOK_CALLS.load(Ordering::SeqCst) > before);
    let saved = SAVED.lock().unwrap();
    let (path, text) = saved
        .iter()
        .rev()
        .find(|(p, _)| *p == cfg.paths().config_path)
        .expect("the hook saw this save");
    assert_eq!(fs::read_to_string(path).unwrap(), *text);
    assert_eq!(text, "extra_model: model-c\n");
}
