//! The command tree (clap derive). Help text is grouped by what people are
//! trying to do; `tests/cli.rs` checks every subcommand appears in it.

use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;

use crate::commands::install::Target;

/// The top-level help, grouped. Keep in step with [`Command`].
const HELP_TEMPLATE: &str = "\
{about-with-newline}
{usage-heading} {usage}

Run `heard` with no command to open the interactive console.

Get started:
  setup        Guided setup: voice model, voice, mode, agent hooks
  doctor       Check the install and print a fix for anything wrong
  install      Add Heard's hooks to Claude Code and/or Codex
  uninstall    Remove Heard's hooks (--purge also deletes models and state)

How Heard sounds:
  mode         How much Heard says: co-pilot, companion or focus
  voice        Pick the Kokoro voice (--preview to hear it, --list to browse)
  persona      Who narrates: jarvis, aria, friday, atlas, raw, or a file
  speed        Speaking rate, 0.5 to 2.0
  alerts       How needs-you moments reach you: voice, notify, both, off

Right now:
  pause        Stop narrating until `heard resume`
  resume       Start narrating again
  mute         Silence speech, or one agent session with --session
  unmute       Undo `mute`
  say          Speak a line of text
  agents       The agent sessions Heard is following
  history      What Heard said recently (--since 1h, --json)
  status       Daemon, settings and agents at a glance (--json)

Daemon and files:
  start        Start the background daemon
  stop         Stop it
  restart      Stop, then start
  models       download | status | remove the Kokoro voice model
  config       get | set | list | path for the raw config
  completions  Print a shell completion script (zsh, bash, fish)

Options:
{options}

Run `heard <command> --help` for details. Exit codes: 0 ok, 1 failed, 2 usage.
";

/// heard — hear your coding agents. Claude Code and Codex CLI, narrated out
/// loud by a local voice. No account, no cloud.
#[derive(Debug, Parser)]
#[command(
    name = "heard",
    version,
    about = "heard — hear your coding agents. A local voice narrates Claude Code and Codex CLI.",
    help_template = HELP_TEMPLATE,
    disable_help_subcommand = true,
    arg_required_else_help = false
)]
pub struct Cli {
    /// The subcommand; none opens the console.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Every subcommand.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Guided setup: download the voice model, pick a voice and a mode, and
    /// offer hooks for the agents found on PATH.
    Setup,

    /// Show or set how much Heard says.
    ///
    /// co-pilot: at the screen: turn results and anything that needs you.
    /// companion: away from it: the agent's prose and steps too.
    /// focus: heads-down: only failures, questions and approvals.
    ///
    /// With no value: a picker on a terminal, the current mode otherwise.
    Mode {
        /// copilot (co-pilot), companion or focus.
        #[arg(num_args = 0.., value_name = "VALUE")]
        value: Vec<String>,
        /// One plain line, for the hook (internal).
        #[arg(long, hide = true)]
        porcelain: bool,
    },

    /// Show or set the Kokoro voice.
    ///
    /// With no name: a picker on a terminal, the current voice otherwise.
    Voice {
        /// A voice id such as bm_george or af_heart.
        #[arg(num_args = 0.., value_name = "VALUE")]
        name: Vec<String>,
        /// One plain line, for the hook (internal).
        #[arg(long, hide = true)]
        porcelain: bool,
        /// Speak a short sample after switching (needs the daemon).
        #[arg(long)]
        preview: bool,
        /// List every voice.
        #[arg(long)]
        list: bool,
        /// With --list: machine-readable output.
        #[arg(long)]
        json: bool,
    },

    /// Show or set the persona (who narrates and in what style).
    ///
    /// A path to a persona .md file installs it into Heard's personas folder
    /// and selects it.
    Persona {
        /// jarvis, aria, friday, atlas, raw, a user persona, or a .md file (words are joined).
        #[arg(num_args = 0.., value_name = "VALUE")]
        value: Vec<String>,
        /// One plain line, for the hook (internal).
        #[arg(long, hide = true)]
        porcelain: bool,
    },

    /// Show or set the speaking rate (0.5 to 2.0).
    Speed {
        /// e.g. 1.1 or 1.1x.
        #[arg(num_args = 0.., value_name = "VALUE")]
        value: Vec<String>,
        /// One plain line, for the hook (internal).
        #[arg(long, hide = true)]
        porcelain: bool,
    },

    /// Show or set how needs-you moments reach you.
    ///
    /// Needs-you lines are approvals and permission requests, questions the
    /// agent asks you, failures, and a turn that ends waiting on your
    /// decision. Everything else is narrated by mode whatever this says.
    ///
    ///   both    say it and post a macOS notification (default)
    ///   voice   say it, no notification
    ///   notify  post a notification instead of saying it
    ///   off     neither: needs-you lines are not spoken or notified
    ///
    /// Notifications come from `osascript` (title "Heard", subtitle
    /// "<agent · project>"); a burst is folded into one every few seconds.
    #[command(verbatim_doc_comment)]
    Alerts {
        /// voice, notify, both or off.
        #[arg(num_args = 0.., value_name = "VALUE")]
        value: Vec<String>,
        /// One plain line, for the hook (internal).
        #[arg(long, hide = true)]
        porcelain: bool,
    },

    /// Stop narrating. Persists across daemon restarts until `heard resume`.
    Pause {
        /// One plain line, for the hook (internal).
        #[arg(long, hide = true)]
        porcelain: bool,
    },

    /// Start narrating again after `heard pause`.
    Resume {
        /// One plain line, for the hook (internal).
        #[arg(long, hide = true)]
        porcelain: bool,
    },

    /// Silence all speech, or one agent session with --session.
    Mute(MuteArgs),

    /// Undo `heard mute` (all speech, or one session with --session).
    Unmute(MuteArgs),

    /// Speak a line of text through the daemon.
    Say {
        /// The text; several words are joined with spaces.
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        text: Vec<String>,
        /// One plain line, for the hook (internal).
        #[arg(long, hide = true)]
        porcelain: bool,
    },

    /// The agent sessions Heard is following right now.
    Agents {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },

    /// What Heard said recently. Local only.
    History {
        /// Only entries newer than this: 30s, 5m, 2h, 1d.
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
        /// At most this many entries (newest kept).
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Machine-readable output (a JSON array of records).
        #[arg(long)]
        json: bool,
    },

    /// Daemon, settings and agents at a glance.
    Status {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// One plain line, for the hook (internal).
        #[arg(long, hide = true)]
        porcelain: bool,
    },

    /// Start the background daemon (it also starts on the first agent event,
    /// and when the console opens).
    ///
    /// Logs to daemon.log in Heard's state directory; `heard status` shows it.
    Start,

    /// Stop the background daemon.
    Stop,

    /// Stop, then start the background daemon.
    Restart,

    /// Add Heard's hooks to an agent CLI.
    Install {
        /// claude-code, codex or all.
        #[arg(value_enum, default_value_t = Target::All)]
        target: Target,
        /// Install even if the Heard app's hooks are present.
        #[arg(long)]
        force: bool,
    },

    /// Remove Heard's hooks from an agent CLI.
    Uninstall {
        /// claude-code, codex or all.
        #[arg(value_enum, default_value_t = Target::All)]
        target: Target,
        /// Also delete the voice model, config and history.
        #[arg(long)]
        purge: bool,
    },

    /// Download, check or remove the Kokoro voice model (about 354 MB).
    Models {
        /// What to do.
        #[command(subcommand)]
        action: ModelsCmd,
    },

    /// Check the install and print a fix for anything wrong.
    Doctor {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },

    /// Read and write the raw config (config.yaml).
    Config {
        /// What to do.
        #[command(subcommand)]
        action: ConfigCmd,
    },

    /// Print a shell completion script.
    ///
    /// zsh:  heard completions zsh > "${fpath[1]}/_heard"
    /// bash: heard completions bash > ~/.local/share/bash-completion/completions/heard
    /// fish: heard completions fish > ~/.config/fish/completions/heard.fish
    Completions {
        /// zsh, bash or fish (also elvish, powershell).
        shell: Shell,
    },

    /// Run the daemon (internal; started automatically).
    ///
    /// One per state directory: a second `heard daemon` exits 0. Stops on
    /// SIGTERM or SIGINT (removing daemon.sock and daemon.pid); SIGHUP
    /// reloads config.
    #[command(hide = true)]
    Daemon {
        /// Stay attached to this terminal and log to it.
        #[arg(long)]
        foreground: bool,
        /// Where lines go: `queued` (the voice, default) or `log` (record
        /// would-say lines to would-say.jsonl, no audio). For tests; also
        /// read from HEARD_DAEMON_SPEECH, so an auto-started daemon can be
        /// told.
        #[arg(long, hide = true, value_name = "KIND")]
        speech: Option<String>,
        /// Voice backend: `auto` (Kokoro, or your own ElevenLabs key, else
        /// silence) or `null` (silence). For tests; also HEARD_DAEMON_TTS.
        #[arg(long, hide = true, value_name = "KIND")]
        tts: Option<String>,
        /// Needs-you notifications: `osascript` (default) or `log` (append
        /// to notifications.jsonl, show nothing). For tests; also
        /// HEARD_DAEMON_NOTIFIER.
        #[arg(long, hide = true, value_name = "KIND")]
        notifier: Option<String>,
    },
}

/// `--session` for mute/unmute.
#[derive(Debug, Args)]
pub struct MuteArgs {
    /// A session id (or its first characters, or the repo name) from
    /// `heard agents`.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
    /// One plain line, for the hook (internal).
    #[arg(long, hide = true)]
    pub porcelain: bool,
}

/// `heard models …`
#[derive(Debug, Subcommand)]
pub enum ModelsCmd {
    /// Download the model (resumes a partial download; verifies SHA-256).
    Download,
    /// Is the model installed? --verify also checks SHA-256.
    Status {
        /// Hash the files (takes a second).
        #[arg(long)]
        verify: bool,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Delete the model files.
    Remove,
}

/// `heard config …`
#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Print one value.
    Get {
        /// The key, e.g. speed.
        key: String,
        /// Print it as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Set one value (validated, then the daemon reloads).
    Set {
        /// The key.
        key: String,
        /// The value (true/false, numbers and strings are typed by the key).
        value: String,
    },
    /// Print every value (API keys redacted).
    List {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// Print API keys and tokens in full.
        #[arg(long)]
        show_secrets: bool,
    },
    /// Print the config file's path.
    Path,
}

/// The names in the grouped help, for the drift test.
pub fn help_listed_commands() -> Vec<&'static str> {
    HELP_TEMPLATE
        .lines()
        .filter_map(|l| l.strip_prefix("  "))
        .filter_map(|l| l.split_whitespace().next())
        .collect()
}
