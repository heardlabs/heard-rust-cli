//! Arrow-key pickers (inquire), shared by subcommands, setup and the console.
//! `Ok(None)` means the user cancelled (Esc / Ctrl-C).

use inquire::{Confirm, InquireError, Select};

use crate::settings::{self, Alerts, Ctx, Mode};
use crate::ui::{CliError, CliResult};
use crate::voices;

fn cancelled<T>(r: Result<T, InquireError>) -> CliResult<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => Ok(None),
        Err(e) => Err(CliError::failure(
            format!("picker failed: {e}"),
            "pass the value on the command line instead",
        )),
    }
}

/// Choose from `(value, label)` pairs, starting at `current`.
pub fn choose(
    prompt: &str,
    items: &[(String, String)],
    current: &str,
) -> CliResult<Option<String>> {
    let labels: Vec<String> = items.iter().map(|(_, l)| l.clone()).collect();
    let start = items.iter().position(|(v, _)| v == current).unwrap_or(0);
    let picked = cancelled(
        Select::new(prompt, labels.clone())
            .with_starting_cursor(start)
            .with_page_size(12)
            .prompt(),
    )?;
    Ok(picked.and_then(|l| {
        labels
            .iter()
            .position(|x| *x == l)
            .map(|i| items[i].0.clone())
    }))
}

/// Pick a mode.
pub fn mode(current: &str) -> CliResult<Option<Mode>> {
    let items: Vec<(String, String)> = Mode::ALL
        .iter()
        .map(|m| {
            (
                m.display().to_string(),
                format!("{:<10} {}", m.display(), m.describe()),
            )
        })
        .collect();
    Ok(choose("Mode", &items, current)?.and_then(|s| Mode::parse(&s)))
}

/// Pick a voice.
pub fn voice(current: &str) -> CliResult<Option<String>> {
    let items: Vec<(String, String)> = voices::picker_order()
        .into_iter()
        .map(|v| (v.to_string(), format!("{v:<14} {}", voices::label(v))))
        .collect();
    choose("Voice (type to filter)", &items, current)
}

/// Pick a persona.
pub fn persona(ctx: &Ctx, current: &str) -> CliResult<Option<String>> {
    let items: Vec<(String, String)> = settings::persona_names(&ctx.paths)
        .into_iter()
        .map(|p| {
            let d = settings::persona_describe(&p);
            (p.clone(), format!("{p:<10} {d}"))
        })
        .collect();
    choose("Persona", &items, current)
}

/// Pick a speed.
pub fn speed(current: f64) -> CliResult<Option<f64>> {
    let steps = [0.8, 0.9, 1.0, 1.05, 1.1, 1.2, 1.3, 1.5];
    let items: Vec<(String, String)> = steps
        .iter()
        .map(|s| (s.to_string(), settings::fmt_speed(*s)))
        .collect();
    Ok(choose("Speed", &items, &current.to_string())?.and_then(|s| s.parse().ok()))
}

/// Pick alerts.
pub fn alerts(current: Alerts) -> CliResult<Option<Alerts>> {
    let items: Vec<(String, String)> = Alerts::ALL
        .iter()
        .map(|a| {
            (
                a.key().to_string(),
                format!("{:<7} {}", a.key(), a.describe()),
            )
        })
        .collect();
    Ok(choose("Alerts", &items, current.key())?.and_then(|s| Alerts::parse(&s)))
}

/// Yes/no, default `default`.
pub fn confirm(prompt: &str, default: bool) -> CliResult<bool> {
    Ok(cancelled(Confirm::new(prompt).with_default(default).prompt())?.unwrap_or(false))
}
