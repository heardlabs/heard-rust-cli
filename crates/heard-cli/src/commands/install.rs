//! `heard install` / `heard uninstall` and the hook-status query.
//!

use std::path::PathBuf;

use heard_config::Paths;

use crate::ui::{CliError, CliResult};

/// Which agent CLI to wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Target {
    /// Claude Code (`~/.claude/settings.json` hooks + `/heard-*` commands).
    #[value(name = "claude-code", alias = "claude")]
    ClaudeCode,
    /// Codex CLI.
    #[value(name = "codex", alias = "codex-cli")]
    Codex,
    /// Every agent found.
    All,
}

impl Target {
    /// The name users type.
    pub fn name(self) -> &'static str {
        match self {
            Target::ClaudeCode => "claude-code",
            Target::Codex => "codex",
            Target::All => "all",
        }
    }
}

/// Whether Heard's hooks are in an agent's config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookState {
    /// Installed.
    Installed,
    /// Not installed.
    Missing,
    /// Unknown (the check is not wired yet).
    Unknown,
}

/// The home whose agent configs are edited. `HOME` (tests point it at a temp dir).
fn home() -> CliResult<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| CliError::failure("HOME is not set", "run from a normal login shell"))
}

fn lib_target(t: Target) -> Option<heard_install::Target> {
    match t {
        Target::ClaudeCode => Some(heard_install::Target::ClaudeCode),
        Target::Codex => Some(heard_install::Target::Codex),
        Target::All => None,
    }
}

/// `All` = the agents found on PATH (both, for uninstall).
fn expand(target: Target, for_install: bool) -> CliResult<Vec<heard_install::Target>> {
    if let Some(t) = lib_target(target) {
        return Ok(vec![t]);
    }
    if !for_install {
        return Ok(heard_install::Target::ALL.to_vec());
    }
    let found: Vec<_> = crate::setup::detected_agents()
        .into_iter()
        .filter_map(|(t, _)| lib_target(t))
        .collect();
    if found.is_empty() {
        return Err(CliError::failure(
            "no agent CLI found on PATH (looked for `claude` and `codex`)",
            "install one, or name it: `heard install claude-code`",
        ));
    }
    Ok(found)
}

fn hook_bin() -> CliResult<PathBuf> {
    if let Some(p) = std::env::var_os("HEARD_HOOK_BIN").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    heard_install::sibling_hook_bin().ok_or_else(|| {
        CliError::failure(
            "cannot locate heard-hook next to this binary",
            "reinstall with install.sh",
        )
    })
}

fn err(e: heard_install::InstallError) -> CliError {
    CliError::failure(e.to_string(), "see the message above")
}

/// Write Heard's hooks (and Claude Code's `/heard-*` commands) for `target`.
pub fn install(paths: &Paths, target: Target, force: bool) -> CliResult<String> {
    let home = home()?;
    let opts = heard_install::InstallOptions {
        hook_bin: hook_bin()?,
        force,
    };
    let mut lines = Vec::new();
    for t in expand(target, true)? {
        let r = heard_install::install(&home, t, &opts).map_err(err)?;
        let verb = if r.changed {
            "installed"
        } else {
            "already installed"
        };
        let mut line = format!("{t}: hooks {verb} in {}", r.config_path.display());
        if !r.commands_written.is_empty() {
            line.push_str(&format!(
                " · {} /heard-* commands",
                r.commands_written.len()
            ));
        }
        lines.push(line);
        lines.extend(r.warnings.iter().map(|w| format!("{t}: warning: {w}")));
    }
    mark_onboarded(paths)?;
    Ok(lines.join("\n"))
}

/// Write `onboarded: true` (see [`crate::edition`] for the first-run rule).
/// `heard install` and `heard setup` both do this, so a user who wires the
/// hooks is never held by the core's first-run gate.
pub fn mark_onboarded(paths: &Paths) -> CliResult<()> {
    crate::edition::register();
    heard_config::Config::new(paths.clone()).set_value("onboarded", serde_json::json!(true))?;
    Ok(())
}

/// Remove Heard's hooks for `target`; `purge` also removes models and state.
pub fn uninstall(ctx: &crate::settings::Ctx, target: Target, purge: bool) -> CliResult<String> {
    let home = home()?;
    let mut lines = Vec::new();
    for t in expand(target, false)? {
        let r = heard_install::uninstall(&home, t).map_err(err)?;
        lines.push(if r.changed {
            format!(
                "{t}: removed {} hook(s) from {}",
                r.hooks_removed,
                r.config_path.display()
            )
        } else {
            format!("{t}: nothing to remove")
        });
    }
    if purge {
        let root = crate::paths::state_root()?;
        // Only ever delete a directory that is plainly ours.
        let ours = root.file_name().is_some_and(|n| n == "heard-cli")
            || std::env::var_os("HEARD_CLI_HOME").is_some_and(|h| h == root.as_os_str());
        if !ours {
            return Err(CliError::failure(
                format!(
                    "refusing to purge {}: not a heard-cli state root",
                    root.display()
                ),
                "delete it by hand if you are sure",
            ));
        }
        let _ = crate::commands::daemon::stop(ctx, std::time::Duration::from_secs(3));
        if root.exists() {
            std::fs::remove_dir_all(&root).map_err(|e| {
                CliError::failure(format!("{}: {e}", root.display()), "delete it by hand")
            })?;
        }
        lines.push(format!("purged {}", root.display()));
    }
    Ok(lines.join("\n"))
}

/// Are Heard's hooks installed for each agent?
pub fn hook_status(_paths: &Paths) -> Vec<(Target, HookState)> {
    let Ok(home) = home() else {
        return vec![
            (Target::ClaudeCode, HookState::Unknown),
            (Target::Codex, HookState::Unknown),
        ];
    };
    let bin = hook_bin().ok();
    [
        (Target::ClaudeCode, heard_install::Target::ClaudeCode),
        (Target::Codex, heard_install::Target::Codex),
    ]
    .into_iter()
    .map(|(t, lt)| {
        let s = heard_install::status(&home, lt, bin.as_deref());
        let state = if s.installed && s.marker_ok {
            HookState::Installed
        } else {
            HookState::Missing
        };
        (t, state)
    })
    .collect()
}
