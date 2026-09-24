//! `heard doctor`: each check says ok / warn / fail and, when not ok, the fix.
//! Exit 1 when any check fails; warnings do not fail.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;

use crate::commands::install::{self, HookState};
use crate::models;
use crate::settings::Ctx;
use crate::ui;

/// A check's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Fine.
    Ok,
    /// Worth knowing; not broken.
    Warn,
    /// Broken.
    Fail,
}

/// One check.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// Short id.
    pub name: &'static str,
    /// Verdict.
    pub status: Level,
    /// What was found.
    pub detail: String,
    /// What to do (None when ok).
    pub fix: Option<String>,
}

fn check(name: &'static str, status: Level, detail: String, fix: Option<String>) -> Check {
    Check {
        name,
        status,
        detail,
        fix,
    }
}

/// Run every check.
pub fn run(ctx: &Ctx) -> Vec<Check> {
    let mut out = Vec::new();

    // Config readable.
    out.push(match ctx.config.load(None) {
        Ok(_) => check(
            "config",
            Level::Ok,
            format!("{}", ctx.paths.config_path.display()),
            None,
        ),
        Err(e) => check(
            "config",
            Level::Fail,
            format!("cannot load config: {e}"),
            Some(format!(
                "fix or move aside {}",
                ctx.paths.config_path.display()
            )),
        ),
    });

    // Model present and verified.
    out.push(match models::Source::from_env() {
        Err(e) => check("model", Level::Fail, e.message, e.fix),
        Ok(src) => {
            let st = models::status(&ctx.paths.models_dir, &src.files, true);
            let bad: Vec<String> = st
                .iter()
                .filter(|s| !s.ok())
                .map(|s| {
                    if !s.present {
                        format!("{} missing", s.name)
                    } else if s.size != s.expected_size {
                        format!("{} is {} of {} bytes", s.name, s.size, s.expected_size)
                    } else {
                        format!("{} fails its SHA-256 check", s.name)
                    }
                })
                .collect();
            if bad.is_empty() {
                check(
                    "model",
                    Level::Ok,
                    format!(
                        "Kokoro model verified in {}",
                        ctx.paths.models_dir.display()
                    ),
                    None,
                )
            } else {
                check(
                    "model",
                    Level::Fail,
                    bad.join("; "),
                    Some("run `heard models download`".into()),
                )
            }
        }
    });

    // Daemon socket.
    out.push(if ctx.daemon.is_up() {
        check(
            "daemon",
            Level::Ok,
            format!("listening on {}", ctx.paths.socket_path.display()),
            None,
        )
    } else if ctx.paths.socket_path.exists() {
        check(
            "daemon",
            Level::Warn,
            format!(
                "stale socket at {} (nothing listening)",
                ctx.paths.socket_path.display()
            ),
            Some("run `heard restart`".into()),
        )
    } else {
        check(
            "daemon",
            Level::Warn,
            "not running (it starts on the first agent event)".into(),
            Some("run `heard start` to start it now".into()),
        )
    });

    // Hooks.
    for (target, state) in install::hook_status(&ctx.paths) {
        let name = match target {
            install::Target::ClaudeCode => "hooks:claude-code",
            install::Target::Codex => "hooks:codex",
            install::Target::All => "hooks",
        };
        out.push(match state {
            HookState::Installed => check(name, Level::Ok, "installed".into(), None),
            HookState::Missing => check(
                name,
                Level::Warn,
                "not installed".into(),
                Some(format!("run `heard install {}`", target.name())),
            ),
            HookState::Unknown => check(
                name,
                Level::Warn,
                "hook check not wired yet in this build".into(),
                Some("check the agent's hook config by hand".into()),
            ),
        });
    }

    // afplay.
    out.push(if cfg!(target_os = "macos") {
        match which("afplay") {
            Some(p) => check("afplay", Level::Ok, p.display().to_string(), None),
            None => check(
                "afplay",
                Level::Fail,
                "afplay not found on PATH".into(),
                Some("afplay ships with macOS in /usr/bin; add /usr/bin to PATH".into()),
            ),
        }
    } else {
        check(
            "afplay",
            Level::Warn,
            "not macOS: audio playback is not supported yet".into(),
            Some("run Heard on macOS 13 or newer".into()),
        )
    });

    // A different `heard` earlier on PATH.
    out.push(shadow_check());

    out
}

/// The first executable named `prog` on PATH.
pub fn which(prog: &str) -> Option<PathBuf> {
    which_all(prog).into_iter().next()
}

fn which_all(prog: &str) -> Vec<PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    std::env::split_paths(&path)
        .map(|d| d.join(prog))
        .filter(|p| is_executable(p))
        .collect()
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn shadow_check() -> Check {
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    let found = which_all("heard");
    let first = found.first().and_then(|p| p.canonicalize().ok());
    match (me, first) {
        (Some(me), Some(first)) if me != first => check(
            "path",
            Level::Fail,
            format!(
                "`heard` on PATH is {}, not this binary ({})",
                found[0].display(),
                me.display()
            ),
            Some(format!(
                "remove the other one (the old Python Heard?) or put {} earlier on PATH",
                me.parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )),
        ),
        (Some(me), None) => check(
            "path",
            Level::Warn,
            "`heard` is not on PATH, so hooks cannot start it".into(),
            Some(format!(
                "add {} to PATH (e.g. in ~/.zshrc)",
                me.parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )),
        ),
        _ => check(
            "path",
            Level::Ok,
            "this `heard` is first on PATH".into(),
            None,
        ),
    }
}

/// Print checks for people.
pub fn print(checks: &[Check]) {
    for c in checks {
        let tag = match c.status {
            Level::Ok => ui::green("  ok "),
            Level::Warn => ui::yellow("warn "),
            Level::Fail => ui::red("FAIL "),
        };
        println!("{tag} {:<18} {}", c.name, c.detail);
        if let Some(f) = &c.fix {
            if c.status != Level::Ok {
                println!("      {:<18} {} {f}", "", ui::dim("fix:"));
            }
        }
    }
}

/// `{"ok":bool,"checks":[…]}`.
pub fn to_json(checks: &[Check]) -> serde_json::Value {
    json!({
        "ok": checks.iter().all(|c| c.status != Level::Fail),
        "checks": checks,
    })
}

/// Any failure?
pub fn failed(checks: &[Check]) -> bool {
    checks.iter().any(|c| c.status == Level::Fail)
}
