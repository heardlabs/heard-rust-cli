//! Edition-aware hook merging, for any edition that embeds the core.
//!
//! heard-cli's own install ([`crate::install`]) is one caller of
//! [`merge_hooks`]: its marker, its `heard-hook` path and its event set go in
//! through a [`HookSpec`] with [`Placement::OwnGroup`]. Another edition
//! passes its own recogniser, command, events and entry options, and may pick
//! [`Placement::SharedCatchAll`] — the layout older installers wrote, where
//! the hook joins the event's first matcher-less group instead of getting a
//! group of its own.
//!
//! [`merge_shared`] is the single-command form of the shared layout (a hook
//! with an optional tool matcher), and [`edit_hooks_file`] is the whole
//! read-validate-merge-write transaction: lock, parse, change, and — only if
//! the document changed — back up and write atomically.
//!
//! Everything here works on the order-preserving [`Json`], so a user's keys,
//! other tools' hooks and their order survive every merge.

use std::path::{Path, PathBuf};

use crate::fsutil::{self, FileNames};
use crate::json::{self, Json, Obj};
use crate::InstallError;

/// Where a merged hook goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// A new `{"hooks":[entry]}` group of its own at the end of the event's
    /// array. Hooks of ours are removed from EVERY event first, except that
    /// the first one already equal to the wanted entry stays where it is (so
    /// a re-run changes nothing). Groups and events emptied by the removal
    /// go too. (heard-cli's layout.)
    OwnGroup,
    /// For each wanted event only: our hooks are filtered out of its groups
    /// (a group emptied this way stays, as an empty group), then the entry
    /// is merged with [`merge_shared`] into the first group with no matcher.
    SharedCatchAll,
}

/// What to install: which commands are ours, which command to write, on
/// which events, with which entry options.
pub struct HookSpec<'a> {
    pub events: &'a [&'a str],
    /// The exact command string to write.
    pub command: &'a str,
    /// Recognises a hook command as ours (any older form included).
    pub is_ours: &'a dyn Fn(&str) -> bool,
    /// The complete hook entry for one event.
    pub entry: &'a dyn Fn(&str) -> Json,
    pub placement: Placement,
}

/// The `command` string of a hook entry.
pub fn command_of(hook: &Json) -> Option<&str> {
    hook.get("command").and_then(Json::as_str)
}

/// Every (event, command) in the file, in file order.
pub fn all_commands(doc: &Json) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(hooks) = doc.get("hooks").and_then(Json::as_obj) {
        for (event, groups) in hooks {
            for g in groups.as_arr().into_iter().flatten() {
                for h in g.get("hooks").and_then(Json::as_arr).into_iter().flatten() {
                    if let Some(c) = command_of(h) {
                        out.push((event.clone(), c.to_owned()));
                    }
                }
            }
        }
    }
    out
}

fn matcher_of(group: &Json) -> Option<&Json> {
    group.get("matcher")
}

/// The hooks of `event` whose group matches: `matcher = None` means the
/// catch-all groups (no matcher, `""` or `"*"`), `Some(m)` exactly `m`.
pub fn entries<'a>(doc: &'a Json, event: &str, matcher: Option<&str>) -> Vec<&'a Json> {
    let mut out = Vec::new();
    let Some(groups) = doc
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(Json::as_arr)
    else {
        return out;
    };
    for g in groups {
        let m = match matcher_of(g) {
            None => Some(""),
            Some(Json::Str(s)) => Some(s.as_str()),
            Some(_) => None,
        };
        let hit = match matcher {
            None => matches!(m, Some("") | Some("*")),
            Some(want) => m == Some(want),
        };
        if hit {
            out.extend(g.get("hooks").and_then(Json::as_arr).into_iter().flatten());
        }
    }
    out
}

fn hooks_obj(doc: &mut Json) -> &mut Obj {
    let root = doc
        .as_obj_mut()
        .expect("validated: the top level is an object");
    json::entry(root, "hooks", || Json::Obj(Vec::new()))
        .as_obj_mut()
        .expect("validated: `hooks` is an object")
}

fn event_arr<'a>(hooks: &'a mut Obj, event: &str) -> &'a mut Vec<Json> {
    json::entry(hooks, event, || Json::Arr(Vec::new()))
        .as_arr_mut()
        .expect("validated: every event is an array")
}

/// Add `command` to `event`, sharing a compatible group and never
/// duplicating it. `matcher = None` merges into the first group with no
/// matcher (or `""`); `Some(m)` into the first group whose matcher is `m`.
/// Before adding, the same command is removed from every group it would
/// collide with (with no matcher: from all groups; with one: from that
/// matcher's groups); a group emptied by that removal is dropped, a group
/// that was already empty is kept. A missing group is appended as
/// `{"hooks":[…]}` (plus `"matcher"` after `hooks` when one is given).
/// The entry is `{"type":"command","command":…}` followed by `options`.
pub fn merge_shared(
    doc: &mut Json,
    event: &str,
    command: &str,
    matcher: Option<&str>,
    options: Obj,
) {
    let hooks = hooks_obj(doc);
    let groups = event_arr(hooks, event);
    let mut kept: Vec<Json> = Vec::new();
    for group in groups.drain(..) {
        let Json::Obj(mut members) = group else {
            kept.push(group);
            continue;
        };
        let gm = members
            .iter()
            .find(|(k, _)| k == "matcher")
            .map(|(_, v)| v.clone());
        let Some(idx) = members.iter().position(|(k, _)| k == "hooks") else {
            kept.push(Json::Obj(members));
            continue;
        };
        let inner = members[idx].1.as_arr().cloned().unwrap_or_default();
        let was_empty = inner.is_empty();
        let remaining: Vec<Json> = inner
            .into_iter()
            .filter(|h| {
                command_of(h) != Some(command)
                    || matcher.is_some_and(|m| gm.as_ref().and_then(Json::as_str) != Some(m))
            })
            .collect();
        if !remaining.is_empty() || was_empty {
            members[idx].1 = Json::Arr(remaining);
            kept.push(Json::Obj(members));
        }
    }
    let mut entry: Obj = vec![
        ("type".into(), Json::Str("command".into())),
        ("command".into(), Json::Str(command.into())),
    ];
    for (k, v) in options {
        match entry.iter_mut().find(|(ek, _)| *ek == k) {
            Some(slot) => slot.1 = v,
            None => entry.push((k, v)),
        }
    }
    let want = matcher.unwrap_or("");
    let target = kept.iter().position(|g| {
        let m = match matcher_of(g) {
            None => Some(""),
            Some(Json::Str(s)) => Some(s.as_str()),
            Some(_) => None,
        };
        m == Some(want)
    });
    let idx = match target {
        Some(i) => i,
        None => {
            let mut g: Obj = vec![("hooks".into(), Json::Arr(Vec::new()))];
            if let Some(m) = matcher {
                g.push(("matcher".into(), Json::Str(m.into())));
            }
            kept.push(Json::Obj(g));
            kept.len() - 1
        }
    };
    if let Some(inner) = kept[idx].get_mut("hooks").and_then(Json::as_arr_mut) {
        inner.push(Json::Obj(entry));
    }
    *groups = kept;
}

/// Remove every hook whose command satisfies `pred` from `event`'s groups,
/// keeping the groups themselves (even when emptied). Creates the event as
/// `[]` when missing, as the shared-layout installers always have.
pub fn filter_event(doc: &mut Json, event: &str, pred: &dyn Fn(&str) -> bool) -> usize {
    let hooks = hooks_obj(doc);
    let groups = event_arr(hooks, event);
    let mut removed = 0;
    for g in groups.iter_mut() {
        if let Some(inner) = g.get_mut("hooks").and_then(Json::as_arr_mut) {
            let before = inner.len();
            inner.retain(|h| !command_of(h).is_some_and(pred));
            removed += before - inner.len();
        }
    }
    removed
}

/// Remove our hooks from every event, except that for events in `keep` the
/// first hook of ours that already equals the wanted entry stays where it
/// is. Groups and events emptied by the removal go too; ones that were
/// empty already are left alone. Returns (hooks removed, events kept).
pub fn strip_ours(
    doc: &mut Json,
    is_ours: &dyn Fn(&str) -> bool,
    keep: &[(&str, Json)],
) -> (usize, Vec<String>) {
    let mut removed = 0;
    let mut kept = Vec::new();
    let Some(root) = doc.as_obj_mut() else {
        return (0, kept);
    };
    let Some(hooks_idx) = root.iter().position(|(k, _)| k == "hooks") else {
        return (0, kept);
    };
    let Some(hooks) = root[hooks_idx].1.as_obj_mut() else {
        return (0, kept);
    };
    let mut emptied_events = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        let want = keep.iter().find(|(e, _)| e == event).map(|(_, j)| j);
        let mut have_kept = false;
        let Some(groups) = groups.as_arr_mut() else {
            continue;
        };
        let before_groups = groups.len();
        groups.retain_mut(|g| {
            let Some(inner) = g.get_mut("hooks").and_then(Json::as_arr_mut) else {
                return true;
            };
            let before = inner.len();
            inner.retain(|h| {
                let ours = command_of(h).is_some_and(is_ours);
                if !ours {
                    return true;
                }
                if !have_kept && want == Some(h) {
                    have_kept = true;
                    return true;
                }
                removed += 1;
                false
            });
            // Drop a group only if WE emptied it.
            !(inner.is_empty() && before > 0)
        });
        if have_kept {
            kept.push(event.clone());
        }
        if groups.is_empty() && before_groups > 0 {
            emptied_events.push(event.clone());
        }
    }
    hooks.retain(|(k, _)| !emptied_events.contains(k));
    if hooks.is_empty() && !emptied_events.is_empty() {
        root.remove(hooks_idx);
    }
    (removed, kept)
}

/// Install `spec`'s hook on each of its events.
pub fn merge_hooks(doc: &mut Json, spec: &HookSpec<'_>) {
    match spec.placement {
        Placement::OwnGroup => {
            let wanted: Vec<(&str, Json)> =
                spec.events.iter().map(|e| (*e, (spec.entry)(e))).collect();
            let (_, kept) = strip_ours(doc, spec.is_ours, &wanted);
            let hooks = hooks_obj(doc);
            for (event, entry) in wanted {
                if kept.iter().any(|k| k == event) {
                    continue;
                }
                event_arr(hooks, event)
                    .push(Json::Obj(vec![("hooks".into(), Json::Arr(vec![entry]))]));
            }
        }
        Placement::SharedCatchAll => {
            for event in spec.events {
                filter_event(doc, event, spec.is_ours);
                let entry = (spec.entry)(event);
                let options: Obj = entry
                    .as_obj()
                    .map(|o| {
                        o.iter()
                            .filter(|(k, _)| k != "type" && k != "command")
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                merge_shared(doc, event, spec.command, None, options);
            }
        }
    }
}

/// What [`edit_hooks_file`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    pub changed: bool,
    /// The pre-change copy, when an existing non-empty file was changed.
    pub backup: Option<PathBuf>,
}

/// The whole transaction on one hooks file: take the edition's lock, read
/// and validate (a file that is not valid JSON, or whose `hooks` is not the
/// agent shape, is refused and left byte-for-byte alone), run `change`, and
/// — only if the document now differs — back up the old file and write the
/// new one atomically (two-space JSON, trailing newline, key order kept).
/// A missing file that `change` leaves as `{}` is not created.
pub fn edit_hooks_file<T>(
    path: &Path,
    names: &FileNames,
    change: impl FnOnce(&mut Json) -> T,
) -> Result<(T, Edit), InstallError> {
    let path = fsutil::resolve_target(path);
    let _lock = fsutil::Lock::acquire_named(&path, names)?;
    let (text, doc) = crate::load(&path)?;
    let mut after = doc.clone();
    let out = change(&mut after);
    if after == doc {
        return Ok((out, Edit::default()));
    }
    let backup = match &text {
        Some(t) if !t.trim().is_empty() => Some(fsutil::backup_with(&path, names.backup_infix)?),
        _ => None,
    };
    let text = match names.style {
        fsutil::WriteStyle::Pretty => after.to_pretty(),
        fsutil::WriteStyle::PythonIndent2 => after.dumps_py(Some(2)) + "\n",
    };
    fsutil::atomic_write_named(&path, text.as_bytes(), names)?;
    Ok((
        out,
        Edit {
            changed: true,
            backup,
        },
    ))
}

/// Validate a parsed hooks document the way [`edit_hooks_file`] does.
pub fn validate(doc: &Json) -> Result<(), String> {
    crate::validate(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> Json {
        Json::parse(s).unwrap()
    }

    #[test]
    fn merge_shared_joins_the_catch_all_group_and_dedups() {
        let mut d = j(
            r#"{"a":1,"hooks":{"Stop":[{"matcher":"Bash","hooks":[{"command":"x"}]},{"hooks":[{"type":"command","command":"me"}]}]}}"#,
        );
        merge_shared(
            &mut d,
            "Stop",
            "me",
            None,
            vec![("async".into(), Json::Bool(true))],
        );
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            r#"{"a":1,"hooks":{"Stop":[{"matcher":"Bash","hooks":[{"command":"x"}]},{"hooks":[{"type":"command","command":"me","async":true}]}]}}"#
        );
    }

    #[test]
    fn merge_shared_with_matcher_appends_hooks_then_matcher() {
        let mut d = j(r#"{}"#);
        merge_shared(
            &mut d,
            "PreToolUse",
            "q",
            Some("AskUserQuestion"),
            Vec::new(),
        );
        assert_eq!(
            serde_json::to_string(&d).unwrap(),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"q"}],"matcher":"AskUserQuestion"}]}}"#
        );
    }

    #[test]
    fn entries_catch_all_includes_star() {
        let d = j(
            r#"{"hooks":{"Stop":[{"matcher":"*","hooks":[{"command":"a"}]},{"matcher":"B","hooks":[{"command":"b"}]},{"hooks":[{"command":"c"}]}]}}"#,
        );
        let got: Vec<_> = entries(&d, "Stop", None)
            .into_iter()
            .filter_map(command_of)
            .collect();
        assert_eq!(got, ["a", "c"]);
        let got: Vec<_> = entries(&d, "Stop", Some("B"))
            .into_iter()
            .filter_map(command_of)
            .collect();
        assert_eq!(got, ["b"]);
    }
}
