//! Claude Code slash commands: `<home>/.claude/commands/heard-*.md`.
//!
//! Each file surfaces as `/heard-mode focus` and so on. The normal path never
//! runs them: `heard-hook`'s `UserPromptSubmit` intercept sees the prompt
//! first and answers it with a `block` decision, zero tokens. If Claude Code
//! expands the command before the hook sees it, the body is the fallback: it
//! asks the model to run `heard <verb> --porcelain` with the arguments
//! single-quoted, and echo the one line it printed — one small turn.
//!
//! The arguments are never spliced into a shell line (no `!` expansion): a
//! prompt like `/heard-say $(rm -rf ~)` reaches the model as text, and the
//! only command it is allowed to run without asking is `heard …`
//! (`allowed-tools: Bash(heard:*)`), so a chained or substituted command
//! still needs the user's approval.
//!
//! Every file carries [`COMMAND_MARKER`]; uninstall removes only files that
//! have it, and install never overwrites a same-named file without it
//! (unless forced).

use std::fs;
use std::path::{Path, PathBuf};

use crate::fsutil::{self, io_err};
use crate::{CommandFileStatus, InstallError};

/// The line that marks a command file as heard-cli's.
pub const COMMAND_MARKER: &str = "<!-- heard-cli:managed";

/// One slash command.
#[derive(Debug, Clone, Copy)]
pub struct CommandFile {
    /// The `heard` subcommand it runs; the file is `heard-<verb>.md`.
    pub verb: &'static str,
    pub description: &'static str,
    /// Claude Code's `argument-hint`, empty for none.
    pub argument_hint: &'static str,
}

/// The command files `heard install claude-code` writes.
pub const COMMAND_FILES: &[CommandFile] = &[
    CommandFile {
        verb: "mode",
        description: "Set Heard's narration mode (copilot, companion or focus)",
        argument_hint: "[copilot|companion|focus]",
    },
    CommandFile {
        verb: "voice",
        description: "Set Heard's voice",
        argument_hint: "[voice name]",
    },
    CommandFile {
        verb: "persona",
        description: "Set Heard's persona",
        argument_hint: "[persona name or file]",
    },
    CommandFile {
        verb: "speed",
        description: "Set Heard's speaking speed",
        argument_hint: "[0.5-2.0]",
    },
    CommandFile {
        verb: "pause",
        description: "Pause Heard's narration",
        argument_hint: "",
    },
    CommandFile {
        verb: "resume",
        description: "Resume Heard's narration",
        argument_hint: "",
    },
    CommandFile {
        verb: "status",
        description: "Show Heard's status in one line",
        argument_hint: "",
    },
];

impl CommandFile {
    pub fn file_name(&self) -> String {
        format!("heard-{}.md", self.verb)
    }

    /// The exact file contents.
    pub fn render(&self) -> String {
        let hint = if self.argument_hint.is_empty() {
            String::new()
        } else {
            format!("argument-hint: \"{}\"\n", self.argument_hint)
        };
        format!(
            "---\n\
             description: {desc}\n\
             {hint}\
             allowed-tools: Bash(heard:*)\n\
             ---\n\
             {COMMAND_MARKER}: written by `heard install claude-code`, removed by `heard uninstall claude-code` -->\n\
             \n\
             Run exactly one Bash command and nothing else: `heard {verb} --porcelain`, followed by each word of the arguments below wrapped in single quotes (write a single quote inside a word as '\\''). With no arguments, run `heard {verb} --porcelain` alone. Never run the arguments themselves. Then reply with only the one line it printed.\n\
             \n\
             Arguments: $ARGUMENTS\n",
            desc = self.description,
            verb = self.verb,
        )
    }
}

fn dir(home: &Path) -> PathBuf {
    home.join(".claude").join("commands")
}

fn is_ours(text: &str) -> bool {
    text.contains(COMMAND_MARKER)
}

/// Write every command file. Returns (files written, warnings).
pub(crate) fn write_all(
    home: &Path,
    force: bool,
) -> Result<(Vec<PathBuf>, Vec<String>), InstallError> {
    let dir = dir(home);
    fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
    let mut written = Vec::new();
    let mut warnings = Vec::new();
    for cf in COMMAND_FILES {
        let path = dir.join(cf.file_name());
        let want = cf.render();
        match fsutil::read_opt(&path)? {
            Some(have) if have == want => continue,
            Some(have) if !is_ours(&have) && !force => {
                warnings.push(format!(
                    "{} exists and is not Heard's; left it alone (use --force to replace it)",
                    path.display()
                ));
                continue;
            }
            _ => {}
        }
        fsutil::atomic_write(&path, want.as_bytes())?;
        written.push(path);
    }
    Ok((written, warnings))
}

/// Remove every command file that carries our marker.
pub(crate) fn remove_all(home: &Path) -> Result<Vec<PathBuf>, InstallError> {
    let mut removed = Vec::new();
    for cf in COMMAND_FILES {
        let path = dir(home).join(cf.file_name());
        if fsutil::read_opt(&path)?.is_some_and(|t| is_ours(&t)) {
            fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
            removed.push(path);
        }
    }
    Ok(removed)
}

pub(crate) fn status_all(home: &Path) -> Vec<CommandFileStatus> {
    COMMAND_FILES
        .iter()
        .map(|cf| {
            let path = dir(home).join(cf.file_name());
            let text = fs::read_to_string(&path).ok();
            CommandFileStatus {
                name: cf.file_name(),
                present: text.is_some(),
                ours: text.as_deref().is_some_and(is_ours),
                current: text.as_deref() == Some(cf.render().as_str()),
                path,
            }
        })
        .collect()
}
