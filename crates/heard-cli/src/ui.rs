//! Terminal manners: colour that honours `NO_COLOR`, the error type every
//! command returns, and the exit codes (0 ok, 1 failure, 2 usage).

use std::fmt;
use std::io::IsTerminal;
use std::sync::OnceLock;

use nu_ansi_term::{Color, Style};

/// Exit code: the command did what was asked.
pub const EXIT_OK: u8 = 0;
/// Exit code: the command ran and failed.
pub const EXIT_FAILURE: u8 = 1;
/// Exit code: the command line itself was wrong.
pub const EXIT_USAGE: u8 = 2;

/// Every failure a command can report. It always names the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// What went wrong, one line.
    pub message: String,
    /// What to do about it: a command to run or a thing to check.
    pub fix: Option<String>,
    /// [`EXIT_FAILURE`] or [`EXIT_USAGE`].
    pub code: u8,
}

impl CliError {
    /// A runtime failure (exit 1).
    pub fn failure(message: impl Into<String>, fix: impl Into<String>) -> Self {
        CliError {
            message: message.into(),
            fix: Some(fix.into()),
            code: EXIT_FAILURE,
        }
    }

    /// A usage error (exit 2).
    pub fn usage(message: impl Into<String>, fix: impl Into<String>) -> Self {
        CliError {
            message: message.into(),
            fix: Some(fix.into()),
            code: EXIT_USAGE,
        }
    }

    /// An error already shown to the user (exit code only).
    pub fn silent(code: u8) -> Self {
        CliError {
            message: String::new(),
            fix: None,
            code,
        }
    }

    /// Print to stderr as `error: …` plus `  fix: …` (nothing when silent).
    pub fn report(&self) {
        if self.message.is_empty() {
            return;
        }
        eprintln!(
            "{} {}",
            paint_err(Color::Red.bold(), "error:"),
            self.message
        );
        if let Some(fix) = &self.fix {
            eprintln!("  {} {}", paint_err(Color::Yellow.normal(), "fix:"), fix);
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(fix) = &self.fix {
            write!(f, " (fix: {fix})")?;
        }
        Ok(())
    }
}

impl std::error::Error for CliError {}

impl From<heard_config::ConfigError> for CliError {
    fn from(e: heard_config::ConfigError) -> Self {
        CliError::failure(
            format!("config: {e}"),
            "check the file named above is readable and valid YAML, or move it aside and re-run",
        )
    }
}

/// The result type every command returns.
pub type CliResult<T> = Result<T, CliError>;

fn colour_allowed() -> bool {
    match std::env::var_os("NO_COLOR") {
        Some(v) if !v.is_empty() => return false,
        _ => {}
    }
    !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
}

/// Colour on stdout?
pub fn stdout_colour() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| colour_allowed() && std::io::stdout().is_terminal())
}

/// Colour on stderr?
pub fn stderr_colour() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| colour_allowed() && std::io::stderr().is_terminal())
}

/// Paint for stdout (plain text when colour is off).
pub fn paint(style: Style, text: &str) -> String {
    if stdout_colour() {
        style.paint(text).to_string()
    } else {
        text.to_string()
    }
}

/// Paint for stderr.
pub fn paint_err(style: Style, text: &str) -> String {
    if stderr_colour() {
        style.paint(text).to_string()
    } else {
        text.to_string()
    }
}

/// Bold.
pub fn bold(text: &str) -> String {
    paint(Style::new().bold(), text)
}

/// Dimmed.
pub fn dim(text: &str) -> String {
    paint(Style::new().dimmed(), text)
}

/// Green.
pub fn green(text: &str) -> String {
    paint(Color::Green.normal(), text)
}

/// Yellow.
pub fn yellow(text: &str) -> String {
    paint(Color::Yellow.normal(), text)
}

/// Red.
pub fn red(text: &str) -> String {
    paint(Color::Red.normal(), text)
}

/// Cyan.
pub fn cyan(text: &str) -> String {
    paint(Color::Cyan.normal(), text)
}

/// Both stdin and stdout are terminals: prompting is allowed.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Print a warning line to stderr.
pub fn warn(msg: &str) {
    eprintln!("{} {msg}", paint_err(Color::Yellow.bold(), "warning:"));
}

/// Print a JSON value, pretty, to stdout.
pub fn print_json(v: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".into())
    );
}
