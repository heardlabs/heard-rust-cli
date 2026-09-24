//! The interactive console: `heard` with no arguments.
//!
//! A header (status + agents), a live feed tailing `history.jsonl` printed
//! ABOVE the prompt through reedline's external printer (so it never garbles
//! the line being typed), and a prompt where `/` opens a completion menu of
//! commands with one-line descriptions. Tab completes. A command given
//! without its argument opens an arrow-key picker. Plain text is spoken.
//! No full-screen TUI: it works in any terminal and over SSH.

mod complete;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reedline::{
    default_emacs_keybindings, ColumnarMenu, EditCommand, Emacs, ExternalPrinter, KeyCode,
    KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    ReedlineEvent, ReedlineMenu, Signal,
};

use crate::cli::ConfigCmd;
use crate::commands::{self, daemon, install};
use crate::history;
use crate::paths;
use crate::pick;
use crate::settings::{self, Ctx};
use crate::setup;
use crate::ui::{self, CliError, CliResult};

pub use complete::{Cmd, COMMANDS};

/// Env: set to `1` to skip starting the daemon when the console opens.
pub const NO_AUTOSTART_ENV: &str = "HEARD_NO_AUTOSTART";

struct HeardPrompt;

impl Prompt for HeardPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        "".into()
    }
    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
        "".into()
    }
    fn render_prompt_indicator(&self, _m: PromptEditMode) -> std::borrow::Cow<'_, str> {
        "> ".into()
    }
    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
        "… ".into()
    }
    fn render_prompt_history_search_indicator(
        &self,
        h: PromptHistorySearch,
    ) -> std::borrow::Cow<'_, str> {
        format!("(search: {}) ", h.term).into()
    }
}

/// The status line: ` heard · co-pilot · jarvis (bm_george) · 1.05× · alerts: voice+notify`.
pub fn status_line(ctx: &Ctx) -> String {
    let dot = ui::dim(" · ");
    let (running, snap) = (ctx.daemon.is_up(), settings::snapshot(ctx));
    let mut s = format!(" {}", ui::bold("heard"));
    match snap {
        Ok(sn) => {
            s.push_str(&format!(
                "{dot}{}{dot}{} ({}){dot}{}{dot}alerts: {}",
                ui::cyan(&sn.mode),
                sn.persona,
                sn.voice,
                settings::fmt_speed(sn.speed),
                settings::Alerts::parse(&sn.alerts)
                    .map(|a| a.display())
                    .unwrap_or("?"),
            ));
            if sn.paused {
                s.push_str(&format!("{dot}{}", ui::yellow("paused")));
            } else if sn.audio_off {
                s.push_str(&format!("{dot}{}", ui::yellow("muted")));
            }
        }
        Err(e) => s.push_str(&format!("{dot}{}", ui::red(&format!("config error: {e}")))),
    }
    s.push_str(&format!(
        "{dot}{}",
        if running {
            ui::green("● daemon")
        } else {
            ui::yellow("○ daemon stopped")
        }
    ));
    s
}

/// The agents line.
pub fn agents_line(ctx: &Ctx) -> String {
    match commands::agent_list(ctx) {
        None => format!(
            " {} {}",
            ui::dim("agents:"),
            ui::dim("— (daemon not running: /doctor)")
        ),
        Some(l) if l.is_empty() => format!(" {} none yet", ui::dim("agents:")),
        Some(l) => format!(
            " {} {}",
            ui::dim("agents:"),
            l.iter().map(|a| a.label()).collect::<Vec<_>>().join("   ")
        ),
    }
}

fn rule() -> String {
    let w = reedline_width().clamp(20, 100);
    ui::dim(&format!(" {}", "─".repeat(w - 2)))
}

fn reedline_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(72)
}

fn header(ctx: &Ctx) {
    println!("{}", status_line(ctx));
    println!("{}", agents_line(ctx));
    println!("{}", rule());
}

/// Run the console.
pub fn run(ctx: &Ctx) -> CliResult<()> {
    if !ui::interactive() {
        return Err(CliError::usage(
            "`heard` with no command opens the console, which needs a terminal",
            "run a subcommand instead (see `heard --help`), e.g. `heard status`",
        ));
    }
    if !ctx.daemon.is_up() && std::env::var(NO_AUTOSTART_ENV).as_deref() != Ok("1") {
        // Best effort: the header says whether it came up.
        let _ = daemon::start(ctx, Duration::from_millis(1500));
    }
    header(ctx);
    let tips = format!(
        " {}",
        ui::dim("type / for commands · plain text is spoken · Ctrl-D quits")
    );
    println!("{tips}");

    let printer: ExternalPrinter<String> = ExternalPrinter::default();
    let stop = Arc::new(AtomicBool::new(false));
    let feed = {
        let sender = printer.sender();
        let stop = stop.clone();
        let path = paths::history_path(&ctx.paths);
        std::thread::spawn(move || {
            let mut tail = history::Tail::from_end(path);
            while !stop.load(Ordering::Relaxed) {
                for r in tail.poll() {
                    if sender.send(feed_line(&r)).is_err() {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        })
    };

    let mut editor = build_editor(ctx, printer);
    let result = loop {
        let sig = match editor.read_line(&HeardPrompt) {
            Ok(s) => s,
            Err(e) => {
                break Err(CliError::failure(
                    format!("terminal error: {e}"),
                    "use the subcommands instead (`heard --help`)",
                ))
            }
        };
        match sig {
            Signal::Success(line) => match handle(ctx, line.trim()) {
                Ok(Flow::Continue) => {}
                Ok(Flow::Quit) => break Ok(()),
                Err(e) => e.report(),
            },
            Signal::CtrlC => {}
            Signal::CtrlD => break Ok(()),
            _ => {}
        }
    };
    stop.store(true, Ordering::Relaxed);
    let _ = feed.join();
    result
}

fn feed_line(r: &history::Record) -> String {
    format!(
        " {}  {:<14} {}",
        ui::dim(&history::local_time(history::field(r, "ts"), false)),
        ui::cyan(&history::source_label(r)),
        history::field(r, "spoken").trim()
    )
}

fn build_editor(ctx: &Ctx, printer: ExternalPrinter<String>) -> Reedline {
    let menu = ColumnarMenu::default()
        .with_name("completion_menu")
        .with_columns(1);
    let mut kb = default_emacs_keybindings();
    kb.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".into()),
            ReedlineEvent::MenuNext,
        ]),
    );
    kb.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    // `/` types itself and opens the command menu, as in Claude Code. The
    // completer only offers commands when the line starts with `/`, so a
    // slash inside spoken text opens nothing.
    kb.add_binding(
        KeyModifiers::NONE,
        KeyCode::Char('/'),
        ReedlineEvent::Multiple(vec![
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('/')]),
            ReedlineEvent::Menu("completion_menu".into()),
        ]),
    );
    Reedline::create()
        .with_completer(Box::new(complete::Completer::new(ctx.clone())))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_edit_mode(Box::new(Emacs::new(kb)))
        .with_external_printer(printer)
}

/// What the loop does next.
#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    /// Keep reading.
    Continue,
    /// Leave.
    Quit,
}

fn say_line(r: CliResult<String>) -> CliResult<Flow> {
    println!(" {}", r?);
    Ok(Flow::Continue)
}

/// Handle one console line.
pub fn handle(ctx: &Ctx, line: &str) -> CliResult<Flow> {
    if line.is_empty() {
        return Ok(Flow::Continue);
    }
    let Some(rest) = line.strip_prefix('/') else {
        commands::say(ctx, line)?;
        println!(" {}", ui::dim("(spoken)"));
        return Ok(Flow::Continue);
    };
    let (cmd, arg) = match rest.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (rest, ""),
    };
    let arg = (!arg.is_empty()).then_some(arg);
    match cmd {
        "mode" => {
            let m = match arg {
                Some(a) => settings::parse_mode(a)?,
                None => match pick::mode(&settings::current_mode(&ctx.load()?))? {
                    Some(m) => m,
                    None => return Ok(Flow::Continue),
                },
            };
            say_line(settings::set_mode(ctx, m))
        }
        "voice" => {
            let id = match arg {
                Some(a) => settings::parse_voice(a)?.to_string(),
                None => match pick::voice(&settings::effective_voice(&ctx.paths, &ctx.load()?))? {
                    Some(v) => v,
                    None => return Ok(Flow::Continue),
                },
            };
            println!(" {}", settings::set_voice(ctx, &id)?);
            if ctx.daemon.is_up() {
                say_line(settings::preview_voice(ctx, &id))
            } else {
                println!(
                    " {}",
                    ui::dim("(start the daemon for a spoken preview: /doctor)")
                );
                Ok(Flow::Continue)
            }
        }
        "persona" => {
            let p = match arg {
                Some(a) => a.to_string(),
                None => match pick::persona(ctx, &settings::current_persona(&ctx.load()?))? {
                    Some(p) => p,
                    None => return Ok(Flow::Continue),
                },
            };
            say_line(settings::set_persona(ctx, &p))
        }
        "speed" => {
            let f = match arg {
                Some(a) => settings::parse_speed(a)?,
                None => match pick::speed(settings::current_speed(&ctx.load()?))? {
                    Some(f) => f,
                    None => return Ok(Flow::Continue),
                },
            };
            say_line(settings::set_speed(ctx, f))
        }
        "alerts" => {
            let a = match arg {
                Some(a) => settings::parse_alerts(a)?,
                None => match pick::alerts(settings::current_alerts(&ctx.paths))? {
                    Some(a) => a,
                    None => return Ok(Flow::Continue),
                },
            };
            say_line(settings::set_alerts(ctx, a))
        }
        "pause" => say_line(settings::set_paused(ctx, true)),
        "resume" => say_line(settings::set_paused(ctx, false)),
        "mute" | "unmute" => {
            let mute = cmd == "mute";
            let session = match arg {
                Some(a) if a != "all" => Some(a.to_string()),
                Some(_) => None,
                None => match pick_session(ctx, mute)? {
                    Some(choice) => choice,
                    None => return Ok(Flow::Continue),
                },
            };
            say_line(settings::set_muted(ctx, mute, session.as_deref()))
        }
        "say" => match arg {
            Some(t) => {
                commands::say(ctx, t)?;
                Ok(Flow::Continue)
            }
            None => Err(CliError::usage("nothing to say", "/say some text")),
        },
        "agents" => {
            println!("{}", agents_line(ctx));
            Ok(Flow::Continue)
        }
        "status" => {
            println!("{}", status_line(ctx));
            println!("{}", agents_line(ctx));
            Ok(Flow::Continue)
        }
        "history" => {
            let n = match arg {
                Some(a) => a
                    .parse::<usize>()
                    .map_err(|_| CliError::usage(format!("not a count: {a:?}"), "/history 20"))?,
                None => 10,
            };
            let recs =
                history::filter(history::read_all(&paths::history_path(&ctx.paths)), None, n);
            if recs.is_empty() {
                println!(" nothing yet");
            }
            for r in &recs {
                println!(" {}", history::format_line(r, false));
            }
            Ok(Flow::Continue)
        }
        "doctor" => {
            let checks = crate::doctor::run(ctx);
            crate::doctor::print(&checks);
            Ok(Flow::Continue)
        }
        "setup" => {
            setup::run(ctx)?;
            Ok(Flow::Continue)
        }
        "install" | "uninstall" => {
            let target = match arg.unwrap_or("all") {
                "claude" | "claude-code" => install::Target::ClaudeCode,
                "codex" | "codex-cli" => install::Target::Codex,
                "all" => install::Target::All,
                other => {
                    return Err(CliError::usage(
                        format!("unknown agent {other:?}"),
                        format!("/{cmd} claude-code | codex | all"),
                    ))
                }
            };
            if cmd == "install" {
                say_line(install::install(&ctx.paths, target, false))
            } else {
                say_line(install::uninstall(ctx, target, false))
            }
        }
        "config" => {
            let words: Vec<&str> = arg.unwrap_or("list").split_whitespace().collect();
            let action = match words.as_slice() {
                ["list"] => ConfigCmd::List {
                    json: false,
                    show_secrets: false,
                },
                ["path"] => ConfigCmd::Path,
                ["get", k] => ConfigCmd::Get {
                    key: k.to_string(),
                    json: false,
                },
                ["set", k, v @ ..] if !v.is_empty() => ConfigCmd::Set {
                    key: k.to_string(),
                    value: v.join(" "),
                },
                _ => {
                    return Err(CliError::usage(
                        "unknown /config form",
                        "/config list | path | get <key> | set <key> <value>",
                    ))
                }
            };
            commands::dispatch(ctx, crate::cli::Command::Config { action })?;
            Ok(Flow::Continue)
        }
        "help" | "?" => {
            for c in COMMANDS {
                println!(" {:<11} {:<26} {}", c.name, ui::dim(c.args), c.about);
            }
            println!(" {}", ui::dim("plain text (no /) is spoken"));
            Ok(Flow::Continue)
        }
        "quit" | "exit" | "q" => Ok(Flow::Quit),
        other => Err(CliError::usage(
            format!("unknown command /{other}"),
            "type / to see the commands, or /help",
        )),
    }
}

/// `None` = cancelled; `Some(None)` = all speech; `Some(Some(id))` = one session.
fn pick_session(ctx: &Ctx, mute: bool) -> CliResult<Option<Option<String>>> {
    let agents = commands::agent_list(ctx).unwrap_or_default();
    let all = if mute {
        "all speech (every agent)"
    } else {
        "all speech"
    };
    let mut items = vec![("all".to_string(), all.to_string())];
    items.extend(
        agents
            .iter()
            .map(|a| (a.session_id.clone(), format!("only {}", a.label()))),
    );
    if items.len() == 1 {
        return Ok(Some(None));
    }
    Ok(
        pick::choose(if mute { "Mute" } else { "Unmute" }, &items, "all")?
            .map(|s| (s != "all").then_some(s)),
    )
}
