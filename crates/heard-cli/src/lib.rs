//! `heard` — the CLI-only Heard.
//!
//! A library so the daemon ([`compose`]) and tests reach the same paths,
//! settings and client the binary uses. `heard daemon` is the CLI edition's
//! composition root: the core daemon, the Kokoro speech queue, templates-only
//! narration and the [`notify`] extension.

#![deny(unsafe_code)]

pub mod cli;
pub mod client;
pub mod commands;
pub mod compose;
pub mod console;
pub mod doctor;
pub mod edition;
pub mod history;
pub mod models;
pub mod notify;
pub mod paths;
pub mod persona;
pub mod pick;
pub mod settings;
pub mod setup;
pub mod ui;
pub mod voices;

use clap::Parser;

/// Parse argv, run, and return the process exit code.
pub fn run() -> u8 {
    let porcelain = std::env::args().any(|a| a == "--porcelain");
    let cli = match cli::Cli::try_parse() {
        Ok(c) => c,
        Err(e) if porcelain && e.use_stderr() => {
            // The hook wants one plain line, even for a usage error.
            let text = e.render().to_string();
            let first = text.lines().next().unwrap_or("error: bad arguments");
            println!("{}", first.trim());
            return ui::EXIT_USAGE;
        }
        Err(e) => {
            // --help / --version print and exit 0; real usage errors exit 2.
            let code = if e.use_stderr() {
                ui::EXIT_USAGE
            } else {
                ui::EXIT_OK
            };
            let _ = e.print();
            return code;
        }
    };
    let ctx = match settings::Ctx::from_env() {
        Ok(c) => c,
        Err(e) => {
            if porcelain {
                println!("error: {e}");
            } else {
                e.report();
            }
            return e.code;
        }
    };
    let r = match cli.command {
        None => console::run(&ctx),
        Some(cmd) => commands::dispatch(&ctx, cmd),
    };
    match r {
        Ok(()) => ui::EXIT_OK,
        Err(e) => {
            e.report();
            e.code
        }
    }
}
