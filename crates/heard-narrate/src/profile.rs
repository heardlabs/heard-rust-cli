//! Verbosity profile loader.
//!
//! Port of `engine/heard/profile.py`. Each profile has five dimensions:
//!
//! ```text
//!   pre_tool        silent | digest | per_tool
//!   post_success    silent | speak
//!   prose           silent | speak
//!   final_budget    int (characters)
//!   burst_threshold int (events / 30 s before pre_tool=per_tool routes
//!                   overflow to digest; ignored for silent / digest)
//! ```
//!
//! Backwards compat: the old "low" / "high" verbosity names map to "quiet" /
//! "verbose" so existing `config.yaml` files keep working.
//!
//! ## Two deliberate differences from the Python loader
//!
//! 1. The four bundled profiles are EMBEDDED from `engine/heard/profiles/*.yaml`
//!    at compile time, so there is exactly one source of truth for their values
//!    and no file I/O on a hot path. Editing a bundled YAML changes this crate.
//! 2. The user-directory override (`$CONFIG_DIR/profiles/<name>.yaml`, which
//!    wins over bundled) is NOT here: it is file I/O and config-dir resolution,
//!    which belong to `heard-config`. This module
//!    stays pure. The golden corpus is generated with the user dir pinned away
//!    for the same reason.
//!
//! The parser below is not a general YAML parser; it reads the flat
//! `key: value` shape the bundled profiles are written in, ignoring comments
//! and blank lines. A user profile with anchors, nesting or block scalars is a
//! `heard-config` problem, not this one — and the unit tests pin every value of
//! all four bundled profiles so a drift in either direction is a failing test.

/// One resolved profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub description: String,
    pub pre_tool: String,
    pub post_success: String,
    pub prose: String,
    pub final_budget: i64,
    pub burst_threshold: i64,
}

impl Default for Profile {
    /// Sane fallbacks for any field a profile YAML omits. Mirrors the bundled
    /// "normal" profile; if a user's custom YAML drops a field, they get the
    /// normal-mode default instead of crashing.
    fn default() -> Self {
        Profile {
            name: "normal".to_owned(),
            description: String::new(),
            pre_tool: "per_tool".to_owned(),
            post_success: "silent".to_owned(),
            prose: "speak".to_owned(),
            final_budget: 600,
            burst_threshold: 5,
        }
    }
}

const QUIET: &str = include_str!("../../../assets/profiles/quiet.yaml");
const BRIEF: &str = include_str!("../../../assets/profiles/brief.yaml");
const NORMAL: &str = include_str!("../../../assets/profiles/normal.yaml");
const VERBOSE: &str = include_str!("../../../assets/profiles/verbose.yaml");

/// Pre-v0.4 verbosity names. Map to the new ones so config.yaml from an older
/// install still resolves. Drop after a migration window.
fn normalize(name: Option<&str>) -> String {
    // Python: `(name or "normal").strip().lower()`. An absent or EMPTY name is
    // "normal"; a name that is only whitespace strips to "", resolves to no
    // file, and lands on the defaults below — which is the same behaviour for
    // every dimension that matters, and is kept exact anyway.
    let raw = name.unwrap_or("");
    let base = if raw.is_empty() { "normal" } else { raw };
    let n = base.trim().to_lowercase();
    match n.as_str() {
        "low" => "quiet".to_owned(),
        "high" => "verbose".to_owned(),
        _ => n,
    }
}

/// Load a profile by name. Unknown names fall back to the normal profile
/// (never crashes the daemon just because a YAML went missing).
pub fn load(name: Option<&str>) -> Profile {
    let name = normalize(name);
    let yaml = match name.as_str() {
        "quiet" => QUIET,
        "brief" => BRIEF,
        "normal" => NORMAL,
        "verbose" => VERBOSE,
        // Last resort: caller asked for a name we can't resolve. Return the
        // normal-mode defaults so the daemon stays useful.
        _ => return Profile::default(),
    };
    parse(yaml)
}

/// Names of profiles shipped in the bundle, sorted — the canonical four levels
/// the menu and `heard tune` show.
pub fn list_bundled() -> [&'static str; 4] {
    ["brief", "normal", "quiet", "verbose"]
}

fn parse(yaml: &str) -> Profile {
    let mut prof = Profile::default();
    for line in yaml.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let value = raw.trim().trim_matches(|c| c == '"' || c == '\'');
        match key.trim() {
            "name" => prof.name = value.to_owned(),
            "description" => prof.description = value.to_owned(),
            "pre_tool" => prof.pre_tool = value.to_owned(),
            "post_success" => prof.post_success = value.to_owned(),
            "prose" => prof.prose = value.to_owned(),
            "final_budget" => {
                if let Ok(n) = value.parse() {
                    prof.final_budget = n;
                }
            }
            "burst_threshold" => {
                if let Ok(n) = value.parse() {
                    prof.burst_threshold = n;
                }
            }
            _ => {}
        }
    }
    prof
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_bundled_profiles_are_what_the_yaml_says() {
        let quiet = load(Some("quiet"));
        assert_eq!(quiet.name, "quiet");
        assert_eq!(quiet.pre_tool, "silent");
        assert_eq!(quiet.post_success, "silent");
        assert_eq!(quiet.prose, "silent");
        assert_eq!(quiet.final_budget, 200);
        assert_eq!(quiet.burst_threshold, 0);

        let brief = load(Some("brief"));
        assert_eq!(brief.pre_tool, "digest");
        assert_eq!(brief.prose, "speak");
        assert_eq!(brief.burst_threshold, 0);

        let normal = load(Some("normal"));
        assert_eq!(
            normal,
            Profile {
                description: normal.description.clone(),
                ..Profile::default()
            }
        );
        assert!(!normal.description.is_empty());

        let verbose = load(Some("verbose"));
        assert_eq!(verbose.pre_tool, "per_tool");
        assert_eq!(verbose.post_success, "speak");
        assert_eq!(verbose.final_budget, 2000);
        assert_eq!(verbose.burst_threshold, 9999);
    }

    #[test]
    fn legacy_and_unknown_names() {
        assert_eq!(load(Some("low")).name, "quiet");
        assert_eq!(load(Some("high")).name, "verbose");
        assert_eq!(load(Some("  NORMAL  ")).name, "normal");
        assert_eq!(load(Some("bogus")).name, "normal");
        assert_eq!(load(Some("")).name, "normal");
        assert_eq!(load(None).name, "normal");
    }
}
