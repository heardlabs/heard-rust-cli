//! Subcommand bodies. Each returns `CliResult<()>` and prints its own output;
//! the console calls the same setting functions in [`crate::settings`].

pub mod daemon;
pub mod install;

use std::time::Duration;

use clap::CommandFactory;
use serde_json::{json, Map, Value};

use crate::cli::{Cli, Command, ConfigCmd, ModelsCmd};
use crate::client::{self, Agent};
use crate::doctor;
use crate::history;
use crate::models;
use crate::pick;
use crate::settings::{self, Ctx};
use crate::setup;
use crate::ui::{self, CliError, CliResult};
use crate::voices;

/// How long `start` waits for the socket.
pub const START_WAIT: Duration = Duration::from_secs(5);

fn joined(v: Vec<String>) -> Option<String> {
    let s = v.join(" ");
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Run one subcommand.
pub fn dispatch(ctx: &Ctx, cmd: Command) -> CliResult<()> {
    if let Some(r) = porcelain(ctx, &cmd) {
        return r;
    }
    match cmd {
        Command::Setup => setup::run(ctx),
        Command::Mode { value, .. } => mode(ctx, joined(value)),
        Command::Voice {
            name,
            preview,
            list,
            json,
            ..
        } => voice(ctx, joined(name), preview, list, json),
        Command::Persona { value, .. } => persona(ctx, joined(value)),
        Command::Speed { value, .. } => speed(ctx, joined(value)),
        Command::Alerts { value, .. } => alerts(ctx, joined(value)),
        Command::Pause { .. } => out(settings::set_paused(ctx, true)),
        Command::Resume { .. } => out(settings::set_paused(ctx, false)),
        Command::Mute(a) => out(settings::set_muted(ctx, true, a.session.as_deref())),
        Command::Unmute(a) => out(settings::set_muted(ctx, false, a.session.as_deref())),
        Command::Say { text, .. } => say(ctx, &say_text(&text)),
        Command::Agents { json } => agents(ctx, json),
        Command::History { since, limit, json } => history_cmd(ctx, since, limit, json),
        Command::Status { json, .. } => status(ctx, json),
        Command::Start => out(daemon::start(ctx, START_WAIT)),
        Command::Stop => out(daemon::stop(ctx, START_WAIT)),
        Command::Restart => {
            println!("{}", daemon::stop(ctx, START_WAIT)?);
            out(daemon::start(ctx, START_WAIT))
        }
        Command::Install { target, force } => out(install::install(&ctx.paths, target, force)),
        Command::Uninstall { target, purge } => out(install::uninstall(ctx, target, purge)),
        Command::Models { action } => models_cmd(ctx, action),
        Command::Doctor { json } => doctor_cmd(ctx, json),
        Command::Config { action } => config_cmd(ctx, action),
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "heard", &mut std::io::stdout());
            Ok(())
        }
        Command::Daemon {
            foreground,
            speech,
            tts,
            notifier,
        } => daemon::run_with(
            &ctx.paths,
            daemon::parse_options(
                foreground,
                &daemon::flag_or_env(speech, daemon::SPEECH_ENV, "queued"),
                &daemon::flag_or_env(tts, daemon::TTS_ENV, "auto"),
                &daemon::flag_or_env(notifier, daemon::NOTIFIER_ENV, "osascript"),
            )?,
        ),
    }
}

/// `say` words, minus a `--porcelain` that landed after the text.
fn say_text(words: &[String]) -> String {
    words
        .iter()
        .filter(|w| w.as_str() != "--porcelain")
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `--porcelain`: for the hook. Exactly one plain line on stdout, success or
/// failure, nothing on stderr, and no pickers. The exit code is unchanged.
fn porcelain(ctx: &Ctx, cmd: &Command) -> Option<CliResult<()>> {
    let v = |x: &Vec<String>| joined(x.clone());
    let r: CliResult<String> = match cmd {
        Command::Mode {
            value,
            porcelain: true,
        } => match v(value) {
            Some(x) => settings::parse_mode(&x).and_then(|m| settings::set_mode(ctx, m)),
            None => ctx
                .load()
                .map(|c| format!("mode: {}", settings::current_mode(&c))),
        },
        Command::Voice {
            name,
            porcelain: true,
            preview,
            ..
        } => match v(name) {
            Some(x) => settings::parse_voice(&x).and_then(|id| {
                let m = settings::set_voice(ctx, id)?;
                if *preview {
                    settings::preview_voice(ctx, id)?;
                }
                Ok(m)
            }),
            None => ctx
                .load()
                .map(|c| format!("voice: {}", settings::effective_voice(&ctx.paths, &c))),
        },
        Command::Persona {
            value,
            porcelain: true,
        } => match v(value) {
            Some(x) => settings::set_persona(ctx, &x),
            None => ctx
                .load()
                .map(|c| format!("persona: {}", settings::current_persona(&c))),
        },
        Command::Speed {
            value,
            porcelain: true,
        } => match v(value) {
            Some(x) => settings::parse_speed(&x).and_then(|f| settings::set_speed(ctx, f)),
            None => ctx.load().map(|c| {
                format!(
                    "speed: {}",
                    settings::fmt_speed(settings::current_speed(&c))
                )
            }),
        },
        Command::Alerts {
            value,
            porcelain: true,
        } => match v(value) {
            Some(x) => settings::parse_alerts(&x).and_then(|a| settings::set_alerts(ctx, a)),
            None => Ok(format!(
                "alerts: {}",
                settings::current_alerts(&ctx.paths).display()
            )),
        },
        Command::Pause { porcelain: true } => settings::set_paused(ctx, true),
        Command::Resume { porcelain: true } => settings::set_paused(ctx, false),
        Command::Mute(a) if a.porcelain => settings::set_muted(ctx, true, a.session.as_deref()),
        Command::Unmute(a) if a.porcelain => settings::set_muted(ctx, false, a.session.as_deref()),
        Command::Say { text, porcelain }
            if *porcelain || text.iter().any(|w| w == "--porcelain") =>
        {
            let t = say_text(text);
            say(ctx, &t).map(|()| format!("said: {t}"))
        }
        Command::Status {
            porcelain: true, ..
        } => porcelain_status(ctx),
        _ => return None,
    };
    Some(match r {
        Ok(line) => {
            println!("{}", one_line(&line));
            Ok(())
        }
        Err(e) => {
            let mut line = format!("error: {}", e.message);
            if let Some(f) = &e.fix {
                line.push_str(&format!(" (fix: {f})"));
            }
            println!("{}", one_line(&line));
            Err(CliError::silent(e.code))
        }
    })
}

/// First line, without the trailing `  (daemon …)` note.
fn one_line(s: &str) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    first
        .split("  (")
        .next()
        .unwrap_or(first)
        .trim()
        .to_string()
}

fn porcelain_status(ctx: &Ctx) -> CliResult<String> {
    let s = settings::snapshot(ctx)?;
    let state = if s.paused {
        "paused"
    } else if s.audio_off {
        "muted"
    } else {
        "on"
    };
    Ok(format!(
        "daemon {} · {} · {} ({}) · {} · alerts: {} · narration {state}",
        if ctx.daemon.is_up() {
            "running"
        } else {
            "stopped"
        },
        s.mode,
        s.persona,
        s.voice,
        settings::fmt_speed(s.speed),
        settings::Alerts::parse(&s.alerts)
            .map(|a| a.display())
            .unwrap_or("?"),
    ))
}

fn out(r: CliResult<String>) -> CliResult<()> {
    println!("{}", r?);
    Ok(())
}

fn mode(ctx: &Ctx, value: Option<String>) -> CliResult<()> {
    let m = match value {
        Some(v) => settings::parse_mode(&v)?,
        None => {
            let cur = settings::current_mode(&ctx.load()?);
            if !ui::interactive() {
                println!("{cur}");
                return Ok(());
            }
            match pick::mode(&cur)? {
                Some(m) => m,
                None => return Ok(()),
            }
        }
    };
    out(settings::set_mode(ctx, m))
}

fn voice(ctx: &Ctx, name: Option<String>, preview: bool, list: bool, json: bool) -> CliResult<()> {
    let cur = settings::effective_voice(&ctx.paths, &ctx.load()?);
    if list {
        if json {
            let v: Vec<Value> = voices::picker_order()
                .into_iter()
                .map(|id| {
                    json!({"id": id, "label": voices::label(id), "english": voices::is_english(id), "current": id == cur})
                })
                .collect();
            ui::print_json(&Value::Array(v));
        } else {
            for id in voices::picker_order() {
                let mark = if id == cur { "*" } else { " " };
                println!("{mark} {id:<14} {}", voices::label(id));
            }
        }
        return Ok(());
    }
    let id = match name {
        Some(n) => settings::parse_voice(&n)?.to_string(),
        None if preview => cur.clone(),
        None => {
            if !ui::interactive() {
                println!("{cur}");
                return Ok(());
            }
            match pick::voice(&cur)? {
                Some(v) => v,
                None => return Ok(()),
            }
        }
    };
    if id != cur {
        println!("{}", settings::set_voice(ctx, &id)?);
    }
    if preview {
        println!("{}", settings::preview_voice(ctx, &id)?);
    } else if id == cur {
        println!("voice: {id} — {} (unchanged)", voices::label(&id));
    }
    Ok(())
}

fn persona(ctx: &Ctx, value: Option<String>) -> CliResult<()> {
    let v = match value {
        Some(v) => v,
        None => {
            let cur = settings::current_persona(&ctx.load()?);
            if !ui::interactive() {
                println!("{cur}");
                return Ok(());
            }
            match pick::persona(ctx, &cur)? {
                Some(p) => p,
                None => return Ok(()),
            }
        }
    };
    out(settings::set_persona(ctx, &v))
}

fn speed(ctx: &Ctx, value: Option<String>) -> CliResult<()> {
    let f = match value {
        Some(v) => settings::parse_speed(&v)?,
        None => {
            let cur = settings::current_speed(&ctx.load()?);
            if !ui::interactive() {
                println!("{cur}");
                return Ok(());
            }
            match pick::speed(cur)? {
                Some(f) => f,
                None => return Ok(()),
            }
        }
    };
    out(settings::set_speed(ctx, f))
}

fn alerts(ctx: &Ctx, value: Option<String>) -> CliResult<()> {
    let a = match value {
        Some(v) => settings::parse_alerts(&v)?,
        None => {
            let cur = settings::current_alerts(&ctx.paths);
            if !ui::interactive() {
                println!("{}", cur.key());
                return Ok(());
            }
            match pick::alerts(cur)? {
                Some(a) => a,
                None => return Ok(()),
            }
        }
    };
    out(settings::set_alerts(ctx, a))
}

/// `heard say` and console plain text.
pub fn say(ctx: &Ctx, text: &str) -> CliResult<()> {
    let text = text.trim();
    if text.is_empty() {
        return Err(CliError::usage("nothing to say", "heard say \"some text\""));
    }
    ctx.daemon.speak(text)?;
    Ok(())
}

/// The daemon's agents, or `None` when it is down.
pub fn agent_list(ctx: &Ctx) -> Option<Vec<Agent>> {
    ctx.daemon.status().map(|s| client::active_sessions(&s))
}

fn agents(ctx: &Ctx, json: bool) -> CliResult<()> {
    let list = agent_list(ctx);
    if json {
        ui::print_json(&json!({
            "daemon_running": list.is_some(),
            "agents": list.clone().unwrap_or_default(),
        }));
        return Ok(());
    }
    match list {
        None => {
            println!("no agents: the daemon is not running");
            eprintln!(
                "  {} {}",
                ui::paint_err(nu_ansi_term::Color::Yellow.normal(), "fix:"),
                client::START_FIX
            );
        }
        Some(l) if l.is_empty() => println!("no agent sessions yet — start Claude Code or Codex"),
        Some(l) => {
            for a in l {
                let ago = a
                    .last_event_ago_s
                    .map(|s| format!("{}s ago", s.round() as i64))
                    .unwrap_or_default();
                let pin = if a.pinned { " (pinned)" } else { "" };
                println!("{:<40} {ago}{pin}", a.label());
            }
        }
    }
    Ok(())
}

fn history_cmd(ctx: &Ctx, since: Option<String>, limit: usize, json: bool) -> CliResult<()> {
    let since_secs = match since {
        None => None,
        Some(s) => Some(history::parse_duration(&s).ok_or_else(|| {
            CliError::usage(
                format!("cannot read --since {s:?}"),
                "use a number and a unit: 30s, 5m, 2h, 1d",
            )
        })?),
    };
    let recs = history::filter(
        history::read_all(&crate::paths::history_path(&ctx.paths)),
        since_secs,
        limit,
    );
    if json {
        ui::print_json(&Value::Array(recs.into_iter().map(Value::Object).collect()));
        return Ok(());
    }
    if recs.is_empty() {
        println!("nothing yet — Heard logs every line it speaks");
        return Ok(());
    }
    for r in &recs {
        println!("{}", history::format_line(r, true));
    }
    Ok(())
}

/// The `heard status --json` object.
pub fn status_json(ctx: &Ctx) -> CliResult<Value> {
    let snap = settings::snapshot(ctx)?;
    let st = ctx.daemon.status();
    let running = st.is_some() || ctx.daemon.is_up();
    let agents = st.as_ref().map(client::active_sessions).unwrap_or_default();
    let pid = std::fs::read_to_string(&ctx.paths.pid_path)
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());
    let model = models::Source::from_env()
        .map(|s| models::installed(&ctx.paths.models_dir, &s.files))
        .unwrap_or(false);
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "daemon": {
            "running": running,
            "socket": ctx.paths.socket_path,
            "pid": if running { pid.map(Value::from).unwrap_or(Value::Null) } else { Value::Null },
        },
        "mode": snap.mode,
        "persona": snap.persona,
        "voice": snap.voice,
        "speed": snap.speed,
        "alerts": snap.alerts,
        "paused": snap.paused,
        "audio_off": snap.audio_off,
        "model": { "installed": model, "dir": ctx.paths.models_dir },
        "agents": agents,
        "state_dir": ctx.paths.config_dir,
    }))
}

fn status(ctx: &Ctx, json: bool) -> CliResult<()> {
    let v = status_json(ctx)?;
    if json {
        ui::print_json(&v);
        return Ok(());
    }
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let row = |k: &str, val: String| println!("  {:<10} {val}", ui::dim(k));
    println!("{} {}", ui::bold("heard"), env!("CARGO_PKG_VERSION"));
    row(
        "daemon",
        if v["daemon"]["running"] == json!(true) {
            ui::green("running")
        } else {
            format!("{} — start it: heard start", ui::yellow("stopped"))
        },
    );
    row("mode", s("mode"));
    row("persona", s("persona"));
    row(
        "voice",
        format!("{} — {}", s("voice"), voices::label(&s("voice"))),
    );
    row(
        "speed",
        settings::fmt_speed(v["speed"].as_f64().unwrap_or(1.0)),
    );
    row(
        "alerts",
        settings::Alerts::parse(&s("alerts"))
            .map(|a| a.display().to_string())
            .unwrap_or_default(),
    );
    let narration = if v["paused"] == json!(true) {
        ui::yellow("paused — heard resume")
    } else if v["audio_off"] == json!(true) {
        ui::yellow("muted — heard unmute")
    } else {
        "on".to_string()
    };
    row("narration", narration);
    row(
        "model",
        if v["model"]["installed"] == json!(true) {
            "installed".to_string()
        } else {
            format!("{} — heard models download", ui::yellow("missing"))
        },
    );
    let agents = v["agents"].as_array().cloned().unwrap_or_default();
    if agents.is_empty() {
        row("agents", "none".into());
    } else {
        for (i, a) in agents.iter().enumerate() {
            let agent: Agent = Agent {
                session_id: a["session_id"].as_str().unwrap_or("").into(),
                repo_name: a["repo_name"].as_str().unwrap_or("").into(),
                last_event_ago_s: None,
                pinned: false,
            };
            row(if i == 0 { "agents" } else { "" }, agent.label());
        }
    }
    Ok(())
}

fn models_cmd(ctx: &Ctx, action: ModelsCmd) -> CliResult<()> {
    let src = models::Source::from_env()?;
    let dir = &ctx.paths.models_dir;
    match action {
        ModelsCmd::Download => {
            let total: u64 = src.files.iter().map(|f| f.size).sum();
            eprintln!(
                "Kokoro voice model ({}) → {}",
                models::human(total),
                dir.display()
            );
            let progress = std::io::IsTerminal::is_terminal(&std::io::stderr());
            for (name, o) in models::download(&src, dir, progress)? {
                match o {
                    models::Outcome::AlreadyPresent => {
                        println!("{name}: already installed and verified")
                    }
                    models::Outcome::Downloaded {
                        fetched,
                        resumed_from,
                    } => {
                        if resumed_from > 0 {
                            println!(
                                "{name}: resumed at {}, fetched {}, verified",
                                models::human(resumed_from),
                                models::human(fetched)
                            )
                        } else {
                            println!("{name}: downloaded {}, verified", models::human(fetched))
                        }
                    }
                }
            }
            Ok(())
        }
        ModelsCmd::Status { verify, json } => {
            let st = models::status(dir, &src.files, verify);
            let ok = st.iter().all(models::FileStatus::ok);
            if json {
                ui::print_json(&json!({
                    "dir": dir,
                    "installed": ok,
                    "verified": if verify { Value::from(st.iter().all(|s| s.verified == Some(true))) } else { Value::Null },
                    "files": st,
                }));
            } else {
                for s in &st {
                    let state = if !s.present {
                        if s.partial_bytes > 0 {
                            format!(
                                "{} (partial {}; `heard models download` resumes)",
                                ui::yellow("missing"),
                                models::human(s.partial_bytes)
                            )
                        } else {
                            ui::yellow("missing")
                        }
                    } else if s.size != s.expected_size {
                        ui::red(&format!("wrong size ({} of {})", s.size, s.expected_size))
                    } else {
                        match s.verified {
                            Some(true) => ui::green("verified"),
                            Some(false) => ui::red("SHA-256 mismatch"),
                            None => ui::green("installed"),
                        }
                    };
                    println!("{:<18} {state}", s.name);
                }
            }
            if ok {
                Ok(())
            } else {
                Err(CliError::failure(
                    "the voice model is not installed",
                    "run `heard models download`",
                ))
            }
        }
        ModelsCmd::Remove => {
            let gone = models::remove(dir, &src.files)?;
            if gone.is_empty() {
                println!("nothing to remove");
            }
            for p in gone {
                println!("removed {}", p.display());
            }
            Ok(())
        }
    }
}

fn doctor_cmd(ctx: &Ctx, json: bool) -> CliResult<()> {
    let checks = doctor::run(ctx);
    if json {
        ui::print_json(&doctor::to_json(&checks));
    } else {
        doctor::print(&checks);
    }
    if doctor::failed(&checks) {
        let n = checks
            .iter()
            .filter(|c| c.status == doctor::Level::Fail)
            .count();
        return Err(CliError::failure(
            format!("{n} check(s) failed"),
            "apply the fixes listed above, then re-run `heard doctor`",
        ));
    }
    Ok(())
}

// ── config ──────────────────────────────────────────────────────────────

const SECRET_SUFFIXES: [&str; 2] = ["_api_key", "_token"];

fn is_secret(k: &str) -> bool {
    SECRET_SUFFIXES.iter().any(|s| k.ends_with(s))
}

fn redact(v: &Value) -> Value {
    match v.as_str() {
        Some("") => json!(""),
        Some(s) => {
            let tail: String = s
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            json!(format!(
                "<redacted, {} chars, last 4: …{tail}>",
                s.chars().count()
            ))
        }
        None => v.clone(),
    }
}

fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `cli.py::_validate` — coerce and bounds-check a value by its key.
fn validate(key: &str, raw: &str) -> CliResult<Value> {
    let bad = |what: &str| {
        CliError::usage(
            format!("{key} {what}; got {raw:?}"),
            format!("`heard config get {key}` shows the current value"),
        )
    };
    match key {
        "verbosity" | "swarm_verbosity" => {
            let v = raw.to_ascii_lowercase();
            const OK: [&str; 6] = ["quiet", "brief", "normal", "verbose", "low", "high"];
            if OK.contains(&v.as_str()) {
                return Ok(json!(v));
            }
            return Err(bad("must be one of quiet, brief, normal, verbose"));
        }
        "skip_under_chars" | "flush_delay_ms" => {
            return raw
                .parse::<u64>()
                .map(|i| json!(i))
                .map_err(|_| bad("must be a whole number, 0 or more"));
        }
        _ => {}
    }
    let Some(default) = heard_config::defaults().get(key) else {
        return Err(CliError::usage(
            format!("unknown config key {key:?}"),
            "run `heard config list` to see the keys (only known keys are saved)",
        ));
    };
    Ok(match default {
        Value::Bool(_) => match raw.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => json!(true),
            "false" | "no" | "off" | "0" => json!(false),
            _ => return Err(bad("must be true or false")),
        },
        Value::Number(n) if n.is_f64() => {
            json!(raw.parse::<f64>().map_err(|_| bad("must be a number"))?)
        }
        Value::Number(_) => json!(raw
            .parse::<i64>()
            .map_err(|_| bad("must be a whole number"))?),
        Value::String(_) => json!(raw),
        _ => serde_json::from_str(raw).map_err(|_| bad("must be JSON"))?,
    })
}

fn config_cmd(ctx: &Ctx, action: ConfigCmd) -> CliResult<()> {
    match action {
        ConfigCmd::Path => {
            println!("{}", ctx.paths.config_path.display());
            Ok(())
        }
        ConfigCmd::Get { key, json } => {
            let v = if key == "alerts" {
                json!(settings::current_alerts(&ctx.paths).key())
            } else {
                ctx.load()?.get(&key).cloned().ok_or_else(|| {
                    CliError::failure(
                        format!("no config key {key:?}"),
                        "run `heard config list` to see the keys",
                    )
                })?
            };
            if json {
                println!("{v}");
            } else {
                println!("{}", plain(&v));
            }
            Ok(())
        }
        ConfigCmd::List { json, show_secrets } => {
            let cfg = ctx.load()?;
            let mut keys: Vec<&String> = cfg.keys().collect();
            keys.sort();
            let mut m = Map::new();
            for k in keys {
                let v = &cfg[k];
                let v = if is_secret(k) && !show_secrets {
                    redact(v)
                } else {
                    v.clone()
                };
                m.insert(k.clone(), v);
            }
            m.insert(
                "alerts".into(),
                json!(settings::current_alerts(&ctx.paths).key()),
            );
            if json {
                ui::print_json(&Value::Object(m));
            } else {
                for (k, v) in &m {
                    println!("{k} = {}", plain(v));
                }
            }
            Ok(())
        }
        ConfigCmd::Set { key, value } => {
            let msg = match key.as_str() {
                "mode" => settings::set_mode(ctx, settings::parse_mode(&value)?)?,
                "persona" => settings::set_persona(ctx, &value)?,
                "speed" => settings::set_speed(ctx, settings::parse_speed(&value)?)?,
                "kokoro_voice" if value.trim().is_empty() => settings::clear_voice(ctx)?,
                "kokoro_voice" => settings::set_voice(ctx, settings::parse_voice(&value)?)?,
                "alerts" => settings::set_alerts(ctx, settings::parse_alerts(&value)?)?,
                _ => {
                    let v = validate(&key, &value)?;
                    ctx.config.set_value(&key, v.clone())?;
                    format!("{key} = {}  ({})", plain(&v), ctx.reload_note())
                }
            };
            println!("{msg}");
            Ok(())
        }
    }
}
