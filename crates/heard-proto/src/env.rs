//! Which terminal owns *this* process — `client.py`'s
//! `_terminal_binding_from_env` and `host_label_from_binding`, ported.
//!
//! This has to run in the **hook**, not the daemon. The hook is a
//! short-lived child of the agent CLI, which is itself a child of the
//! terminal or editor that launched it, so these variables are the session's
//! own process-tree evidence, inherited at process creation. The daemon's
//! environment is the menu-bar app's and identifies no terminal at all —
//! deriving the binding there would label every session wrong, which is
//! exactly the class of bug seen before (a VS Code session labelled Ghostty,
//! on a machine where Ghostty was not installed).
//!
//! Read [`HookEnv::from_process`] once, then borrow a [`Binding`] out of it.

use crate::event::Binding;

/// VS Code-family bundle ids, lower-cased. Mirrors `client.py`'s
/// `_EDITOR_BUNDLE_IDS`, `tools/capture.py` and `BindingDelivery.swift`; a
/// new editor belongs in all of them or the surfaces disagree about the same
/// session.
const EDITOR_BUNDLE_IDS: [&str; 4] = [
    "com.microsoft.vscode",
    "com.microsoft.vscodeinsiders",
    "com.todesktop.230313mzl4w4u92",
    "com.exafunction.windsurf",
];

/// Cursor ships via ToDesktop, so its real bundle id spells neither
/// "cursor" nor anything else recognisable.
const CURSOR_TODESKTOP_ID: &str = "com.todesktop.230313mzl4w4u92";

/// `"env_terminal_binding"` — this evidence came from the process's own
/// environment, never from a foreground-window poll.
const PROVENANCE: &str = "env_terminal_binding";

/// What the env path claims. `0.9` in Python, and not a guess: the variables
/// are set at process creation and inherited, not sampled.
const CONFIDENCE: f64 = 0.9;

/// The six keys a Herdr pane contributes, written only when Herdr itself
/// says `HERDR_ENV=1`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Herdr {
    pane_id: String,
    tab_id: String,
    workspace_id: String,
    session: String,
    socket_path: String,
    bin_path: String,
}

/// An owned snapshot of the variables the binding is derived from.
///
/// Owned because `std::env` hands back owned strings; a [`Binding`] then
/// borrows from this, so the derivation itself copies nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookEnv {
    term_program: String,
    bundle_id: String,
    ghostty: bool,
    cursor_trace: bool,
    iterm_session: bool,
    herdr: Option<Herdr>,
    pid: i64,
}

impl HookEnv {
    /// Read this process's environment and its parent pid.
    ///
    /// The parent is the agent CLI that spawned the hook, which is what
    /// `os.getppid()` means in `_terminal_binding_from_env`.
    pub fn from_process() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok(), parent_pid())
    }

    /// The same derivation over an arbitrary environment, for tests.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>, pid: i64) -> Self {
        let herdr = if get("HERDR_ENV").unwrap_or_default().trim() == "1" {
            // Herdr exports these into every pane it launches; the `or ""`
            // in Python means a missing one is empty, not absent.
            Some(Herdr {
                pane_id: get("HERDR_PANE_ID").unwrap_or_default(),
                tab_id: get("HERDR_TAB_ID").unwrap_or_default(),
                workspace_id: get("HERDR_WORKSPACE_ID").unwrap_or_default(),
                session: get("HERDR_SESSION").unwrap_or_default(),
                socket_path: get("HERDR_SOCKET_PATH").unwrap_or_default(),
                bin_path: get("HERDR_BIN_PATH").unwrap_or_default(),
            })
        } else {
            None
        };
        Self {
            term_program: get("TERM_PROGRAM").unwrap_or_default().trim().to_string(),
            bundle_id: get("__CFBundleIdentifier")
                .unwrap_or_default()
                .to_lowercase(),
            // Python takes `bool(env.get(...))`, so an empty value is no
            // evidence at all.
            ghostty: !get("GHOSTTY_RESOURCES_DIR").unwrap_or_default().is_empty(),
            cursor_trace: !get("CURSOR_TRACE_ID").unwrap_or_default().is_empty(),
            iterm_session: !get("ITERM_SESSION_ID").unwrap_or_default().is_empty(),
            herdr,
            pid,
        }
    }

    /// The display host name, or `None` when the evidence names no known
    /// host — never a guessed name.
    pub fn host_name(&self) -> Option<&'static str> {
        // A Herdr pane is "in Herdr" whatever its TERM_PROGRAM
        // says — Herdr owns the pty. Only when Herdr says so itself: a stale
        // HERDR_PANE_ID without HERDR_ENV=1 is never enough.
        if let Some(h) = &self.herdr {
            if !h.pane_id.trim().is_empty() {
                return Some("Herdr");
            }
        }
        if self.ghostty {
            return Some("Ghostty");
        }
        // `TERM_PROGRAM=vscode` comes from VS Code's integrated
        // terminal. A session the Claude Code extension spawned has no shell
        // at all — it is a child of the extension host — so it arrives with
        // a bundle id and nothing else; the bundle id alone is evidence.
        if self.term_program == "vscode" || EDITOR_BUNDLE_IDS.contains(&self.bundle_id.as_str()) {
            // The forks all report TERM_PROGRAM=vscode, so the bundle id
            // (and, for Cursor, its own CURSOR_TRACE_ID) is the separator.
            if self.cursor_trace
                || self.bundle_id.contains("cursor")
                || self.bundle_id == CURSOR_TODESKTOP_ID
            {
                return Some("Cursor");
            }
            // Windsurf is a nameable, drivable host: labelling its sessions
            // "VS Code" would send a typed reply to the wrong editor.
            if self.bundle_id.contains("windsurf") {
                return Some("Windsurf");
            }
            return Some("VS Code");
        }
        if self.term_program == "iTerm.app" || self.iterm_session {
            return Some("iTerm");
        }
        if self.term_program == "Apple_Terminal" {
            return Some("Terminal");
        }
        None
    }

    /// The binding record, or `None` when no host could be identified — in
    /// which case the key is omitted from the frame entirely.
    ///
    /// Note that the Herdr keys ride along whenever `HERDR_ENV=1`, even when
    /// the *label* came from the terminal evidence instead (a Herdr pane
    /// with no pane id). `_terminal_binding_from_env` does the same via its
    /// unconditional `binding.update(herdr)`.
    pub fn binding(&self) -> Option<Binding<'_>> {
        let host_name = self.host_name()?;
        let host_type = match host_name {
            "VS Code" | "Cursor" | "Windsurf" => "editor_terminal",
            // Ghostty, iTerm, Terminal, Herdr — and the `.get(label,
            // "terminal")` default for anything added later.
            _ => "terminal",
        };
        let h = self.herdr.as_ref();
        Some(Binding {
            host_name,
            host_type,
            provenance: PROVENANCE,
            confidence: CONFIDENCE,
            pid: self.pid,
            // No cheap, dependency-free way to read a process's start time
            // on macOS (no /proc); Python leaves it None and so do we.
            process_started_at: None,
            herdr_pane_id: h.map(|h| h.pane_id.as_str()),
            herdr_tab_id: h.map(|h| h.tab_id.as_str()),
            herdr_workspace_id: h.map(|h| h.workspace_id.as_str()),
            herdr_session: h.map(|h| h.session.as_str()),
            herdr_socket_path: h.map(|h| h.socket_path.as_str()),
            herdr_bin_path: h.map(|h| h.bin_path.as_str()),
        })
    }
}

/// `os.getppid()`.
fn parent_pid() -> i64 {
    // SAFETY-free: `getppid` is one of the few syscalls std exposes directly.
    std::os::unix::process::parent_id() as i64
}
