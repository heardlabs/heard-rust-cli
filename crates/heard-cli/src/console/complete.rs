//! The `/` menu: commands with one-line descriptions, then per-command
//! argument completions.

use reedline::{CompletionResult, Span, Suggestion};

use crate::settings::{self, Alerts, Ctx, Mode};
use crate::voices;

/// One console command.
#[derive(Debug, Clone, Copy)]
pub struct Cmd {
    /// `/name`.
    pub name: &'static str,
    /// Argument hint.
    pub args: &'static str,
    /// One line.
    pub about: &'static str,
}

const fn c(name: &'static str, args: &'static str, about: &'static str) -> Cmd {
    Cmd { name, args, about }
}

/// Every console command, in menu order.
pub const COMMANDS: &[Cmd] = &[
    c("/mode", "[copilot|companion|focus]", "How much Heard says"),
    c(
        "/voice",
        "[name]",
        "Pick the Kokoro voice, with a spoken preview",
    ),
    c("/persona", "[name|file]", "Who narrates, and in what style"),
    c("/speed", "[0.5–2.0]", "Speaking rate"),
    c(
        "/alerts",
        "[voice|notify|both|off]",
        "How needs-you moments reach you",
    ),
    c("/pause", "", "Stop narrating until /resume"),
    c("/resume", "", "Start narrating again"),
    c(
        "/mute",
        "[agent|all]",
        "Silence speech, or one agent session",
    ),
    c("/unmute", "[agent|all]", "Undo /mute"),
    c("/say", "<text>", "Speak a line (plain text does this too)"),
    c("/agents", "", "Agent sessions Heard is following"),
    c("/history", "[count]", "What Heard said recently"),
    c(
        "/doctor",
        "",
        "Check the install; prints a fix for each problem",
    ),
    c("/setup", "", "Guided setup: model, voice, mode, hooks"),
    c(
        "/install",
        "[claude-code|codex|all]",
        "Add Heard's hooks to an agent",
    ),
    c(
        "/uninstall",
        "[claude-code|codex|all]",
        "Remove Heard's hooks",
    ),
    c(
        "/config",
        "[list|path|get|set]",
        "Read or change the raw config",
    ),
    c("/help", "", "List the commands"),
    c("/quit", "", "Leave the console (the daemon keeps running)"),
];

/// The completer; owns a context to list personas and agents.
pub struct Completer {
    ctx: Ctx,
}

impl Completer {
    /// New.
    pub fn new(ctx: Ctx) -> Self {
        Completer { ctx }
    }

    fn args_for(&self, cmd: &str) -> Vec<(String, String)> {
        match cmd {
            "/mode" => Mode::ALL
                .iter()
                .map(|m| (m.key().to_string(), m.describe().to_string()))
                .collect(),
            "/voice" => voices::picker_order()
                .into_iter()
                .map(|v| (v.to_string(), voices::label(v)))
                .collect(),
            "/persona" => settings::persona_names(&self.ctx.paths)
                .into_iter()
                .map(|p| {
                    let d = settings::persona_describe(&p).to_string();
                    (p, d)
                })
                .collect(),
            "/speed" => ["0.9", "1.0", "1.05", "1.1", "1.2", "1.3"]
                .iter()
                .map(|s| (s.to_string(), String::new()))
                .collect(),
            "/alerts" => Alerts::ALL
                .iter()
                .map(|a| (a.key().to_string(), a.describe().to_string()))
                .collect(),
            "/install" | "/uninstall" => [
                ("claude-code", "Claude Code"),
                ("codex", "Codex CLI"),
                ("all", "every agent found"),
            ]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
            "/config" => [
                ("list", "every value"),
                ("path", "where config.yaml lives"),
                ("get", "one value"),
                ("set", "change one value"),
            ]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
            "/mute" | "/unmute" => {
                let mut v = vec![("all".to_string(), "all speech".to_string())];
                if let Some(st) = self.ctx.daemon.status() {
                    for a in crate::client::active_sessions(&st) {
                        let short: String = a.session_id.chars().take(8).collect();
                        v.push((short, a.label()));
                    }
                }
                v
            }
            _ => Vec::new(),
        }
    }
}

/// Pure command-name completion (tested without a terminal).
pub fn command_suggestions(prefix: &str) -> Vec<&'static Cmd> {
    COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(prefix))
        .collect()
}

impl reedline::Completer for Completer {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let upto = &line[..pos.min(line.len())];
        if !upto.starts_with('/') {
            return CompletionResult::fresh(Vec::<Suggestion>::new());
        }
        let out: Vec<Suggestion> = match upto.split_once(' ') {
            None => command_suggestions(upto)
                .into_iter()
                .map(|c| Suggestion {
                    value: c.name.to_string(),
                    display_override: Some(format!("{:<11}", c.name)),
                    description: Some(if c.args.is_empty() {
                        c.about.to_string()
                    } else {
                        format!("{}  {}", c.about, c.args)
                    }),
                    span: Span::new(0, upto.len()),
                    append_whitespace: !c.args.is_empty(),
                    ..Suggestion::default()
                })
                .collect(),
            Some((cmd, rest)) => {
                // Only the first argument completes.
                if rest.contains(' ') && cmd != "/config" {
                    Vec::new()
                } else {
                    let word_start = upto.rfind(' ').map(|i| i + 1).unwrap_or(0);
                    let word = &upto[word_start..];
                    self.args_for(cmd)
                        .into_iter()
                        .filter(|(v, _)| v.starts_with(word))
                        .map(|(v, d)| Suggestion {
                            value: v,
                            description: (!d.is_empty()).then_some(d),
                            span: Span::new(word_start, upto.len()),
                            append_whitespace: false,
                            ..Suggestion::default()
                        })
                        .collect()
                }
            }
        };
        CompletionResult::fresh(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::Completer as _;

    fn ctx() -> (tempfile::TempDir, Ctx) {
        let d = tempfile::tempdir().unwrap();
        let c = Ctx::new(crate::paths::for_root(d.path()));
        (d, c)
    }

    fn values(r: CompletionResult) -> Vec<String> {
        r.suggestions().iter().map(|s| s.value.clone()).collect()
    }

    #[test]
    fn slash_lists_every_command_with_a_description() {
        let (_d, c) = ctx();
        let mut comp = Completer::new(c);
        let r = comp.complete("/", 1);
        assert_eq!(r.suggestions().len(), COMMANDS.len());
        assert!(r.suggestions().iter().all(|s| s.description.is_some()));
    }

    #[test]
    fn prefix_filters_and_args_complete() {
        let (_d, c) = ctx();
        let mut comp = Completer::new(c);
        assert_eq!(values(comp.complete("/mo", 3)), vec!["/mode"]);
        assert_eq!(values(comp.complete("/mode f", 7)), vec!["focus"]);
        assert!(values(comp.complete("/voice bm_", 10)).contains(&"bm_george".to_string()));
        assert!(values(comp.complete("hello /mo", 9)).is_empty());
    }
}
