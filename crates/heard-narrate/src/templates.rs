//! Default per-tool narration templates.
//!
//! Port of `engine/heard/templates.py`. Each event returns a [`Narration`]:
//!   - `tag`: a stable string the persona layer uses to look up overrides
//!   - `text`: the neutral spoken string (used when the persona is raw, and as
//!     seed material for an optional LLM narrator's rewrites)
//!   - `ctx`: variables available to persona template substitution (e.g.
//!     `{"file": "auth.py"}`)
//!
//! `None` means "stay silent" — the dispatcher skips synthesis.
//!
//! Tool inputs and results arrive as decoded JSON, so they are taken as
//! [`serde_json::Value`]; every string that survives into the narration is
//! borrowed from that value where it can be ([`Cow::Borrowed`]), which is most
//! of the time — this is substring work over text we already own.
//!
//! Python raises `TypeError` when a field it treats as text is not a string
//! (`os.path.basename(123)`), so such inputs have no defined behaviour to port
//! and are not in the golden corpus; here a non-string field reads as absent.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// One narration event: what to say, under what tag, with what context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Narration<'a> {
    pub tag: &'static str,
    pub text: Cow<'a, str>,
    pub ctx: Vec<(&'static str, Ctx<'a>)>,
}

/// A `ctx` value. Almost everything is text; `exit_code` is whatever JSON the
/// tool result carried, and is passed through unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ctx<'a> {
    Text(Cow<'a, str>),
    Json(Value),
}

impl Ctx<'_> {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Ctx::Text(s) => Some(s),
            Ctx::Json(Value::String(s)) => Some(s),
            Ctx::Json(_) => None,
        }
    }
}

impl<'a> Narration<'a> {
    fn new(tag: &'static str, text: impl Into<Cow<'a, str>>) -> Self {
        Narration {
            tag,
            text: text.into(),
            ctx: Vec::new(),
        }
    }

    fn with(mut self, key: &'static str, value: impl Into<Cow<'a, str>>) -> Self {
        self.ctx.push((key, Ctx::Text(value.into())));
        self
    }

    fn with_json(mut self, key: &'static str, value: Value) -> Self {
        self.ctx.push((key, Ctx::Json(value)));
        self
    }

    /// The `ctx` dict as JSON, for the golden corpus and for the prompt
    /// builders that hand it to the model.
    pub fn ctx_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        for (key, value) in &self.ctx {
            map.insert(
                (*key).to_owned(),
                match value {
                    Ctx::Text(s) => Value::String(s.to_string()),
                    Ctx::Json(v) => v.clone(),
                },
            );
        }
        Value::Object(map)
    }
}

/// Per-side cap for change snippets passed into `tool_pre` ctx (Edit old/new,
/// Write content, NotebookEdit new_source). An optional LLM narrator reads them as raw
/// context, not for narration. 400 chars is enough for a model to identify a
/// feature add / refactor / config tweak without bloating the prompt for very
/// large rewrites. Pinned by `test_change_snippet_cap_is_prompt_context_untouched`.
pub const CHANGE_SNIPPET_CAP: usize = 400;

// ---------------------------------------------------------------------------
// filenames
// ---------------------------------------------------------------------------

/// Stems that, on their own, parse as English pronouns / conjunctions /
/// prepositions / abbreviations and would confuse the listener. K. heard
/// "Editing me." while we were editing heard-api/src/me.ts — the spoken
/// narration was technically correct but sounded like the AI was editing the
/// listener. For these stems we keep the extension so TTS reads "me dot ts"
/// rather than just "me".
const AMBIGUOUS_SHORT_STEMS: &[&str] = &[
    // 2 chars — pronouns / shorthand
    "me", "do", "go", "is", "it", "in", "on", "of", "at", "an", "or", "as", "if", "no", "so", "to",
    "by", "my", "we", "he", "us", "up", "am", "ok", "id", "ui", "io", "ip",
    // 3 chars — common confusing tokens (functions, conjunctions, etc.)
    "and", "the", "but", "for", "any", "all", "you", "she", "her", "him", "his", "out", "now",
    "way", "new", "old", "yes", "may", "can", "did", "let", "got", "see", "say", "use", "run",
    "log",
];

/// `os.path.basename` on a POSIX path.
fn basename(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// `os.path.splitext` — the last dot that is not part of a leading run of dots.
/// `".zshrc"` splits to `(".zshrc", "")`; `"a.tar.gz"` to `("a.tar", ".gz")`.
fn splitext(name: &str) -> (&str, &str) {
    let Some(dot) = name.rfind('.') else {
        return (name, "");
    };
    // Skip a leading run of dots: a dotfile has no extension.
    let leading_dots = name.len() - name.trim_start_matches('.').len();
    if dot < leading_dots {
        return (name, "");
    }
    (&name[..dot], &name[dot..])
}

/// Filename for TTS — drops the extension so we don't speak ".py" aloud ("ui
/// dot py"). Bare files like Dockerfile / Makefile keep their full name;
/// dotfiles like .zshrc keep their stem.
///
/// Exception: when the stem alone would parse as an English word (pronoun,
/// conjunction, common abbreviation), keep the extension so the listener gets
/// context. "me.ts" → spoken as "me dot ts" (the listener registers "code
/// file") rather than "me" (which sounds like a pronoun).
fn spoken_filename(path: &str) -> &str {
    let name = basename(path);
    if name.is_empty() {
        return "";
    }
    let (stem, ext) = splitext(name);
    if stem.is_empty() {
        return name;
    }
    if !ext.is_empty() && AMBIGUOUS_SHORT_STEMS.contains(&stem.to_lowercase().as_str()) {
        return name;
    }
    stem
}

// ---------------------------------------------------------------------------
// bash intent
// ---------------------------------------------------------------------------

const BUILD_VERBS: &[&str] = &["build", "compile", "bundle"];
const TEST_MARKERS: &[&str] = &[
    "pytest",
    "jest",
    "vitest",
    "go test",
    "cargo test",
    "rspec",
    "npm test",
    "pnpm test",
    "yarn test",
];
const INSTALL_MARKERS: &[&str] = &[
    "npm install",
    "pnpm install",
    "yarn install",
    "pip install",
    "uv add",
    "uv sync",
    "bundle install",
    "cargo add",
    "brew install",
];

/// Single-word command → present-tense narration. Hit when no higher-priority
/// pattern matches AND the agent didn't pass a description. Saves us from
/// saying "Running a shell command" for every grep/ls/cat in a session — the
/// user wants intent, not a blank acknowledgment.
const BASH_VERBS: &[(&str, (&str, &str))] = &[
    ("ls", ("tool_bash_list", "Listing files.")),
    ("find", ("tool_bash_find", "Searching.")),
    ("grep", ("tool_bash_grep_cmd", "Searching the codebase.")),
    ("rg", ("tool_bash_grep_cmd", "Searching the codebase.")),
    ("cat", ("tool_bash_read", "Reading a file.")),
    ("head", ("tool_bash_read", "Reading a file.")),
    ("tail", ("tool_bash_read", "Reading a file.")),
    ("less", ("tool_bash_read", "Reading a file.")),
    ("rm", ("tool_bash_remove", "Removing files.")),
    ("cp", ("tool_bash_copy", "Copying files.")),
    ("mv", ("tool_bash_move", "Moving files.")),
    ("mkdir", ("tool_bash_mkdir", "Creating a directory.")),
    ("touch", ("tool_bash_touch", "Creating a file.")),
    ("ps", ("tool_bash_ps", "Listing processes.")),
    ("kill", ("tool_bash_kill", "Killing a process.")),
    ("pkill", ("tool_bash_kill", "Killing processes.")),
    ("chmod", ("tool_bash_chmod", "Setting permissions.")),
    ("chown", ("tool_bash_chmod", "Setting ownership.")),
    ("make", ("tool_bash_build", "Building.")),
    ("ssh", ("tool_bash_ssh", "Connecting via ssh.")),
    ("scp", ("tool_bash_scp", "Copying over ssh.")),
    ("curl", ("tool_bash_curl", "Fetching over HTTP.")),
    ("wget", ("tool_bash_curl", "Downloading.")),
    ("tar", ("tool_bash_tar", "Working with an archive.")),
    ("zip", ("tool_bash_tar", "Compressing.")),
    ("unzip", ("tool_bash_tar", "Extracting.")),
    ("open", ("tool_bash_open", "Opening.")),
    ("diff", ("tool_bash_diff", "Diffing.")),
    ("wc", ("tool_bash_wc", "Counting.")),
];

static COMPOUND_SEP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*(?:&&|\|\||;|\|)\s*").unwrap());

/// Verb-extraction front-end: returns the most informative command-name from a
/// shell line.
///
/// Handles three real-world shapes the agent emits routinely:
///   * `FOO=bar cmd`        → `cmd` (skip env-var prefix)
///   * `sudo cmd`           → `cmd` (skip sudo wrapper)
///   * `cd src && grep foo` → `grep` (the actual intent isn't `cd`)
///
/// For compound commands we take the LAST segment after the shell separators.
/// The first segment is usually setup (`cd`, `cd ..`, `export X=…`); the
/// trailing segment carries the action.
pub fn first_token(command: &str) -> &str {
    let text = command.trim();
    if text.is_empty() {
        return "";
    }
    let last = COMPOUND_SEP.split(text).last().unwrap_or("").trim();
    let parts: Vec<&str> = last.split_whitespace().collect();
    let mut i = 0usize;
    while i < parts.len() && parts[i].contains('=') && !parts[i].starts_with('-') {
        i += 1;
    }
    if i < parts.len() && parts[i] == "sudo" {
        i += 1;
    }
    parts.get(i).copied().unwrap_or("")
}

/// First words that are NOT imperative verbs — leave these descriptions alone
/// rather than mangle them ("Quick check…" must not become "Quicking…"). Small,
/// high-frequency stoplist; anything else is treated as a leading verb.
const NOT_A_LEADING_VERB: &[&str] = &[
    "a", "an", "the", "quick", "sanity", "full", "final", "one", "two", "all", "both", "no",
    "first", "second", "double", "re",
];

/// Best-effort present participle of a single imperative verb, preserving the
/// original capitalisation. English gerund morphology is irregular; this covers
/// the common cases (silent-e drop, CVC doubling) and falls back to bare +ing
/// otherwise.
fn to_gerund(word: &str) -> String {
    // Python indexes strings by CHARACTER, so the suffix arithmetic below has
    // to as well ("café" must not be sliced mid-codepoint).
    let chars: Vec<char> = word.chars().collect();
    let low: Vec<char> = word.to_lowercase().chars().collect();
    let ends_with = |suffix: &str| -> bool {
        let s: Vec<char> = suffix.chars().collect();
        low.len() >= s.len() && low[low.len() - s.len()..] == s[..]
    };
    let join =
        |take: usize, tail: &str| -> String { chars[..take].iter().collect::<String>() + tail };

    if ends_with("ing") {
        return word.to_owned();
    }
    if ends_with("ie") {
        // die → dying, tie → tying
        return join(chars.len().saturating_sub(2), "ying");
    }
    if ends_with("e") && !(ends_with("ee") || ends_with("oe") || ends_with("ye")) {
        // locate → locating, probe → probing
        return join(chars.len().saturating_sub(1), "ing");
    }
    // Short CVC verbs double the final consonant: run → running, set →
    // setting. Restricted to short words so multi-syllable verbs (open, visit)
    // don't wrongly double.
    if (2..=4).contains(&low.len()) {
        let c1 = low[low.len() - 1];
        let c2 = low[low.len() - 2];
        let third_is_vowel = low.len() >= 3 && "aeiou".contains(low[low.len() - 3]);
        if !"aeiouwxy".contains(c1) && "aeiou".contains(c2) && (low.len() < 3 || !third_is_vowel) {
            let mut out = word.to_owned();
            out.push(c1);
            out.push_str("ing");
            return out;
        }
    }
    // extract → extracting, verify → verifying
    format!("{word}ing")
}

/// Turn an imperative intent line ("Locate the sample video") into present
/// continuous ("Locating the sample video"). Only the leading verb is
/// transformed; the rest is untouched. Non-verb openers (per
/// [`NOT_A_LEADING_VERB`]) and non-alphabetic first tokens are left as-is.
pub fn present_continuous(text: &str) -> Cow<'_, str> {
    if text.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut parts = text.splitn(2, ' ');
    let first = parts.next().unwrap_or("");
    let rest = parts.next();
    let core: String = first.chars().filter(|c| *c != '-').collect();
    // Python: "".isalpha() is False, so an empty core bails out too.
    if core.is_empty()
        || !core.chars().all(char::is_alphabetic)
        || NOT_A_LEADING_VERB.contains(&first.to_lowercase().as_str())
    {
        return Cow::Borrowed(text);
    }
    let gerund = to_gerund(first);
    Cow::Owned(match rest {
        Some(tail) => format!("{gerund} {tail}"),
        None => gerund,
    })
}

/// Map a shell line (and the agent's own description, when it wrote one) to a
/// `(tag, spoken text)` pair. This is where "grep → search, ls → list" lives.
pub fn bash_tag_and_text<'a>(
    command: Option<&'a str>,
    description: Option<&'a str>,
) -> (&'static str, Cow<'a, str>) {
    let cmd = command.unwrap_or("").trim();
    let low = cmd.to_lowercase();
    if TEST_MARKERS.iter().any(|m| low.contains(m)) {
        return ("tool_bash_test", Cow::Borrowed("Running the test suite."));
    }
    if low.starts_with("git commit") {
        return ("tool_bash_commit", Cow::Borrowed("Committing."));
    }
    if low.starts_with("git push") {
        return ("tool_bash_push", Cow::Borrowed("Pushing."));
    }
    if low.starts_with("git pull") || low.starts_with("git fetch") {
        return ("tool_bash_sync", Cow::Borrowed("Syncing with git."));
    }
    if low.starts_with("git status") || low.starts_with("git log") || low.starts_with("git diff") {
        return (
            "tool_bash_git_inspect",
            Cow::Borrowed("Checking git status."),
        );
    }
    if INSTALL_MARKERS.iter().any(|m| low.starts_with(m)) {
        return (
            "tool_bash_install",
            Cow::Borrowed("Installing dependencies."),
        );
    }
    let first_tokens: Vec<&str> = low.split_whitespace().take(3).collect();
    if BUILD_VERBS.iter().any(|v| first_tokens.contains(v)) {
        return ("tool_bash_build", Cow::Borrowed("Building."));
    }

    // Description (when CC populates it) wins over verb detection — the
    // agent's hand-written intent line is almost always more specific than
    // what we'd derive from the command verb alone. CC writes these in the
    // imperative ("Locate sample video", "Probe specs"); narration wants
    // present continuous ("Locating sample video", "Probing specs") since the
    // work is in flight.
    if let Some(desc) = description.filter(|d| !d.is_empty()) {
        let trimmed = desc.trim_end_matches('.');
        return (
            "tool_bash_generic",
            Cow::Owned(format!("{}.", present_continuous(trimmed))),
        );
    }

    // No description: extract intent from the command's first verb so the user
    // hears "Searching the codebase." instead of the dreaded "Running a shell
    // command."
    let verb = first_token(&low);
    if let Some((_, (tag, text))) = BASH_VERBS.iter().find(|(v, _)| *v == verb) {
        return (tag, Cow::Borrowed(*text));
    }
    if !verb.is_empty() {
        return ("tool_bash_generic", Cow::Owned(format!("Running {verb}.")));
    }
    (
        "tool_bash_generic",
        Cow::Borrowed("Running a shell command."),
    )
}

// ---------------------------------------------------------------------------
// pre-tool
// ---------------------------------------------------------------------------

fn field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

/// Python's `(x or "")[:cap]` — a CHARACTER cap, not a byte one.
fn char_cap(text: &str, cap: usize) -> &str {
    match text.char_indices().nth(cap) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

/// The spoken beat for a tool that is about to run. `None` is silence.
pub fn pre_tool_event<'a>(tool_name: &str, tool_input: &'a Value) -> Option<Narration<'a>> {
    let input = tool_input;
    match tool_name {
        "Bash" => {
            let (tag, text) =
                bash_tag_and_text(field(input, "command"), field(input, "description"));
            // Classification: PROMPT CONTEXT, not speech. `text` (the
            // actual spoken/template string) is built above from the verb/tag
            // heuristics, never from this slice. `ctx["command"]` only flows
            // into the harness/persona user message as raw context for the LLM;
            // no persona template substitutes `{command}` into spoken text.
            let command = char_cap(field(input, "command").unwrap_or("").trim(), 200);
            Some(Narration::new(tag, text).with("command", command))
        }
        "Edit" => {
            let path = field(input, "file_path").unwrap_or("");
            let spoken = spoken_filename(path);
            let text: Cow<'a, str> = if spoken.is_empty() {
                Cow::Borrowed("Editing a file.")
            } else {
                Cow::Owned(format!("Editing {spoken}."))
            };
            Some(
                Narration::new("tool_edit", text)
                    .with("file", basename(path))
                    // `abs_path` carries the full path so the daemon's router
                    // can attribute this session to the correct project. The
                    // basename above stays for narration text; the full path is
                    // for routing only.
                    .with("abs_path", path)
                    .with(
                        "change_old",
                        char_cap(field(input, "old_string").unwrap_or(""), CHANGE_SNIPPET_CAP),
                    )
                    .with(
                        "change_new",
                        char_cap(field(input, "new_string").unwrap_or(""), CHANGE_SNIPPET_CAP),
                    ),
            )
        }
        "Write" => {
            let path = field(input, "file_path").unwrap_or("");
            let spoken = spoken_filename(path);
            let text: Cow<'a, str> = if spoken.is_empty() {
                Cow::Borrowed("Writing a file.")
            } else {
                Cow::Owned(format!("Writing {spoken}."))
            };
            Some(
                Narration::new("tool_write", text)
                    .with("file", basename(path))
                    .with("abs_path", path)
                    .with(
                        "change_new",
                        char_cap(field(input, "content").unwrap_or(""), CHANGE_SNIPPET_CAP),
                    ),
            )
        }
        "NotebookEdit" => {
            let path = field(input, "notebook_path").unwrap_or("");
            let spoken = spoken_filename(path);
            let text: Cow<'a, str> = if spoken.is_empty() {
                Cow::Borrowed("Editing a notebook.")
            } else {
                Cow::Owned(format!("Editing {spoken}."))
            };
            Some(
                Narration::new("tool_edit", text)
                    .with("file", basename(path))
                    .with("abs_path", path)
                    .with(
                        "change_new",
                        char_cap(field(input, "new_source").unwrap_or(""), CHANGE_SNIPPET_CAP),
                    ),
            )
        }
        "Read" => None,
        "Glob" => Some(
            Narration::new("tool_glob", "Searching for files.")
                .with("pattern", field(input, "pattern").unwrap_or("")),
        ),
        "Grep" => Some(
            Narration::new("tool_grep", "Searching the codebase.")
                .with("pattern", field(input, "pattern").unwrap_or("")),
        ),
        "WebFetch" => {
            let host = netloc(field(input, "url").unwrap_or(""));
            let text: Cow<'a, str> = if host.is_empty() {
                Cow::Borrowed("Fetching a page.")
            } else {
                Cow::Owned(format!("Fetching {host}."))
            };
            Some(Narration::new("tool_webfetch", text).with("host", host))
        }
        "WebSearch" => Some(
            Narration::new("tool_websearch", "Searching the web.")
                .with("query", field(input, "query").unwrap_or("")),
        ),
        "Agent" => {
            let desc = field(input, "description").unwrap_or("").trim();
            let text: Cow<'a, str> = if desc.is_empty() {
                Cow::Borrowed("Delegating to a subagent.")
            } else {
                Cow::Owned(format!("Delegating: {desc}."))
            };
            Some(Narration::new("tool_agent", text).with("description", desc))
        }
        "AskUserQuestion" => {
            let q = input
                .get("questions")
                .and_then(Value::as_array)
                .and_then(|qs| qs.first())
                .and_then(|q| q.get("question"))
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if input
                .get("questions")
                .and_then(Value::as_array)
                .is_some_and(|qs| !qs.is_empty())
                && !q.is_empty()
            {
                // recent_intent asks the LLM narrator (when one is wired) so the question gets summarised
                // to one sentence instead of synthed verbatim.
                Some(
                    Narration::new("tool_question", q)
                        .with("question", q)
                        .with("recent_intent", q),
                )
            } else {
                None
            }
        }
        "Skill" => {
            let skill = field(input, "skill").unwrap_or("").trim();
            let text: Cow<'a, str> = if skill.is_empty() {
                Cow::Borrowed("Running a skill.")
            } else {
                Cow::Owned(format!("Running the {skill} skill."))
            };
            Some(Narration::new("tool_skill", text).with("skill", skill))
        }
        "TaskCreate" => {
            let subj = field(input, "subject").unwrap_or("").trim();
            let text: Cow<'a, str> = if subj.is_empty() {
                Cow::Borrowed("Adding a task.")
            } else {
                Cow::Owned(format!("Tracking: {subj}."))
            };
            Some(Narration::new("tool_task_create", text).with("subject", subj))
        }
        "SendMessage" => {
            let to = field(input, "to").unwrap_or("").trim();
            let text: Cow<'a, str> = if to.is_empty() {
                Cow::Borrowed("Sending a message.")
            } else {
                Cow::Owned(format!("Messaging {to}."))
            };
            Some(Narration::new("tool_send_message", text).with("to", to))
        }
        // Silent on purpose: query/status tools (like Read), plan-mode
        // transitions (the agent narrates its own beats around them), and MCP
        // tools (their output shape isn't standardized).
        _ => None,
    }
}

/// `urllib.parse.urlparse(url).netloc`, for the subset of URLs a WebFetch
/// carries. Python wraps this in `try/except` and falls back to `""`, which is
/// what an unparseable URL yields here too.
fn netloc(url: &str) -> &str {
    // urlsplit removes tab/CR/LF anywhere and trims C0-control-or-space at the
    // ends. A URL carrying those is not in the corpus; trimming is enough.
    let url = url.trim_matches(|c: char| c <= ' ');
    let rest = match url.find(':') {
        Some(i)
            if i > 0
                && url[..i].starts_with(|c: char| c.is_ascii_alphabetic())
                && url[..i]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) =>
        {
            &url[i + 1..]
        }
        _ => url,
    };
    let Some(after) = rest.strip_prefix("//") else {
        return "";
    };
    match after.find(['/', '?', '#']) {
        Some(i) => &after[..i],
        None => after,
    }
}

// ---------------------------------------------------------------------------
// post-tool
// ---------------------------------------------------------------------------

/// An agent can be stopped by something only the HUMAN can do — install a
/// browser extension, sign in, grant access, enable an integration. That is not
/// a failure and not an AskUserQuestion, so it produced only a routine tool
/// event and the narration brain dropped it as chatter: the owner never heard
/// "install the Claude Chrome extension" from voice OR the notch (2026-09-11).
/// These lists together name that shape; one alone is too loose ("install"
/// appears in every npm log).
const NEEDS_YOU_ACTIONS: &[&str] = &[
    "install",
    "sign in",
    "signin",
    "log in",
    "login",
    "enable",
    "grant",
    "authorize",
    "authorise",
    "authenticate",
    "connect",
    "add the",
    "set up",
    "configure",
    "activate",
];
const NEEDS_YOU_SUBJECTS: &[&str] = &[
    "extension",
    "plugin",
    "browser",
    "chrome",
    "account",
    "permission",
    "access",
    "api key",
    "token",
    "integration",
    "mcp server",
    "app store",
    "sign-in",
    "credentials",
    "subscription",
    "license",
    "licence",
];
const NEEDS_YOU_EXPLICIT: &[&str] = &[
    "please install",
    "you need to install",
    "not installed",
    "is not connected",
    "not signed in",
    "requires you to",
    "action required",
    "manual step",
];
const NEEDS_YOU_MAX_CHARS: usize = 400;

/// Cut `text` to at most `max_chars` (counted in characters, as Python counts
/// them), never mid-word. Port of `text_shape.truncate_at_word_boundary` for
/// the two spoken-verbatim slices in this module; the rest of `text_shape`
/// belongs to a later stage.
fn truncate_at_word_boundary(text: &str, max_chars: usize) -> &str {
    if text.chars().count() <= max_chars {
        return text;
    }
    let end = text
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let head = &text[..end];
    let cut = match head.rfind(' ') {
        Some(i) => head[..i].trim_end(),
        None => "",
    };
    if cut.is_empty() {
        // No whitespace inside the budget at all — degenerate input. Hard-cut
        // rather than return nothing.
        if head.trim().is_empty() {
            return &text[..end];
        }
        return head.trim_end();
    }
    cut
}

/// The spoken line when a tool result says the USER must do something, else
/// `None`.
///
/// Deliberately narrow: an explicit phrase, or an action word AND an
/// out-of-band subject in the same short message. A long log is never this — it
/// is output.
/// Borrowed whenever the first line's whitespace is already clean, which is
/// nearly always; only a line that needs collapsing allocates.
pub fn needs_you_line(text: &str) -> Option<Cow<'_, str>> {
    let body = text.trim();
    if body.is_empty() || body.chars().count() > NEEDS_YOU_MAX_CHARS {
        return None;
    }
    let low = body.to_lowercase();
    let explicit = NEEDS_YOU_EXPLICIT.iter().any(|p| low.contains(p));
    let paired = NEEDS_YOU_ACTIONS.iter().any(|a| low.contains(a))
        && NEEDS_YOU_SUBJECTS.iter().any(|s| low.contains(s));
    if !(explicit || paired) {
        return None;
    }
    let first_line = body.lines().next().unwrap_or("");
    match collapse_spaces(first_line) {
        Cow::Borrowed(s) => {
            let cut = truncate_at_word_boundary(s, 160);
            (!cut.is_empty()).then_some(Cow::Borrowed(cut))
        }
        Cow::Owned(s) => {
            let cut = truncate_at_word_boundary(&s, 160).to_owned();
            (!cut.is_empty()).then_some(Cow::Owned(cut))
        }
    }
}

/// `" ".join(s.split())` — collapse every run of whitespace to one space and
/// trim the ends, borrowing when there is nothing to do.
fn collapse_spaces(text: &str) -> Cow<'_, str> {
    let clean = {
        let mut clean =
            !text.starts_with(char::is_whitespace) && !text.ends_with(char::is_whitespace);
        if clean {
            let mut prev_ws = false;
            for c in text.chars() {
                if c.is_whitespace() {
                    if prev_ws || c != ' ' {
                        clean = false;
                        break;
                    }
                    prev_ws = true;
                } else {
                    prev_ws = false;
                }
            }
        }
        clean
    };
    if clean {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Whatever a tool result carries as prose, for [`needs_you_line`].
fn result_text(tool_response: &Value) -> Cow<'_, str> {
    if let Value::String(s) = tool_response {
        return Cow::Borrowed(s);
    }
    let Value::Object(map) = tool_response else {
        return Cow::Borrowed("");
    };
    for key in [
        "content", "text", "message", "result", "stdout", "error", "detail",
    ] {
        let Some(v) = map.get(key) else { continue };
        if let Value::String(s) = v {
            if !s.trim().is_empty() {
                return Cow::Borrowed(s);
            }
        }
        if let Value::Array(items) = v {
            let parts: Vec<&str> = items
                .iter()
                .filter_map(|x| x.get("text").and_then(Value::as_str))
                .collect();
            // Python's `any(parts)` — at least one non-empty string.
            if parts.iter().any(|p| !p.is_empty()) {
                return Cow::Owned(parts.join(" "));
            }
        }
    }
    Cow::Borrowed("")
}

/// How Python renders a value inside an f-string, for the one place a raw JSON
/// value reaches spoken text (`Command failed with exit code {ec}.`).
fn py_display(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Null => "None".to_owned(),
        other => other.to_string(),
    }
}

/// Python's `ec not in (None, 0)` — equality, so `0`, `0.0` and `False` all
/// count as zero.
fn is_zero_or_none(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !*b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        _ => false,
    }
}

/// Terse post-tool narration. Silent on success; speaks on failure — and on a
/// result that is blocked waiting for the user to do something by hand.
pub fn post_tool_event<'a>(tool_name: &'a str, tool_response: &'a Value) -> Option<Narration<'a>> {
    let body: Cow<'a, str> = result_text(tool_response);
    let blocked: Option<Cow<'a, str>> = match body {
        Cow::Borrowed(s) => needs_you_line(s),
        Cow::Owned(s) => needs_you_line(&s).map(|c| Cow::Owned(c.into_owned())),
    };
    if let Some(blocked) = blocked {
        return Some(
            Narration::new("tool_post_needs_you", blocked.clone())
                .with("needs_you", blocked)
                .with("tool", tool_name),
        );
    }
    let Value::Object(map) = tool_response else {
        return None;
    };
    let failed = || -> Narration<'a> {
        let text: Cow<'a, str> = if tool_name.is_empty() {
            Cow::Borrowed("That failed.")
        } else {
            Cow::Owned(format!("{tool_name} failed."))
        };
        Narration::new("tool_post_failure", text).with("tool", tool_name)
    };
    if map.get("success") == Some(&Value::Bool(false)) {
        return Some(failed());
    }
    if let Some(err) = map.get("error") {
        if let Value::String(err) = err {
            if !err.trim().is_empty() {
                // This line is spoken VERBATIM via the deterministic
                // fast path (is_critical_template_event -> always template,
                // never the harness LLM) — a blind [:120] slice could cut
                // mid-word ("...auth modu"). Cut at a word boundary instead so
                // a truncated line still reads as words, never a fragment.
                let first_line = err.trim().lines().next().unwrap_or("");
                let first = truncate_at_word_boundary(first_line, 120);
                return Some(
                    Narration::new("tool_post_failure", format!("Error: {first}."))
                        .with("error", first),
                );
            }
        }
        return Some(failed());
    }
    if tool_name == "Bash" {
        let ec = map
            .get("exit_code")
            .filter(|v| !v.is_null())
            .or_else(|| map.get("exitCode"))
            .unwrap_or(&Value::Null);
        if !is_zero_or_none(ec) {
            // Surface the last meaningful line of stderr so the user hears what
            // *actually* went wrong instead of a flat "Command failed." three
            // calls in a row.
            let stderr = map
                .get("stderr")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let mut last = "";
            if !stderr.is_empty() {
                if let Some(line) = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()) {
                    // The same spoken-verbatim fast path as the error
                    // branch above — word-boundary cut, not a blind slice.
                    last = truncate_at_word_boundary(line, 120);
                }
            }
            let text = if last.is_empty() {
                format!("Command failed with exit code {}.", py_display(ec))
            } else {
                format!("Command failed. {last}")
            };
            return Some(
                Narration::new("tool_post_command_failed", text)
                    .with_json("exit_code", ec.clone())
                    .with("stderr_tail", last),
            );
        }
    }
    None
}

// --- Backwards-compat wrappers (kept for tests and old call sites) ----------

pub fn pre_tool_line<'a>(tool_name: &str, tool_input: &'a Value) -> Option<Cow<'a, str>> {
    pre_tool_event(tool_name, tool_input).map(|n| n.text)
}

pub fn post_tool_line<'a>(tool_name: &'a str, tool_response: &'a Value) -> Option<Cow<'a, str>> {
    post_tool_event(tool_name, tool_response).map(|n| n.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pre(tool: &str, input: Value) -> Option<String> {
        pre_tool_line(tool, &input).map(|t| t.into_owned())
    }

    #[test]
    fn the_python_template_tests_hold() {
        // engine/tests/test_templates.py
        assert_eq!(
            pre("Bash", json!({"command": "git commit -m 'wip'"})).unwrap(),
            "Committing."
        );
        assert_eq!(
            pre(
                "Bash",
                json!({"command": "./s", "description": "Do the thing"})
            )
            .unwrap(),
            "Doing the thing."
        );
        assert_eq!(
            pre("Edit", json!({"file_path": "/Users/x/project/auth.py"})).unwrap(),
            "Editing auth."
        );
        assert_eq!(
            pre("Edit", json!({"file_path": "/Users/x/.zshrc"})).unwrap(),
            "Editing .zshrc."
        );
        assert_eq!(
            pre("Edit", json!({"file_path": "/repo/Dockerfile"})).unwrap(),
            "Editing Dockerfile."
        );
        assert_eq!(
            pre("Edit", json!({"file_path": "/api/src/me.ts"})).unwrap(),
            "Editing me.ts."
        );
        assert_eq!(
            pre("Edit", json!({"file_path": "/proj/app.py"})).unwrap(),
            "Editing app."
        );
        assert!(pre("Read", json!({"file_path": "/tmp/foo.txt"})).is_none());
        assert!(pre("mcp__foo__bar", json!({"x": 1})).is_none());
        assert_eq!(
            pre("Bash", json!({"command": "lsof -i :8080"})).unwrap(),
            "Running lsof."
        );
        assert_eq!(
            pre("Bash", json!({"command": "FOO=bar sudo lsof -i :8080"})).unwrap(),
            "Running lsof."
        );
        assert_eq!(
            pre("Bash", json!({"command": "cd src && grep -rn foo ."})).unwrap(),
            "Searching the codebase."
        );
        assert_eq!(
            pre("Bash", json!({"command": "cd build; make clean"})).unwrap(),
            "Building."
        );
    }

    #[test]
    fn gerunds() {
        assert_eq!(present_continuous("Probe specs"), "Probing specs");
        assert_eq!(present_continuous("Set the env var"), "Setting the env var");
        assert_eq!(
            present_continuous("Cost-estimate the push"),
            "Cost-estimating the push"
        );
        assert_eq!(present_continuous("Running tests"), "Running tests");
        assert_eq!(
            present_continuous("Quick check of logs"),
            "Quick check of logs"
        );
    }

    #[test]
    fn post_tool_speaks_only_when_it_matters() {
        assert!(post_tool_line("Edit", &json!({"filePath": "/a", "success": true})).is_none());
        assert_eq!(
            post_tool_line("Edit", &json!({"success": false})).unwrap(),
            "Edit failed."
        );
        assert_eq!(
            post_tool_line("Bash", &json!({"exit_code": 1})).unwrap(),
            "Command failed with exit code 1."
        );
        assert_eq!(
            post_tool_line(
                "Bash",
                &json!({"exit_code": 1, "stderr": "warning: x\nfatal: cause\n"})
            )
            .unwrap(),
            "Command failed. fatal: cause"
        );
    }

    #[test]
    fn netloc_extraction() {
        assert_eq!(netloc("https://example.com/path"), "example.com");
        assert_eq!(
            netloc("http://user:pw@host.io:8080/a?b=1#c"),
            "user:pw@host.io:8080"
        );
        assert_eq!(netloc("example.com/nope"), "");
        assert_eq!(netloc(""), "");
    }
}
