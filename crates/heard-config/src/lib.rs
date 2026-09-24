//! `heard-config` — the Rust port of `engine/heard/config.py`.
//!
//! Layered config, lowest priority first:
//!
//! 1. [`defaults::defaults`] — the core `DEFAULTS`, in this crate, plus every
//!    layer registered with [`register_layer`] (see [`all_defaults`]).
//! 2. The global user config at `CONFIG_DIR/config.yaml`.
//! 3. The per-project `.heard.yaml`, walking up from the event's cwd.
//!
//! # Registered layers
//!
//! The core declares only the keys the core reads. An edition that reads more
//! (the full app) calls [`register_layer`] with its own defaults, and
//! optionally [`register_critical_keys`] and [`register_save_hook`], ONCE at
//! startup and before its first load or save. From then on the strict
//! [`Config::save`] keeps those keys exactly as it keeps the core's, and
//! still drops a key nobody declared.
//!
//! [`Config::load`] does all three layers in one call, exactly as Python's
//! `load(cwd=…)` does.
//!
//! # What is different in shape (but not in behaviour)
//!
//! Python keeps the paths as module constants and the test suite monkeypatches
//! them. There is no module state here, so the constants become a [`Paths`]
//! value and every operation hangs off a [`Config`] that owns one. That is the
//! whole structural difference: `Config::new(Paths::under(tmp))` is the Rust
//! spelling of `conftest.py`'s isolation fixture.
//!
//! Failures stay values. A missing file is an empty layer; a corrupt GLOBAL
//! file is an empty layer plus a stderr line plus a rename to
//! `config.yaml.broken-<ts>`; a corrupt PROJECT file is an empty layer and
//! nothing else, because that file lives in the user's own repo.
//!
//! # Parity
//!
//! `fixtures/config/*.json` is the golden corpus, generated from the live
//! Python module and cut down to the core's keys. `tests/fixtures.rs` replays
//! it here.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod defaults;
pub mod error;
pub mod legacy;
pub mod paths;
pub mod pyyaml;
mod yaml;

pub use defaults::{defaults, APP, CLI_APP, PROJECT_FILE};
pub use error::{ConfigError, Result};
pub use paths::{PathEnv, Paths};

use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Keys always written to `config.yaml`, even when equal to their default
/// (`config._ALWAYS_PERSIST`).
///
/// Without this an explicitly-chosen value that happens to land on the default
/// (`narration_skill = "default"`) stops being written, and a later save can
/// silently revert the choice.
pub const ALWAYS_PERSIST: [&str; 7] = [
    "narration_skill",
    "narrate_routine",
    "mode",
    "onboarded",
    "first_run_generation",
    "first_run_completed_generation",
    "first_run_attempt",
];

/// Keys a save must never silently drop (`config.CRITICAL_KEYS`), the core's
/// share of them. A registered layer adds its own with
/// [`register_critical_keys`]; [`critical_keys`] is the merged list.
///
/// A writer whose in-memory config lacks one of these while the file HAS it
/// is stale or defaulted, so the disk value is merged back rather than lost.
/// Any key whose loss reverts an explicit user choice belongs here.
pub const CRITICAL_KEYS: [&str; 7] = [
    "onboarded",
    "first_run_generation",
    "first_run_completed_generation",
    "first_run_attempt",
    "persona",
    "voice",
    "elevenlabs_api_key",
];

/// Called after every successful [`Config::save`] with the config, the
/// mapping that was written and the exact YAML body. For a registered layer
/// that keeps a side file in step with `config.yaml` (a backup, a marker).
/// Failures inside the hook are the hook's own business: the save has
/// already succeeded.
pub type SaveHook = fn(&Config, &Map<String, Value>, &str);

#[derive(Default)]
struct Layers {
    defaults: Vec<&'static Map<String, Value>>,
    critical_keys: Vec<&'static str>,
    save_hooks: Vec<SaveHook>,
    merged: Option<Arc<Map<String, Value>>>,
}

fn layers() -> &'static RwLock<Layers> {
    static LAYERS: OnceLock<RwLock<Layers>> = OnceLock::new();
    LAYERS.get_or_init(|| RwLock::new(Layers::default()))
}

/// Add a layer of defaults above the core [`defaults()`].
///
/// Call once per layer at startup, BEFORE the first [`Config::load`] or
/// [`Config::save`]: a save before registration drops the layer's keys as
/// undeclared. A key in both a layer and the core takes the layer's value;
/// between layers, the later registration wins.
pub fn register_layer(defaults: &'static Map<String, Value>) {
    let mut l = layers().write().unwrap_or_else(|e| e.into_inner());
    l.defaults.push(defaults);
    l.merged = None;
}

/// Add keys to the save's wipe guard (see [`CRITICAL_KEYS`]).
pub fn register_critical_keys(keys: &'static [&'static str]) {
    let mut l = layers().write().unwrap_or_else(|e| e.into_inner());
    for k in keys {
        if !l.critical_keys.contains(k) && !CRITICAL_KEYS.contains(k) {
            l.critical_keys.push(k);
        }
    }
}

/// Run `hook` after every successful save (see [`SaveHook`]).
pub fn register_save_hook(hook: SaveHook) {
    layers()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .save_hooks
        .push(hook);
}

/// The merged defaults: the core [`defaults()`] with every registered layer
/// on top. What [`Config::load`] starts from and what the strict
/// [`Config::save`] treats as declared.
pub fn all_defaults() -> Arc<Map<String, Value>> {
    if let Some(m) = &layers().read().unwrap_or_else(|e| e.into_inner()).merged {
        return Arc::clone(m);
    }
    let mut l = layers().write().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = &l.merged {
        return Arc::clone(m);
    }
    let mut m = defaults().clone();
    for layer in &l.defaults {
        m.extend(layer.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    let m = Arc::new(m);
    l.merged = Some(Arc::clone(&m));
    m
}

/// The wipe guard's keys: [`CRITICAL_KEYS`], then every registered one.
pub fn critical_keys() -> Vec<&'static str> {
    let l = layers().read().unwrap_or_else(|e| e.into_inner());
    CRITICAL_KEYS
        .iter()
        .copied()
        .chain(l.critical_keys.iter().copied())
        .collect()
}

fn save_hooks() -> Vec<SaveHook> {
    layers()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .save_hooks
        .clone()
}

/// `app.language` → (human name for the narration directive, TTS lang code)
/// (`config._NARRATION_LANGS`).
///
/// English is the default and returns `None` (no directive, no TTS override) so
/// the byte-stable system block is unchanged for the common case. Dictation is
/// NOT affected — Parakeet v3 auto-detects the spoken language regardless.
pub const NARRATION_LANGS: [(&str, &str, &str); 2] = [
    ("zh", "Simplified Chinese (简体中文)", "zh"),
    ("es", "Spanish", "es"),
];

/// The config module, bound to one set of paths.
#[derive(Debug, Clone)]
pub struct Config {
    paths: Paths,
}

impl Config {
    /// Bind the module to a set of paths.
    pub fn new(paths: Paths) -> Self {
        Config { paths }
    }

    /// Bind to the paths the running process resolves (`Paths::from_env`).
    pub fn from_env() -> Result<Self> {
        Ok(Config::new(Paths::from_env()?))
    }

    /// The path constants this instance reads and writes.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// `config.ensure_dirs()`.
    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [
            &self.paths.config_dir,
            &self.paths.data_dir,
            &self.paths.models_dir,
        ] {
            fs::create_dir_all(d).map_err(|e| ConfigError::io(d, e))?;
        }
        Ok(())
    }

    // ── reading ──────────────────────────────────────────────────────────

    /// `config._read_yaml(path)` — a YAML file as a mapping, `{}` on
    /// missing-file OR parse-error.
    ///
    /// The parse-error case is the load-bearing one. A corrupt `config.yaml`
    /// (test pollution writing into the prod path, a crash mid-write, a
    /// hand-edit with mismatched brackets) used to crash whoever called
    /// `load()`, which bricked the whole app launch — the daemon and the UI
    /// both call it at startup. Now: log it, rename the broken file to
    /// `<name>.broken-<ts>` so the next read succeeds with defaults, and return
    /// an empty layer.
    ///
    /// The auto-rename fires only for the GLOBAL config path. Per-project
    /// `.heard.yaml` files live in the user's own repos; those are left alone
    /// and the override layer is simply absent until the user fixes the file.
    ///
    /// (Python also fires a best-effort `notify()` on the rename. That is a
    /// `notify.py` concern and arrives with the module that owns it.)
    fn read_yaml(&self, path: &Path) -> Result<Map<String, Value>> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(e) => return Err(ConfigError::io(path, e)),
        };
        match yaml::parse(&text) {
            Ok(parsed) => yaml::mapping_or_error(parsed, path),
            Err(e) if e.kind == pyyaml::ErrorKind::Python => Err(ConfigError::Load {
                path: path.to_path_buf(),
                message: e.message,
            }),
            Err(e) => {
                eprintln!(
                    "config: failed to parse {} — falling back to defaults: {e}",
                    path.display()
                );
                if path == self.paths.config_path {
                    let backup = paths::append_suffix(path, &format!(".broken-{}", now_secs()));
                    match fs::rename(path, &backup) {
                        Ok(()) => eprintln!("config: moved broken file to {}", backup.display()),
                        Err(rename_err) => eprintln!("config: rename failed: {rename_err}"),
                    }
                }
                Ok(Map::new())
            }
        }
    }

    /// `config._read_disk_config()` — the raw global file, `{}` on ANY failure
    /// and on anything that is not a mapping. Deliberately does not rename or
    /// log: it is the wipe-guard's read, not a user-facing load.
    pub fn read_disk_config(&self) -> Map<String, Value> {
        read_mapping_file(&self.paths.config_path)
    }

    /// `config.load(cwd)` — defaults, then the global file, then the nearest
    /// `.heard.yaml` walking up from `cwd`.
    ///
    /// # Errors
    ///
    /// A layer whose `yaml.safe_load` raises something other than a
    /// `YAMLError` (`!!timestamp 2026-02-30` → `ValueError`) gives
    /// [`ConfigError::Load`]: `config._read` does not catch those either.
    ///
    /// A layer that parses into a TRUTHY non-mapping (`- a`, `hello`, `7`)
    /// gives [`ConfigError::NotAMapping`]. Python raises there too — it calls
    /// `dict.update` on the value and gets a `ValueError`/`TypeError` — so the
    /// "this does not load" outcome is the same, only the spelling differs.
    ///
    /// The ONE knowing divergence in this crate: a YAML sequence of pairs
    /// (`- [a, b]`) is a truthy non-mapping here, whereas `dict.update` happily
    /// reads it as `{"a": "b"}`. That is a `dict.update` accident, not designed
    /// behaviour, and the corpus marks the case `python_only`.
    pub fn load(&self, cwd: Option<&Path>) -> Result<Map<String, Value>> {
        let mut cfg = (*all_defaults()).clone();
        if self.paths.config_path.exists() {
            let layer = self.read_yaml(&self.paths.config_path)?;
            cfg.extend(layer);
        }
        if let Some(proj) = find_project_config(cwd) {
            let layer = self.read_yaml(&proj)?;
            cfg.extend(layer);
        }
        // SINGLE SOURCE OF TRUTH for who speaks. The "Narration voice" picker
        // writes `voice` with one of the four persona names (each persona ships
        // its own voice + style), but everything that speaks reads `persona`.
        // Derive persona from the pick HERE so every reader is correct at once
        // — otherwise the picker set voice while the speak path kept
        // persona=jarvis (default) → always Jarvis.
        let v = cfg
            .get("voice")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if legacy::PERSONA_VOICES.contains(&v.as_str()) {
            cfg.insert("persona".into(), Value::String(v));
        }
        Ok(cfg)
    }

    /// `config.project_label(cwd)` — the nearest `.heard.yaml`'s `label:`, or
    /// `""`.
    ///
    /// Reads ONLY the project file (not the global config) so a stray global
    /// `label` cannot leak in. It is a spoken narration nickname and
    /// local-only: it is NEVER sent to analytics — same distillation rule as
    /// project paths and file names. Keep it out of any `capture()` payload.
    pub fn project_label(&self, cwd: Option<&Path>) -> String {
        let Some(proj) = find_project_config(cwd) else {
            return String::new();
        };
        let data = self.read_yaml(&proj).unwrap_or_default();
        data.get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    }

    // ── writing ──────────────────────────────────────────────────────────

    /// `config.save(cfg)` — persist non-default values to the global user
    /// config file.
    ///
    /// Strict: only keys defined in `DEFAULTS` — the core's plus every
    /// registered layer's ([`all_defaults`]) — get written. Accepting any key
    /// let `apply_preset` leak persona-internal frontmatter (`name`,
    /// `address`) into `config.yaml`; the strict pass auto-cleans any such
    /// pollution the next time anything saves.
    ///
    /// `intentional_clear` is `save`'s `_intentional_clear`: the keys whose
    /// disappearance is deliberate (a sign-out), so the wipe guard leaves them
    /// alone.
    pub fn save(&self, cfg: &Map<String, Value>, intentional_clear: &[&str]) -> Result<()> {
        self.ensure_dirs()?;
        let defaults = all_defaults();
        // User-chosen narration settings must persist even when they equal the
        // default, otherwise a value that lands on its default stops being
        // written and a later save can silently revert an explicit choice.
        let mut user_cfg = Map::new();
        for (k, v) in cfg {
            let Some(d) = defaults.get(k) else { continue };
            if d != v || ALWAYS_PERSIST.contains(&k.as_str()) {
                user_cfg.insert(k.clone(), v.clone());
            }
        }
        // WIPE GUARD: never write a file that silently loses a critical key the
        // disk currently has.
        let disk = self.read_disk_config();
        for k in critical_keys() {
            if intentional_clear.contains(&k) {
                continue;
            }
            if user_cfg.contains_key(k) {
                continue;
            }
            // Python: `disk.get(k) not in (None, "", False)`. `==` comparison,
            // so 0 and 0.0 are excluded along with False.
            let Some(dv) = disk.get(k) else { continue };
            if is_none_empty_or_false(dv) {
                continue;
            }
            user_cfg.insert(k.to_string(), dv.clone());
            eprintln!("config: wipe-guard preserved {k:?} (writer state was missing it)");
        }
        // Atomic write. A plain truncate-and-write lets a CONCURRENT reader
        // (every process that shares this file) load an EMPTY file mid-write,
        // fall back to DEFAULTS and then persist those defaults, silently
        // wiping the user's settings. A rename is atomic on the same
        // filesystem, so a reader always sees either the old or the new
        // COMPLETE file, never a partial one.
        let body = yaml::dump(&user_cfg);
        atomic_write(&self.paths.config_path, &body)?;

        for hook in save_hooks() {
            hook(self, &user_cfg, &body);
        }
        Ok(())
    }

    /// `config.set_value(key, value)` — load, set one key, save, all under the
    /// cross-process lock.
    pub fn set_value(&self, key: &str, value: Value) -> Result<()> {
        let _guard = self.config_lock()?;
        let mut cfg = self.load(None)?;
        cfg.insert(key.to_string(), value);
        self.save(&cfg, &[])
    }

    /// `config.apply_preset(preset)` — merge a preset into the global config.
    pub fn apply_preset(&self, preset: &Map<String, Value>) -> Result<()> {
        let _guard = self.config_lock()?;
        let mut cfg = self.load(None)?;
        cfg.extend(preset.clone());
        self.save(&cfg, &[])
    }

    /// `config._config_lock()` — the cross-process advisory lock that
    /// serialises load-modify-save.
    ///
    /// Every writer (daemon `set_value`, voice serve, engine API) must hold it
    /// around the FULL read-modify-write, or two racing writers resurrect the
    /// 2026-08-11 wipe.
    pub fn config_lock(&self) -> Result<ConfigLock> {
        self.ensure_dirs()?;
        let path = self.paths.lock_path();
        let file = fs::File::create(&path).map_err(|e| ConfigError::io(&path, e))?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
            .map_err(|e| ConfigError::io(&path, std::io::Error::from(e)))?;
        Ok(ConfigLock { file })
    }

    // ── language ─────────────────────────────────────────────────────────

    /// `config.narration_language(cfg)` — `(name, tts_lang)` for a
    /// non-English `app.language`, else `None`.
    pub fn narration_language(cfg: &Map<String, Value>) -> Option<(&'static str, &'static str)> {
        let lang = cfg
            .get("app.language")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("system")
            .to_ascii_lowercase();
        NARRATION_LANGS
            .iter()
            .find(|(code, _, _)| *code == lang)
            .map(|(_, name, tts)| (*name, *tts))
    }

    /// `config.tts_lang_for(cfg)` — follow `app.language` when it is
    /// non-English, else the configured `lang` (default `en-us`).
    pub fn tts_lang_for(cfg: &Map<String, Value>) -> String {
        if let Some((_, tts)) = Self::narration_language(cfg) {
            return tts.to_string();
        }
        cfg.get("lang")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("en-us")
            .to_string()
    }
}

/// A held `.config.lock`. Released when dropped, like Python's context manager.
#[derive(Debug)]
pub struct ConfigLock {
    file: fs::File,
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

/// `config.find_project_config(start)` — walk up from `start` (a dir or a file)
/// looking for a `.heard.yaml`. `None` when `start` is `None`, matching Python:
/// no cwd means no project layer, never "use the process cwd".
pub fn find_project_config(start: Option<&Path>) -> Option<PathBuf> {
    let start = start?;
    let mut p = resolve(start);
    if p.is_file() {
        p = p.parent()?.to_path_buf();
    }
    for dir in p.ancestors() {
        let candidate = dir.join(PROJECT_FILE);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// `Path.resolve()` with `strict=False`: absolute, symlinks followed where they
/// exist, `.`/`..` folded away where they do not.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `value not in (None, "", False)`, inverted — Python's `==` semantics, so a
/// numeric zero counts as `False`.
fn is_none_empty_or_false(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !*b,
        Value::String(s) => s.is_empty(),
        Value::Number(n) => n.as_f64().map(|f| f == 0.0).unwrap_or(false),
        _ => false,
    }
}

/// A YAML file as a mapping: `{}` on ANY failure and on anything that is not
/// a mapping. No rename, no log — the wipe guard's read, public so a
/// registered layer can read a side file (a backup) the same way.
pub fn read_mapping_file(path: &Path) -> Map<String, Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Map::new();
    };
    match yaml::parse(&text) {
        Ok(yaml::Parsed::Mapping(m)) => m,
        _ => Map::new(),
    }
}

/// `tempfile.mkstemp` + write + `fsync` + `os.replace`: the atomic write
/// [`Config::save`] uses, public for a registered layer's side files.
///
/// # Errors
///
/// Any I/O failure creating, writing, syncing or renaming the temp file.
pub fn write_atomic(target: &Path, body: &str) -> Result<()> {
    atomic_write(target, body)
}

/// `tempfile.mkstemp` + write + `fsync` + `os.replace`.
fn atomic_write(target: &Path, body: &str) -> Result<()> {
    let dir = target.parent().unwrap_or(Path::new("."));
    let tmp = unique_temp(dir, ".config-");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| ConfigError::io(&tmp, e))?;
        let write = f
            .write_all(body.as_bytes())
            .and_then(|()| f.flush())
            .and_then(|()| f.sync_all());
        if let Err(e) = write {
            let _ = fs::remove_file(&tmp);
            return Err(ConfigError::io(&tmp, e));
        }
    }
    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(ConfigError::io(target, e));
    }
    Ok(())
}

fn unique_temp(dir: &Path, prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!("{prefix}{}-{nanos}.tmp", std::process::id()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
