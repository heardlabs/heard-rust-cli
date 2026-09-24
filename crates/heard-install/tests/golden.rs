//! Installer golden tests. Every test builds its own temp HOME; nothing here
//! reads or writes the real `~/.claude` or `~/.codex`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use heard_install::{
    classify, hook_command, install, status, uninstall, InstallError, InstallOptions, Owner,
    Target, BACKUP_INFIX, COMMAND_FILES, COMMAND_MARKER,
};

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");

fn golden(name: &str, bin: &Path) -> String {
    fs::read_to_string(Path::new(GOLDEN).join(name))
        .unwrap()
        .replace("@BIN@", &bin.to_string_lossy())
}

/// A temp HOME plus an executable fake `heard-hook` in `<home>/bin`.
struct Home {
    root: PathBuf,
    bin: PathBuf,
}

impl Home {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "heard-install-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("bin")).unwrap();
        let bin = root.join("bin").join("heard-hook");
        write_exe(&bin);
        Self { root, bin }
    }

    fn opts(&self) -> InstallOptions {
        InstallOptions::new(&self.bin)
    }

    fn settings(&self) -> PathBuf {
        self.root.join(".claude/settings.json")
    }

    fn codex_hooks(&self) -> PathBuf {
        self.root.join(".codex/hooks.json")
    }

    fn put(&self, rel: &str, text: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn read(&self, p: &Path) -> String {
        fs::read_to_string(p).unwrap()
    }

    fn backups(&self, file: &Path) -> Vec<PathBuf> {
        let prefix = format!(
            "{}{}",
            file.file_name().unwrap().to_string_lossy(),
            BACKUP_INFIX
        );
        let mut v: Vec<PathBuf> = fs::read_dir(file.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect();
        v.sort();
        v
    }

    /// Everything in a directory (to prove no tmp/lock files are left behind).
    fn listing(&self, dir: &str) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(self.root.join(dir))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn write_exe(p: &Path) {
    fs::write(p, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

fn value(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

/// Order-blind meaning, with empty event arrays / an empty `hooks` treated
/// as absent — to Claude Code and Codex they are the same thing.
fn meaning(text: &str) -> serde_json::Value {
    let mut v = value(text);
    if let Some(obj) = v.as_object_mut() {
        if let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) {
            hooks.retain(|_, groups| groups.as_array().is_none_or(|a| !a.is_empty()));
        }
        if obj
            .get("hooks")
            .and_then(|h| h.as_object())
            .is_some_and(|h| h.is_empty())
        {
            obj.remove("hooks");
        }
    }
    v
}

// ---------------------------------------------------------------------------
// Claude Code settings.json

#[test]
fn install_into_a_missing_settings_file_matches_the_golden() {
    let h = Home::new("empty");
    let r = install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    assert!(r.changed);
    assert_eq!(r.backup, None, "nothing to back up");
    assert_eq!(
        h.read(&h.settings()),
        golden("claude_empty.after.json", &h.bin)
    );
    // No temp or lock file left behind.
    assert_eq!(h.listing(".claude"), ["commands", "settings.json"]);
}

#[test]
fn install_into_an_empty_object_and_an_empty_file() {
    for text in ["{}", "", "{}\n"] {
        let h = Home::new("emptyobj");
        h.put(".claude/settings.json", text);
        install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
        assert_eq!(
            h.read(&h.settings()),
            golden("claude_empty.after.json", &h.bin)
        );
    }
}

#[test]
fn install_beside_foreign_hooks_matches_the_golden_and_keeps_key_order() {
    let h = Home::new("foreign");
    let before = golden("claude_foreign.before.json", &h.bin);
    h.put(".claude/settings.json", &before);
    let r = install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    assert!(r.changed);
    assert_eq!(
        h.read(&h.settings()),
        golden("claude_foreign.after.json", &h.bin)
    );
    // The backup is the original, byte for byte, and private.
    let backup = r.backup.expect("a backup of the existing file");
    assert_eq!(h.read(&backup), before);
    assert_eq!(
        fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn uninstall_leaves_foreign_content_semantically_identical() {
    let h = Home::new("roundtrip");
    let before = golden("claude_foreign.before.json", &h.bin);
    h.put(".claude/settings.json", &before);
    install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    let r = uninstall(&h.root, Target::ClaudeCode).unwrap();
    assert!(r.changed);
    assert_eq!(r.hooks_removed, 4);
    let after = h.read(&h.settings());
    assert_eq!(meaning(&after), meaning(&before));
    // With no pre-existing empty event array the round trip is byte-exact.
    let h2 = Home::new("roundtrip-bytes");
    let before2 = before.replace(",\n    \"Stop\": []", "");
    assert_ne!(before2, before, "fixture edit applied");
    h2.put(".claude/settings.json", &before2);
    install(&h2.root, Target::ClaudeCode, &h2.opts()).unwrap();
    uninstall(&h2.root, Target::ClaudeCode).unwrap();
    assert_eq!(h2.read(&h2.settings()), before2);
}

#[test]
fn install_then_uninstall_from_nothing_leaves_an_empty_object() {
    let h = Home::new("fromnothing");
    install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    uninstall(&h.root, Target::ClaudeCode).unwrap();
    assert_eq!(h.read(&h.settings()), "{}\n");
    assert!(!h.root.join(".claude/commands/heard-mode.md").exists());
}

#[test]
fn reinstall_is_idempotent_and_backs_up_once() {
    let h = Home::new("idem");
    h.put(
        ".claude/settings.json",
        &golden("claude_foreign.before.json", &h.bin),
    );
    let first = install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    let bytes = h.read(&h.settings());
    let mtime = fs::metadata(h.settings()).unwrap().modified().unwrap();
    for _ in 0..3 {
        let again = install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
        assert!(!again.changed);
        assert_eq!(again.backup, None);
        assert!(again.commands_written.is_empty());
    }
    assert_eq!(h.read(&h.settings()), bytes);
    assert_eq!(
        fs::metadata(h.settings()).unwrap().modified().unwrap(),
        mtime,
        "an idempotent re-run does not even rewrite the file"
    );
    assert_eq!(h.backups(&h.settings()), vec![first.backup.unwrap()]);
}

#[test]
fn a_reformatted_but_equal_file_is_not_rewritten() {
    // Same meaning, 4-space indent: nothing to change, so nothing is written.
    let h = Home::new("fmt");
    install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    let four_ordered = golden("claude_empty.after.json", &h.bin).replace("  ", "    ");
    h.put(".claude/settings.json", &four_ordered);
    let r = install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    assert!(!r.changed);
    assert_eq!(h.read(&h.settings()), four_ordered);
}

#[test]
fn a_moved_binary_replaces_the_old_hook_everywhere() {
    let h = Home::new("upgrade");
    let old = h.root.join("old").join("heard-hook");
    fs::create_dir_all(old.parent().unwrap()).unwrap();
    write_exe(&old);
    install(&h.root, Target::ClaudeCode, &InstallOptions::new(&old)).unwrap();
    let r = install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    assert!(r.changed);
    assert_eq!(
        h.read(&h.settings()),
        golden("claude_empty.after.json", &h.bin)
    );
    let st = status(&h.root, Target::ClaudeCode, Some(&h.bin));
    assert!(st.installed && st.marker_ok);
}

#[test]
fn duplicated_or_stray_hooks_of_ours_are_collapsed() {
    let h = Home::new("dups");
    install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    // Hand-add a duplicate Stop hook and one on an event we don't use.
    let mut v = value(&h.read(&h.settings()));
    let cmd = hook_command(&h.bin, Target::ClaudeCode);
    let extra = serde_json::json!({"hooks":[{"type":"command","command":cmd}]});
    v["hooks"]["Stop"]
        .as_array_mut()
        .unwrap()
        .push(extra.clone());
    v["hooks"]["Notification"] = serde_json::json!([extra]);
    h.put(
        ".claude/settings.json",
        &serde_json::to_string_pretty(&v).unwrap(),
    );
    let st = status(&h.root, Target::ClaudeCode, Some(&h.bin));
    assert!(st.installed);
    assert!(!st.marker_ok, "duplicates need a re-install");
    install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    assert_eq!(
        meaning(&h.read(&h.settings())),
        meaning(&golden("claude_empty.after.json", &h.bin))
    );
}

#[test]
fn paid_app_hooks_refuse_install_and_leave_the_file_untouched() {
    let h = Home::new("paid");
    let before = golden("claude_paid.before.json", &h.bin);
    h.put(".claude/settings.json", &before);
    let err = install(&h.root, Target::ClaudeCode, &h.opts()).unwrap_err();
    match &err {
        InstallError::PaidAppHooks { commands, .. } => {
            assert_eq!(commands.len(), 1, "{commands:?}");
            assert!(commands[0].starts_with("Stop: "));
        }
        other => panic!("wrong error: {other}"),
    }
    let msg = err.to_string();
    assert!(
        msg.contains("--force") && msg.contains("paid Heard app"),
        "{msg}"
    );
    assert_eq!(h.read(&h.settings()), before);
    assert!(h.backups(&h.settings()).is_empty());
    assert!(!h.root.join(".claude/commands").exists());

    let st = status(&h.root, Target::ClaudeCode, Some(&h.bin));
    assert_eq!(st.paid_app_hooks.len(), 1);
    assert!(!st.installed);
}

#[test]
fn force_installs_beside_the_paid_app_and_uninstall_keeps_its_hooks() {
    let h = Home::new("paidforce");
    let before = golden("claude_paid.before.json", &h.bin);
    h.put(".claude/settings.json", &before);
    let mut opts = h.opts();
    opts.force = true;
    let r = install(&h.root, Target::ClaudeCode, &opts).unwrap();
    assert!(r.warnings.iter().any(|w| w.contains("twice")));
    let st = status(&h.root, Target::ClaudeCode, Some(&h.bin));
    assert!(st.installed && st.marker_ok);
    assert_eq!(st.paid_app_hooks.len(), 1);
    uninstall(&h.root, Target::ClaudeCode).unwrap();
    assert_eq!(h.read(&h.settings()), before);
}

#[test]
fn every_paid_app_spelling_is_detected() {
    for cmd in [
        "PYTHONDONTWRITEBYTECODE=1 /usr/bin/python3 -m heard.hook claude-code",
        "/Applications/Heard.app/Contents/MacOS/heard-hook claude-code",
        "/opt/whatever/heard-hook claude-code",
        "'/Users/x/Library/Application Support/heard/bin/heard-hook' codex",
    ] {
        assert_eq!(classify(cmd), Owner::PaidApp, "{cmd}");
    }
    for cmd in [
        "/Users/x/.local/bin/heard-hook claude-code --edition heard-cli",
        "'/Users/x/my bin/heard-hook' codex --edition heard-cli",
        "/Users/x/.local/bin/heard-hook claude-code --edition=heard-cli",
    ] {
        assert_eq!(classify(cmd), Owner::HeardCli, "{cmd}");
    }
    for cmd in [
        "~/.claude/hooks/guard.sh",
        "echo heard-cli",
        "/Users/x/.heard/permission-bridge.sh",
        "",
    ] {
        assert_eq!(classify(cmd), Owner::Foreign, "{cmd}");
    }
}

#[test]
fn invalid_files_are_refused_and_never_replaced() {
    for (text, want_json_err) in [
        ("{not json", true),
        ("[]", false),
        (r#"{"hooks": []}"#, false),
        (r#"{"hooks": {"Stop": {}}}"#, false),
        (r#"{"hooks": {"Stop": [{"matcher": ""}]}}"#, false),
    ] {
        let h = Home::new("invalid");
        h.put(".claude/settings.json", text);
        let err = install(&h.root, Target::ClaudeCode, &h.opts()).unwrap_err();
        assert_eq!(
            matches!(err, InstallError::InvalidJson { .. }),
            want_json_err,
            "{text}: {err}"
        );
        assert!(err.to_string().contains("never overwrites"));
        assert_eq!(h.read(&h.settings()), text);
        assert!(uninstall(&h.root, Target::ClaudeCode).is_err());
        assert_eq!(h.read(&h.settings()), text);
        let st = status(&h.root, Target::ClaudeCode, Some(&h.bin));
        assert!(st.config_error.is_some() && !st.installed);
    }
}

#[test]
fn a_missing_or_relative_hook_binary_is_refused() {
    let h = Home::new("nobin");
    let err = install(
        &h.root,
        Target::ClaudeCode,
        &InstallOptions::new(h.root.join("nope/heard-hook")),
    )
    .unwrap_err();
    assert!(matches!(err, InstallError::HookBinaryMissing(_)));
    let err = install(
        &h.root,
        Target::ClaudeCode,
        &InstallOptions::new("heard-hook"),
    )
    .unwrap_err();
    assert!(matches!(err, InstallError::HookBinaryNotAbsolute(_)));
    assert!(!h.settings().exists());
}

#[test]
fn a_symlinked_settings_file_is_written_through() {
    let h = Home::new("symlink");
    h.put("dotfiles/claude-settings.json", "{\"model\": \"x\"}\n");
    fs::create_dir_all(h.root.join(".claude")).unwrap();
    std::os::unix::fs::symlink(h.root.join("dotfiles/claude-settings.json"), h.settings()).unwrap();
    install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    assert!(fs::symlink_metadata(h.settings())
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(h
        .read(&h.root.join("dotfiles/claude-settings.json"))
        .contains("--edition heard-cli"));
}

#[test]
fn a_path_with_spaces_is_quoted_and_still_recognised() {
    let h = Home::new("spaces");
    let bin = h.root.join("my bin").join("heard-hook");
    fs::create_dir_all(bin.parent().unwrap()).unwrap();
    write_exe(&bin);
    install(&h.root, Target::ClaudeCode, &InstallOptions::new(&bin)).unwrap();
    let text = h.read(&h.settings());
    assert!(text.contains(&format!(
        "\"'{}' claude-code --edition heard-cli\"",
        bin.display()
    )));
    let st = status(&h.root, Target::ClaudeCode, Some(&bin));
    assert!(st.installed && st.marker_ok && st.binary_ok);
    assert_eq!(st.hook_binary.as_deref(), Some(bin.as_path()));
}

// ---------------------------------------------------------------------------
// Slash command files

#[test]
fn command_files_round_trip() {
    let h = Home::new("cmds");
    let r = install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    assert_eq!(r.commands_written.len(), 7);
    let names: Vec<String> = COMMAND_FILES.iter().map(|c| c.file_name()).collect();
    assert_eq!(
        names,
        [
            "heard-mode.md",
            "heard-voice.md",
            "heard-persona.md",
            "heard-speed.md",
            "heard-pause.md",
            "heard-resume.md",
            "heard-status.md"
        ]
    );
    let mode = h.read(&h.root.join(".claude/commands/heard-mode.md"));
    assert_eq!(
        mode,
        "---\n\
         description: Set Heard's narration mode (copilot, companion or focus)\n\
         argument-hint: \"[copilot|companion|focus]\"\n\
         allowed-tools: Bash(heard:*)\n\
         ---\n\
         <!-- heard-cli:managed: written by `heard install claude-code`, removed by `heard uninstall claude-code` -->\n\
         \n\
         Run exactly one Bash command and nothing else: `heard mode --porcelain`, followed by each word of the arguments below wrapped in single quotes (write a single quote inside a word as '\\''). With no arguments, run `heard mode --porcelain` alone. Never run the arguments themselves. Then reply with only the one line it printed.\n\
         \n\
         Arguments: $ARGUMENTS\n"
    );
    for cf in COMMAND_FILES {
        let text = h.read(&h.root.join(".claude/commands").join(cf.file_name()));
        assert!(text.contains(COMMAND_MARKER));
        assert!(text.contains("allowed-tools: Bash(heard:*)\n"));
        assert!(text.contains(&format!("`heard {} --porcelain`", cf.verb)));
        assert!(text.ends_with("Arguments: $ARGUMENTS\n"));
        // Never a shell line with the user's text spliced in.
        assert!(!text.contains("!`"), "{text}");
        assert!(text.starts_with("---\ndescription: "));
    }
    let st = status(&h.root, Target::ClaudeCode, Some(&h.bin));
    assert!(st.command_files_ok);
    assert!(st
        .command_files
        .iter()
        .all(|c| c.present && c.ours && c.current));

    // A user's own file with the same name and an unrelated file survive both
    // install and uninstall.
    let h2 = Home::new("cmds-foreign");
    h2.put(".claude/commands/heard-pause.md", "my own pause\n");
    h2.put(".claude/commands/deploy.md", "deploy\n");
    let r = install(&h2.root, Target::ClaudeCode, &h2.opts()).unwrap();
    assert_eq!(r.commands_written.len(), 6);
    assert!(r.warnings.iter().any(|w| w.contains("heard-pause.md")));
    let st = status(&h2.root, Target::ClaudeCode, Some(&h2.bin));
    assert!(!st.command_files_ok);
    let rm = uninstall(&h2.root, Target::ClaudeCode).unwrap();
    assert_eq!(rm.commands_removed.len(), 6);
    assert_eq!(
        h2.listing(".claude/commands"),
        ["deploy.md", "heard-pause.md"]
    );
    assert_eq!(
        h2.read(&h2.root.join(".claude/commands/heard-pause.md")),
        "my own pause\n"
    );
}

#[test]
fn a_stale_command_file_of_ours_is_rewritten() {
    let h = Home::new("cmds-stale");
    h.put(
        ".claude/commands/heard-mode.md",
        &format!("old body\n{COMMAND_MARKER} -->\n"),
    );
    let r = install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    assert_eq!(r.commands_written.len(), 7);
    assert!(h
        .read(&h.root.join(".claude/commands/heard-mode.md"))
        .contains("`heard mode --porcelain`"));
}

// ---------------------------------------------------------------------------
// Codex hooks.json

#[test]
fn codex_install_into_nothing_matches_the_golden() {
    let h = Home::new("codex");
    let r = install(&h.root, Target::Codex, &h.opts()).unwrap();
    assert!(r.changed && r.warnings.is_empty());
    assert!(r.commands_written.is_empty());
    assert_eq!(
        h.read(&h.codex_hooks()),
        golden("codex_empty.after.json", &h.bin)
    );
    assert!(
        !h.root.join(".claude").exists(),
        "codex touches nothing of Claude's"
    );
    let st = status(&h.root, Target::Codex, Some(&h.bin));
    assert!(st.installed && st.marker_ok && st.binary_ok && st.command_files_ok);
    assert!(st.command_files.is_empty());
}

#[test]
fn codex_foreign_hooks_survive_the_round_trip() {
    let h = Home::new("codex-foreign");
    let before = "{\n  \"hooks\": {\n    \"PreToolUse\": [\n      {\n        \"matcher\": \"Bash\",\n        \"hooks\": [\n          {\n            \"type\": \"command\",\n            \"command\": \"/usr/local/bin/audit\"\n          }\n        ]\n      }\n    ]\n  },\n  \"version\": 1\n}\n";
    h.put(".codex/hooks.json", before);
    install(&h.root, Target::Codex, &h.opts()).unwrap();
    assert!(h.read(&h.codex_hooks()).contains("/usr/local/bin/audit"));
    let again = install(&h.root, Target::Codex, &h.opts()).unwrap();
    assert!(!again.changed);
    uninstall(&h.root, Target::Codex).unwrap();
    assert_eq!(h.read(&h.codex_hooks()), before);
    assert_eq!(h.backups(&h.codex_hooks()).len(), 2, "install + uninstall");
}

#[test]
fn codex_paid_app_hooks_refuse() {
    let h = Home::new("codex-paid");
    h.put(
        ".codex/hooks.json",
        r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/usr/bin/python3 -m heard.hook codex","timeout":60}]}]}}"#,
    );
    assert!(matches!(
        install(&h.root, Target::Codex, &h.opts()),
        Err(InstallError::PaidAppHooks { .. })
    ));
}

#[test]
fn codex_disabled_hooks_warn_without_editing_config_toml() {
    for toml in [
        "[features]\nhooks = false\n",
        "model = \"o3\"\n[features]\ncodex_hooks=false # old alias\n",
        "features.hooks = false\n",
    ] {
        let h = Home::new("codex-off");
        h.put(".codex/config.toml", toml);
        let r = install(&h.root, Target::Codex, &h.opts()).unwrap();
        assert_eq!(r.warnings.len(), 1, "{toml}");
        assert!(r.warnings[0].contains("config.toml"));
        assert_eq!(h.read(&h.root.join(".codex/config.toml")), toml);
        assert!(status(&h.root, Target::Codex, None).codex_hooks_disabled);
    }
    for toml in [
        "[features]\nhooks = true\n",
        "[features.hooks]\nx = false\n",
        "hooks = false\n",
    ] {
        let h = Home::new("codex-on");
        h.put(".codex/config.toml", toml);
        let r = install(&h.root, Target::Codex, &h.opts()).unwrap();
        assert!(r.warnings.is_empty(), "{toml}");
    }
}

// ---------------------------------------------------------------------------
// status()

#[test]
fn status_of_a_clean_home() {
    let h = Home::new("status-clean");
    let st = status(&h.root, Target::ClaudeCode, Some(&h.bin));
    assert!(!st.config_exists && st.config_error.is_none());
    assert!(!st.installed && !st.marker_ok && !st.binary_ok);
    assert_eq!(
        st.missing_events,
        ["Stop", "PreToolUse", "PostToolUse", "UserPromptSubmit"]
    );
    assert!(st.paid_app_hooks.is_empty());
    assert!(!st.command_files_ok);
    assert!(st.command_files.iter().all(|c| !c.present));
    // It serialises for `heard doctor --json`.
    let j = serde_json::to_value(&st).unwrap();
    assert_eq!(j["agent"], "claude-code");
}

#[test]
fn status_flags_a_deleted_binary_and_a_different_expected_path() {
    let h = Home::new("status-bin");
    install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    let st = status(&h.root, Target::ClaudeCode, None);
    assert!(st.installed && st.marker_ok && st.binary_ok);
    let other = h.root.join("elsewhere/heard-hook");
    let st = status(&h.root, Target::ClaudeCode, Some(&other));
    assert!(st.installed && !st.marker_ok);
    fs::remove_file(&h.bin).unwrap();
    let st = status(&h.root, Target::ClaudeCode, Some(&h.bin));
    assert!(st.installed && st.marker_ok && !st.binary_ok);
    assert_eq!(st.hook_binary.as_deref(), Some(h.bin.as_path()));
}

#[test]
fn a_partial_install_is_reported_missing_events() {
    let h = Home::new("status-partial");
    install(&h.root, Target::ClaudeCode, &h.opts()).unwrap();
    let mut v = value(&h.read(&h.settings()));
    v["hooks"]
        .as_object_mut()
        .unwrap()
        .remove("UserPromptSubmit");
    h.put(".claude/settings.json", &v.to_string());
    let st = status(&h.root, Target::ClaudeCode, Some(&h.bin));
    assert!(!st.installed && !st.marker_ok);
    assert_eq!(st.missing_events, ["UserPromptSubmit"]);
}

#[test]
fn uninstall_of_nothing_is_a_quiet_no_op() {
    let h = Home::new("uninstall-none");
    let r = uninstall(&h.root, Target::ClaudeCode).unwrap();
    assert!(!r.changed && r.hooks_removed == 0 && r.commands_removed.is_empty());
    assert!(!h.settings().exists());
    let before = "{\"model\":\"x\"}";
    h.put(".claude/settings.json", before);
    let r = uninstall(&h.root, Target::ClaudeCode).unwrap();
    assert!(!r.changed);
    assert_eq!(h.read(&h.settings()), before, "not even reformatted");
    assert!(h.backups(&h.settings()).is_empty());
}
