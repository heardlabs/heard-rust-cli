//! Install, remove and inspect heard-cli's hooks in the agent CLIs.
//!
//! * **Claude Code** — hooks for `Stop`, `PreToolUse`, `PostToolUse` and
//!   `UserPromptSubmit` in `<home>/.claude/settings.json`, plus the
//!   `/heard-*` slash commands in `<home>/.claude/commands/`.
//! * **Codex CLI** — hooks for `Stop`, `PreToolUse` and `PostToolUse` in
//!   `<home>/.codex/hooks.json`. If `<home>/.codex/config.toml` turns hooks
//!   off, the install warns; it never edits that file.
//!
//! Every function takes the home directory explicitly, so nothing here ever
//! reaches for the real `~` on its own and tests run against a temp dir.
//!
//! ## What is ours
//!
//! The hook command is the absolute path of the installed `heard-hook`, the
//! agent name, and `--edition heard-cli`:
//!
//! ```text
//! /Users/me/.local/bin/heard-hook claude-code --edition heard-cli
//! ```
//!
//! The `--edition heard-cli` pair is the marker. Uninstall removes only hooks
//! that carry it; everything else in the file — other tools' hooks, unknown
//! keys, the user's own settings — is left as it was, in the order it was.
//! The paid Heard app's hooks (`python -m heard.hook …`, anything inside
//! `Heard.app`, or a `heard-hook` without our marker) are detected and
//! reported, and an install refuses to run beside them unless forced,
//! because both would narrate every event.
//!
//! ## How files are written
//!
//! Read under a lock, merge, and write only if something changed: a re-run
//! is a no-op that touches nothing. Before any change to an existing file,
//! a timestamped copy goes next to it (`settings.json.heard-cli-backup-…`).
//! The write itself is temp file + rename, two-space JSON with a trailing
//! newline, keys in their original order. A file that is not valid JSON, or
//! whose `hooks` is not the shape Claude Code reads, is refused — never
//! replaced.

#![forbid(unsafe_code)]

mod commands;
pub mod fsutil;
pub mod hooks;
pub mod json;
pub mod shell;

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Serialize;

use json::{Json, Obj};

pub use commands::{CommandFile, COMMAND_FILES, COMMAND_MARKER};
pub use fsutil::{FileNames, LockStyle, WriteStyle, BACKUP_INFIX};

/// The marker flag and value on every hook command this crate writes.
pub const EDITION_FLAG: &str = "--edition";
/// This edition's name — the value after [`EDITION_FLAG`].
pub const EDITION: &str = "heard-cli";

/// An agent CLI Heard can hook into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum Target {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "codex")]
    Codex,
}

impl Target {
    pub const ALL: [Target; 2] = [Target::ClaudeCode, Target::Codex];

    /// The name the hook binary takes as its first argument.
    pub fn as_str(self) -> &'static str {
        match self {
            Target::ClaudeCode => "claude-code",
            Target::Codex => "codex",
        }
    }

    /// The events Heard hooks for this agent.
    pub fn events(self) -> &'static [&'static str] {
        match self {
            Target::ClaudeCode => &["Stop", "PreToolUse", "PostToolUse", "UserPromptSubmit"],
            Target::Codex => &["Stop", "PreToolUse", "PostToolUse"],
        }
    }

    /// The hooks file for this agent under `home`.
    pub fn config_path(self, home: &Path) -> PathBuf {
        match self {
            Target::ClaudeCode => home.join(".claude").join("settings.json"),
            Target::Codex => home.join(".codex").join("hooks.json"),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Target {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "claude-code" | "claude" => Ok(Target::ClaudeCode),
            "codex" | "codex-cli" => Ok(Target::Codex),
            other => Err(format!(
                "unknown agent `{other}` (expected claude-code or codex)"
            )),
        }
    }
}

/// Everything that can stop an install or uninstall. Each message says what
/// to do next.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON ({detail}). Fix or move the file and run the install again; Heard never overwrites a file it cannot read.")]
    InvalidJson { path: PathBuf, detail: String },
    #[error("{path} has an unexpected shape ({detail}). Fix the file and run the install again; Heard never overwrites a file it cannot read.")]
    InvalidShape { path: PathBuf, detail: String },
    #[error("the paid Heard app's hooks are already in {path} ({}). Both would narrate every event. Turn the agent off in the Heard app first, or re-run with --force to install alongside it.", .commands.join("; "))]
    PaidAppHooks {
        path: PathBuf,
        commands: Vec<String>,
    },
    #[error("heard-hook was not found at {0} (or is not executable). Reinstall heard with install.sh, then run the install again.")]
    HookBinaryMissing(PathBuf),
    #[error("the heard-hook path must be absolute, got {0}")]
    HookBinaryNotAbsolute(PathBuf),
    #[error("{path} is held by another heard process. Wait a moment and try again (delete the file if no heard is running).")]
    Locked { path: PathBuf },
}

/// How to install.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Absolute path of the `heard-hook` binary the hooks will run.
    pub hook_bin: PathBuf,
    /// Install even beside the paid app's hooks, and overwrite same-named
    /// command files that are not ours.
    pub force: bool,
}

impl InstallOptions {
    pub fn new(hook_bin: impl Into<PathBuf>) -> Self {
        Self {
            hook_bin: hook_bin.into(),
            force: false,
        }
    }
}

/// What an install did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct InstallReport {
    pub config_path: PathBuf,
    /// Whether the hooks file was written. False on an idempotent re-run.
    pub changed: bool,
    /// The pre-change copy, when an existing file was changed.
    pub backup: Option<PathBuf>,
    /// Command files written or rewritten (Claude Code only).
    pub commands_written: Vec<PathBuf>,
    /// Human-readable warnings to print (paid app forced, codex hooks off,
    /// a same-named command file left alone).
    pub warnings: Vec<String>,
}

/// What an uninstall did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UninstallReport {
    pub config_path: PathBuf,
    pub changed: bool,
    pub backup: Option<PathBuf>,
    pub hooks_removed: usize,
    pub commands_removed: Vec<PathBuf>,
}

/// One slash-command file's state.
#[derive(Debug, Clone, Serialize)]
pub struct CommandFileStatus {
    pub name: String,
    pub path: PathBuf,
    pub present: bool,
    /// Present and carrying our marker.
    pub ours: bool,
    /// Ours and byte-identical to what this version would write.
    pub current: bool,
}

/// Everything `heard doctor` needs to say about one agent.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub agent: Target,
    pub config_path: PathBuf,
    pub config_exists: bool,
    /// The file exists but cannot be read or parsed; nothing else is known.
    pub config_error: Option<String>,
    /// Our hook is on every event this agent needs.
    pub installed: bool,
    /// Events with no hook of ours.
    pub missing_events: Vec<String>,
    /// Every hook of ours is exactly the command this install would write
    /// (the expected binary path, the agent, the marker). False when not
    /// installed, or when a stale/duplicated entry needs a re-install.
    pub marker_ok: bool,
    /// Our hook commands as found (deduplicated).
    pub hook_commands: Vec<String>,
    /// The `heard-hook` path our hooks run (first one found).
    pub hook_binary: Option<PathBuf>,
    /// That path exists and is executable.
    pub binary_ok: bool,
    /// Hooks belonging to the paid Heard app, as `Event: command`.
    pub paid_app_hooks: Vec<String>,
    /// Claude Code only; empty for Codex.
    pub command_files: Vec<CommandFileStatus>,
    /// Every command file is present, ours and current (true for Codex).
    pub command_files_ok: bool,
    /// Codex only: `config.toml` explicitly turns hooks off.
    pub codex_hooks_disabled: bool,
}

/// The hook command line this crate writes for `target`.
pub fn hook_command(hook_bin: &Path, target: Target) -> String {
    format!(
        "{} {} {EDITION_FLAG} {EDITION}",
        shell::quote(&hook_bin.to_string_lossy()),
        target.as_str()
    )
}

/// `heard-hook` beside the running executable — where install.sh puts it.
pub fn sibling_hook_bin() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("heard-hook"))
}

/// Who a hook command belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// Ours: `heard-hook … --edition heard-cli`.
    HeardCli,
    /// The paid Heard app.
    PaidApp,
    /// Anyone else.
    Foreign,
}

/// Classify one hook command string.
pub fn classify(command: &str) -> Owner {
    let words = shell::split(command);
    if let Some(words) = &words {
        let argv = shell::strip_env_prefix(words);
        let is_hook_bin = argv
            .first()
            .is_some_and(|w| Path::new(w).file_name().is_some_and(|n| n == "heard-hook"));
        let marked = argv
            .windows(2)
            .any(|p| p[0] == EDITION_FLAG && p[1] == EDITION)
            || argv
                .iter()
                .any(|w| *w == format!("{EDITION_FLAG}={EDITION}"));
        if is_hook_bin && marked {
            return Owner::HeardCli;
        }
    }
    let names_hook_bin = words
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|w| Path::new(w).file_name().is_some_and(|n| n == "heard-hook"));
    if command.contains("heard.hook") || command.contains("Heard.app") || names_hook_bin {
        Owner::PaidApp
    } else {
        Owner::Foreign
    }
}

// ---------------------------------------------------------------------------
// Reading and validating a hooks file

pub(crate) fn load(path: &Path) -> Result<(Option<String>, Json), InstallError> {
    let text = fsutil::read_opt(path)?;
    let doc = match &text {
        None => Json::Obj(Vec::new()),
        Some(t) if t.trim().is_empty() => Json::Obj(Vec::new()),
        Some(t) => Json::parse(t).map_err(|e| InstallError::InvalidJson {
            path: path.to_owned(),
            detail: e.to_string(),
        })?,
    };
    validate(&doc).map_err(|detail| InstallError::InvalidShape {
        path: path.to_owned(),
        detail,
    })?;
    Ok((text, doc))
}

/// The shape Claude Code and Codex both read: `hooks` is an object of
/// event → array of groups, each group an object with a `hooks` array of
/// objects. The same checks the Heard app makes.
pub(crate) fn validate(doc: &Json) -> Result<(), String> {
    let root = doc.as_obj().ok_or("the top level is not an object")?;
    let Some((_, hooks)) = root.iter().find(|(k, _)| k == "hooks") else {
        return Ok(());
    };
    let hooks = hooks.as_obj().ok_or("`hooks` is not an object")?;
    for (event, groups) in hooks {
        let groups = groups
            .as_arr()
            .ok_or_else(|| format!("`hooks.{event}` is not an array"))?;
        for g in groups {
            let inner = g
                .get("hooks")
                .and_then(Json::as_arr)
                .ok_or_else(|| format!("a `hooks.{event}` group has no `hooks` array"))?;
            if inner.iter().any(|h| h.as_obj().is_none()) {
                return Err(format!("a `hooks.{event}` hook is not an object"));
            }
        }
    }
    Ok(())
}

use hooks::all_commands;

fn paid_app_hooks(doc: &Json) -> Vec<String> {
    all_commands(doc)
        .into_iter()
        .filter(|(_, c)| classify(c) == Owner::PaidApp)
        .map(|(e, c)| format!("{e}: {c}"))
        .collect()
}

// ---------------------------------------------------------------------------
// The merge

/// The hook entry for one event. Claude Code's observe events run `async`
/// so they never hold the agent up; `UserPromptSubmit` must be synchronous
/// because its intercept answers with a `block` decision.
fn hook_entry(target: Target, event: &str, command: &str) -> Json {
    let mut e: Obj = vec![
        ("type".into(), Json::Str("command".into())),
        ("command".into(), Json::Str(command.into())),
    ];
    match (target, event) {
        (Target::ClaudeCode, "UserPromptSubmit") => {
            e.push(("timeout".into(), Json::Num(10.into())));
        }
        (Target::ClaudeCode, _) => e.push(("async".into(), Json::Bool(true))),
        (Target::Codex, _) => e.push(("timeout".into(), Json::Num(60.into()))),
    }
    Json::Obj(e)
}

fn is_cli_hook(command: &str) -> bool {
    classify(command) == Owner::HeardCli
}

fn strip_ours(doc: &mut Json, keep: &[(&str, Json)]) -> (usize, Vec<String>) {
    hooks::strip_ours(doc, &is_cli_hook, keep)
}

fn merge_install(doc: &mut Json, target: Target, command: &str) {
    let entry = |event: &str| hook_entry(target, event, command);
    hooks::merge_hooks(
        doc,
        &hooks::HookSpec {
            events: target.events(),
            command,
            is_ours: &is_cli_hook,
            entry: &entry,
            placement: hooks::Placement::OwnGroup,
        },
    );
}

/// Write `doc` to `path` if it differs from what was read, backing up an
/// existing file first. Returns (changed, backup).
fn commit(
    path: &Path,
    before_text: Option<&str>,
    before: &Json,
    after: &Json,
) -> Result<(bool, Option<PathBuf>), InstallError> {
    if before == after && before_text.is_some() {
        return Ok((false, None));
    }
    let backup = match before_text {
        Some(t) if !t.trim().is_empty() => Some(fsutil::backup(path)?),
        _ => None,
    };
    fsutil::atomic_write(path, after.to_pretty().as_bytes())?;
    Ok((true, backup))
}

// ---------------------------------------------------------------------------
// Public API

/// Install heard-cli's hooks (and, for Claude Code, its slash commands).
pub fn install(
    home: &Path,
    target: Target,
    opts: &InstallOptions,
) -> Result<InstallReport, InstallError> {
    if !opts.hook_bin.is_absolute() {
        return Err(InstallError::HookBinaryNotAbsolute(opts.hook_bin.clone()));
    }
    if !fsutil::is_executable(&opts.hook_bin) {
        return Err(InstallError::HookBinaryMissing(opts.hook_bin.clone()));
    }
    let config_path = target.config_path(home);
    let path = fsutil::resolve_target(&config_path);
    let mut report = InstallReport {
        config_path: config_path.clone(),
        ..Default::default()
    };

    {
        let _lock = fsutil::Lock::acquire(&path)?;
        let (text, doc) = load(&path)?;
        let paid = paid_app_hooks(&doc);
        if !paid.is_empty() {
            if !opts.force {
                return Err(InstallError::PaidAppHooks {
                    path: config_path,
                    commands: paid,
                });
            }
            report.warnings.push(format!(
                "the paid Heard app's hooks are also in {} — every event will be narrated twice until one is removed",
                config_path.display()
            ));
        }
        let mut after = doc.clone();
        merge_install(&mut after, target, &hook_command(&opts.hook_bin, target));
        let (changed, backup) = commit(&path, text.as_deref(), &doc, &after)?;
        report.changed = changed;
        report.backup = backup;
    }

    match target {
        Target::ClaudeCode => {
            let (written, warnings) = commands::write_all(home, opts.force)?;
            report.commands_written = written;
            report.warnings.extend(warnings);
        }
        Target::Codex => {
            if codex_hooks_disabled(home) {
                report.warnings.push(format!(
                    "Codex hooks are turned off in {}. Remove `hooks = false` under [features] (or set `hooks = true`) so Codex runs them; Heard does not edit that file.",
                    home.join(".codex/config.toml").display()
                ));
            }
        }
    }
    Ok(report)
}

/// Remove heard-cli's hooks (and command files). Everything else stays.
/// A missing file is not an error.
pub fn uninstall(home: &Path, target: Target) -> Result<UninstallReport, InstallError> {
    let config_path = target.config_path(home);
    let path = fsutil::resolve_target(&config_path);
    let mut report = UninstallReport {
        config_path,
        ..Default::default()
    };
    if path.exists() {
        let _lock = fsutil::Lock::acquire(&path)?;
        let (text, doc) = load(&path)?;
        let mut after = doc.clone();
        let (removed, _) = strip_ours(&mut after, &[]);
        report.hooks_removed = removed;
        if removed > 0 {
            let (changed, backup) = commit(&path, text.as_deref(), &doc, &after)?;
            report.changed = changed;
            report.backup = backup;
        }
    }
    if target == Target::ClaudeCode {
        report.commands_removed = commands::remove_all(home)?;
    }
    Ok(report)
}

/// Inspect one agent without changing anything. `expected_hook_bin` is the
/// `heard-hook` this install would write (usually [`sibling_hook_bin`]);
/// with `None`, `marker_ok` only requires our hooks to agree with each other.
pub fn status(home: &Path, target: Target, expected_hook_bin: Option<&Path>) -> Status {
    let config_path = target.config_path(home);
    let path = fsutil::resolve_target(&config_path);
    let mut st = Status {
        agent: target,
        config_path,
        config_exists: path.exists(),
        config_error: None,
        installed: false,
        missing_events: Vec::new(),
        marker_ok: false,
        hook_commands: Vec::new(),
        hook_binary: None,
        binary_ok: false,
        paid_app_hooks: Vec::new(),
        command_files: Vec::new(),
        command_files_ok: true,
        codex_hooks_disabled: target == Target::Codex && codex_hooks_disabled(home),
    };
    if target == Target::ClaudeCode {
        st.command_files = commands::status_all(home);
        st.command_files_ok = st.command_files.iter().all(|c| c.current);
    }
    let doc = match load(&path) {
        Ok((_, d)) => d,
        Err(e) => {
            st.config_error = Some(e.to_string());
            st.missing_events = target.events().iter().map(|e| e.to_string()).collect();
            return st;
        }
    };
    st.paid_app_hooks = paid_app_hooks(&doc);

    let ours: Vec<(String, String)> = all_commands(&doc)
        .into_iter()
        .filter(|(_, c)| classify(c) == Owner::HeardCli)
        .collect();
    for event in target.events() {
        if !ours.iter().any(|(e, _)| e == event) {
            st.missing_events.push(event.to_string());
        }
    }
    st.installed = st.missing_events.is_empty();
    for (_, c) in &ours {
        if !st.hook_commands.contains(c) {
            st.hook_commands.push(c.clone());
        }
    }
    st.hook_binary = st.hook_commands.first().and_then(|c| {
        let words = shell::split(c)?;
        shell::strip_env_prefix(&words).first().map(PathBuf::from)
    });
    st.binary_ok = st.hook_binary.as_deref().is_some_and(fsutil::is_executable);

    // Exactly one wanted entry per event, and nothing of ours elsewhere.
    let expected_cmd = match expected_hook_bin {
        Some(bin) => Some(hook_command(bin, target)),
        None => st.hook_commands.first().cloned(),
    };
    st.marker_ok = st.installed
        && st.hook_commands.len() == 1
        && expected_cmd.as_deref() == st.hook_commands.first().map(String::as_str)
        && ours.len() == target.events().len()
        && {
            let mut probe = doc.clone();
            let wanted: Vec<(&str, Json)> = target
                .events()
                .iter()
                .map(|e| {
                    (
                        *e,
                        hook_entry(target, e, expected_cmd.as_deref().unwrap_or("")),
                    )
                })
                .collect();
            let (removed, kept) = strip_ours(&mut probe, &wanted);
            removed == 0 && kept.len() == wanted.len()
        };
    st
}

/// True iff `<home>/.codex/config.toml` explicitly sets `hooks = false` (or
/// the older `codex_hooks = false`) under `[features]`. A line scan, not a
/// TOML parser: the common spellings, nothing exotic.
pub fn codex_hooks_disabled(home: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(home.join(".codex").join("config.toml")) else {
        return false;
    };
    let mut table = String::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            table = line
                .trim_matches(|c| c == '[' || c == ']')
                .trim()
                .to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim().trim_matches('"'), value.trim());
        let full = if table.is_empty() {
            key.to_owned()
        } else {
            format!("{table}.{key}")
        };
        if value == "false" && (full == "features.hooks" || full == "features.codex_hooks") {
            return true;
        }
    }
    false
}
