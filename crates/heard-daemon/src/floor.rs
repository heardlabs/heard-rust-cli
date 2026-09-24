//! `Daemon._floor_text` and `Daemon._final_lead`, ported verbatim.
//!
//! The no-LLM floor is what keeps Heard from going silent when the brain
//! punts: silence from an ambient tool reads as "broken". The rules, unchanged:
//!
//! * **`final`** — a short final is read as-is; a long one (the "verbatim
//!   wall") is replaced by a bounded LEAD of the message prefixed with the
//!   project, because the floor has no LLM to summarise it and reading the
//!   wall aloud is worse than an honest partial.
//! * **`intermediate`** — dropped. A mid-stream blip the brain could not
//!   shape is not worth a canned line; the next event narrates.
//! * **everything else** (tool-ish) — the neutral TEMPLATE, which is already
//!   a clean one-liner, never verbatim.

use heard_narrate::markdown;

/// `Daemon._FLOOR_FINAL_VERBATIM_MAX`. A final shorter than this is already
/// spoken-friendly; longer means it is the agent's raw closing text.
pub const FLOOR_FINAL_VERBATIM_MAX: usize = 240;

/// `_final_lead`'s default budget.
pub const FINAL_LEAD_MAX_CHARS: usize = 220;

/// `_floor_text(kind, neutral, persona, project)`.
///
/// `address` is `persona.address` — the form of address the persona uses
/// ("sir", a name, …), or `""`. With no persona the daemon
/// passes `""` and the shaping below is a no-op.
pub fn floor_text(kind: &str, neutral: &str, address: &str, project: &str) -> String {
    if kind == "final" {
        // Python measures with `len()`, which counts CHARACTERS.
        if !neutral.is_empty() && neutral.chars().count() <= FLOOR_FINAL_VERBATIM_MAX {
            return with_address(neutral, address);
        }
        let project = project.trim();
        let lead = final_lead(neutral, FINAL_LEAD_MAX_CHARS);
        if !lead.is_empty() {
            return if project.is_empty() {
                with_address(&lead, address)
            } else {
                with_address(&format!("On {project}, {lead}"), address)
            };
        }
        if !project.is_empty() {
            return with_address(&format!("That's wrapped up on {project}"), address);
        }
        return with_address("That's wrapped up", address);
    }
    if kind == "intermediate" {
        return String::new();
    }
    neutral.to_owned()
}

/// `_with_addr` from inside `_floor_text`: strip trailing full stops, append
/// the address when the text does not already end with it, and end with one.
fn with_address(text: &str, address: &str) -> String {
    let trimmed = text.trim_end_matches('.');
    if !address.is_empty() && !trimmed.to_lowercase().ends_with(&address.to_lowercase()) {
        return format!("{trimmed}, {address}.");
    }
    format!("{trimmed}.")
}

/// `_final_lead(neutral, max_chars=…)` — the first sentence or two of a long
/// final, markdown-stripped. `""` when nothing is usable.
pub fn final_lead(neutral: &str, max_chars: usize) -> String {
    let stripped = markdown::strip(neutral);
    let text = stripped.trim();
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for part in split_sentences(text) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if !out.is_empty() && out.chars().count() + 1 + part.chars().count() > max_chars {
            break;
        }
        if out.is_empty() {
            out.push_str(part);
        } else {
            out.push(' ');
            out.push_str(part);
        }
        if out.chars().count() >= max_chars {
            break;
        }
    }
    if out.chars().count() > max_chars {
        // One run-on sentence longer than the budget — cut on the last word
        // boundary so we don't slice mid-word. Python's `rsplit(" ", 1)[0]`
        // returns the WHOLE string when there is no space in it.
        let head: String = out.chars().take(max_chars).collect();
        out = match head.rsplit_once(' ') {
            Some((left, _)) => left.to_owned(),
            None => head,
        };
    }
    out.trim().to_owned()
}

/// `re.split(r"(?<=[.!?])\s+", text)` — split AFTER a sentence-ending mark,
/// on the run of whitespace that follows it.
///
/// Hand-rolled rather than pulled through `regex`: the pattern is one
/// lookbehind over three ASCII characters, and the narration path is meant to
/// be `&str` work over text the process already owns.
fn split_sentences(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'.' || c == b'!' || c == b'?' {
            // The split point is the whitespace run immediately after.
            let mut end = i + 1;
            let mut ws = end;
            while ws < bytes.len() && (bytes[ws] as char).is_ascii_whitespace() {
                ws += 1;
            }
            if ws > end {
                parts.push(&text[start..end]);
                start = ws;
                i = ws;
                end = ws;
                let _ = end;
                continue;
            }
        }
        i += 1;
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    if parts.is_empty() {
        parts.push(text);
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_final_is_read_as_is() {
        assert_eq!(
            floor_text("final", "All tests pass", "", "heard"),
            "All tests pass."
        );
    }

    #[test]
    fn a_short_final_keeps_one_full_stop() {
        assert_eq!(floor_text("final", "All done.", "", ""), "All done.");
    }

    #[test]
    fn an_intermediate_is_dropped() {
        assert_eq!(
            floor_text("intermediate", "thinking about it", "", "heard"),
            ""
        );
    }

    #[test]
    fn a_tool_event_keeps_its_template() {
        assert_eq!(
            floor_text("tool_pre", "Editing auth.py", "", "heard"),
            "Editing auth.py"
        );
    }

    #[test]
    fn a_long_final_becomes_a_project_prefixed_lead() {
        let wall = format!("First sentence here. {}", "x".repeat(400));
        let out = floor_text("final", &wall, "", "heard");
        assert_eq!(out, "On heard, First sentence here.");
    }

    #[test]
    fn a_long_final_with_no_usable_lead_falls_back_to_the_canned_line() {
        // Nothing but blank lines: long enough to miss the verbatim branch,
        // and `_final_lead` strips it to "".
        let wall = "\n".repeat(300);
        assert_eq!(
            floor_text("final", &wall, "", "heard"),
            "That's wrapped up on heard."
        );
        assert_eq!(floor_text("final", &wall, "", ""), "That's wrapped up.");
    }

    #[test]
    fn the_address_is_appended_once() {
        assert_eq!(floor_text("final", "All good", "sir", ""), "All good, sir.");
        assert_eq!(
            floor_text("final", "All good, sir.", "sir", ""),
            "All good, sir."
        );
    }

    #[test]
    fn a_lead_stops_at_the_budget_on_a_word_boundary() {
        let run_on = "word ".repeat(200);
        let lead = final_lead(&run_on, 20);
        assert!(lead.chars().count() <= 20, "{lead:?}");
        assert!(!lead.ends_with(' '));
        assert!(lead.starts_with("word word"));
    }

    #[test]
    fn sentences_split_after_the_mark() {
        assert_eq!(split_sentences("A. B! C? D"), vec!["A.", "B!", "C?", "D"]);
        assert_eq!(split_sentences("no.split"), vec!["no.split"]);
    }
}
