//! Replays `fixtures/daemon/narration_policy.json` and
//! `speech_shaping.json` (made by the fixture generator, not included, from
//! the Python reference implementation) through the Rust port: the feature sources and
//! `decide` exactly as `Daemon::route_event` builds them, the `_start_speech`
//! shaping, and the floor with the persona's address.
//!
//! Modes retired 2026-10-14: the corpus was recorded with the old mode
//! knobs, so cases recorded under a non-default mode setting are skipped
//! ([`mode_retired`]); every default-config case must still match the
//! recorded Python decision exactly.

use std::cell::RefCell;
use std::path::PathBuf;

use heard_daemon::floor;
use heard_daemon::policy::{self, EventView, FeatureState, Features, Thunks};
use heard_daemon::shape;
use heard_daemon::{BundledPersonas, PersonaSource};
use serde_json::{Map, Value};

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/daemon")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).expect("fixture")).expect("json")
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// Modes retired 2026-10-14: a config that sets one of the retired knobs to
/// anything but its default recorded behaviour Heard no longer has.
fn mode_retired(cfg: &Map<String, Value>) -> bool {
    let is = |k: &str, ok: &[Value]| cfg.get(k).is_some_and(|v| !ok.contains(v));
    is("mode", &[Value::from("copilot")])
        || is("narration_volume", &[Value::from(-1), Value::Null])
        || is("narrate_routine", &[Value::Bool(false)])
        || is("verbosity", &[Value::from("normal")])
        || is("send_mode", &[Value::from(""), Value::from("prefill")])
}

/// Rules the Python table had that went with the modes (retired 2026-10-14).
const RETIRED_RULES: &[&str] = &[
    "volume_budget",
    "focus_attention_drop",
    "focus_alert",
    "focus_no_prompt",
    "focus_fastpath_drop",
    "copilot_tool_tier_suppressed",
    "fastpath_verbosity_drop",
    "fastpath_verbosity_digest",
];

/// Feature thunks that went with the modes: the routine-word budget (an
/// unset `narration_volume` never exhausts it) and the verbosity gates (only
/// reached by tool and prose events, which no longer speak).
const RETIRED_CALLS: &[&str] = &[
    "budget_exhausted",
    "verbosity_pre",
    "verbosity_post",
    "verbosity_prose",
];

fn calls_of(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|c| !RETIRED_CALLS.contains(c))
        .collect()
}

#[test]
fn every_policy_case_matches_the_python() {
    let data = fixture("narration_policy.json");
    let events = data["events"].as_array().expect("events");
    let configs = data["configs"].as_array().expect("configs");
    let states = data["states"].as_array().expect("states");
    let cases: Vec<&Value> = data["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .filter(|c| {
            let cfg = &configs[c["c"].as_u64().unwrap() as usize]["cfg"];
            !mode_retired(cfg.as_object().unwrap())
        })
        .collect();
    assert!(cases.len() >= 3000, "{}", cases.len());
    let rules: Vec<&str> = data["rules"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|r| !RETIRED_RULES.contains(r))
        .collect();
    assert_eq!(rules, policy::RULES);

    let mut bad = Vec::new();
    for case in cases.iter().copied() {
        let ev = &events[case["e"].as_u64().unwrap() as usize];
        let cfg_v = &configs[case["c"].as_u64().unwrap() as usize]["cfg"];
        let cfg: &Map<String, Value> = cfg_v.as_object().unwrap();
        assert_eq!(case["mode"], "copilot");
        let st = &states[case["s"].as_u64().unwrap() as usize];
        let ctx = ev["ctx"].as_object().cloned().unwrap_or_default();
        let view = EventView {
            kind: s(ev, "kind"),
            tag: s(ev, "tag"),
            neutral: s(ev, "neutral"),
            session_id: s(ev, "session_id"),
            abs_path: ctx.get("abs_path").and_then(Value::as_str).unwrap_or(""),
        };
        let persona_name = match cfg.get("persona") {
            None => "raw".to_string(),
            Some(v) => policy::py_str(v),
        };
        let persona = BundledPersonas.load(&persona_name);
        let recent: Vec<String> = st
            .get("recent_edit_paths")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let calls = RefCell::new(Vec::<&'static str>::new());
        let feats = Features::build(
            &view,
            cfg,
            FeatureState {
                first_skill: view.tag == "tool_skill" && !b(st, "skill_announced"),
                multi_agent_active: b(st, "multi_agent_active"),
                recent_edit_paths: &recent,
                harness_enabled: true,
            },
            Thunks {
                prompt_watch_owns: Box::new(|| {
                    calls.borrow_mut().push("prompt_watch_owns");
                    policy::cfg_truthy(cfg, "voice_prompt_announce", true)
                        && b(st, "question_spooled")
                }),
                duplicate_tool_line: Box::new(|| {
                    calls.borrow_mut().push("duplicate_tool_line");
                    b(st, "dup_tool_line")
                }),
            },
        );
        let got = serde_json::json!({
            "speakup": feats.speakup_allowed, "critical": feats.critical,
            "fast_path": feats.fast_path,
        });
        let d = policy::decide(&feats);
        drop(feats);
        let mut diffs = Vec::new();
        for k in ["speakup", "critical", "fast_path"] {
            if got[k] != case[k] {
                diffs.push(format!("{k}: rs={} py={}", got[k], case[k]));
            }
        }
        if d.outcome.as_str() != s(case, "outcome") || d.rule != s(case, "rule") {
            diffs.push(format!(
                "decision rs={}/{} py={}/{}",
                d.outcome.as_str(),
                d.rule,
                case["outcome"],
                case["rule"]
            ));
        }
        let want_calls = calls_of(&case["calls"]);
        if *calls.borrow() != want_calls {
            diffs.push(format!("calls rs={:?} py={want_calls:?}", calls.borrow()));
        }
        if policy::first_run_held(cfg) != b(case, "first_run_held") {
            diffs.push("first_run_held".into());
        }
        // The recorded Focus alert is "<prefix> <prompt>": the prompt (what
        // the CLI's notifier still reads) is its tail.
        let prompt = policy::focus_prompt_text(&view);
        let speech = s(case, "focus_speech");
        if prompt.is_empty() != speech.is_empty() || !speech.ends_with(prompt.as_str()) {
            diffs.push(format!("focus_prompt rs={prompt:?} py={speech:?}"));
        }
        if persona.name != s(case, "persona") || persona.address != s(case, "address") {
            diffs.push(format!("persona rs={persona:?}"));
        }
        if !diffs.is_empty() {
            bad.push(format!(
                "e={} c={} s={} {} {:?}: {}",
                case["e"],
                configs[case["c"].as_u64().unwrap() as usize]["name"],
                st["name"],
                view.kind,
                view.tag,
                diffs.join("; ")
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} cases differ:\n{}",
        bad.len(),
        cases.len(),
        bad[..bad.len().min(25)].join("\n")
    );
}

#[test]
fn the_synthetic_table_matches_decide() {
    let data = fixture("narration_policy.json");
    // Rows with a mode other than copilot, routine narration on, or an
    // exhausted routine budget describe modes retired 2026-10-14.
    let table: Vec<&Value> = data["table"]
        .as_array()
        .expect("table")
        .iter()
        .filter(|row| {
            let f = &row["f"];
            !matches!(s(f, "mode"), "focus" | "companion")
                && !b(f, "narrate_routine")
                && !b(f, "t_budget_exhausted")
        })
        .collect();
    let mut bad = Vec::new();
    for row in table.iter().copied() {
        let f = &row["f"];
        let calls = RefCell::new(Vec::<&'static str>::new());
        let feats = Features {
            kind: s(f, "kind"),
            tag: s(f, "tag"),
            has_text: b(f, "has_text"),
            speakup_allowed: b(f, "speakup_allowed"),
            onboarded: b(f, "onboarded"),
            critical: b(f, "critical"),
            first_skill: b(f, "first_skill"),
            harness_enabled: b(f, "harness_enabled"),
            fast_path: b(f, "fast_path"),
            prompt_watch_owns: Box::new(|| {
                calls.borrow_mut().push("prompt_watch_owns");
                b(f, "t_prompt_watch_owns")
            }),
            duplicate_tool_line: Box::new(|| {
                calls.borrow_mut().push("duplicate_tool_line");
                b(f, "t_duplicate_tool_line")
            }),
        };
        let d = policy::decide(&feats);
        drop(feats);
        let want_calls = calls_of(&row["calls"]);
        if d.outcome.as_str() != s(row, "outcome")
            || d.rule != s(row, "rule")
            || *calls.borrow() != want_calls
        {
            bad.push(format!(
                "{f}: rs={}/{} {:?} py={}/{} {want_calls:?}",
                d.outcome.as_str(),
                d.rule,
                calls.borrow(),
                row["outcome"],
                row["rule"]
            ));
        }
    }
    assert!(table.len() >= 400, "{}", table.len());
    assert!(
        bad.is_empty(),
        "{} differ:\n{}",
        bad.len(),
        bad[..bad.len().min(10)].join("\n")
    );
}

#[test]
fn speech_shaping_matches_the_python() {
    let data = fixture("speech_shaping.json");
    let mut bad = Vec::new();
    for c in data["register"].as_array().unwrap() {
        let stop = c.get("stop");
        let got = shape::register_apply(s(c, "text"), stop, s(c, "kind"), s(c, "tag"));
        if got != s(c, "out") {
            bad.push(format!(
                "register {:?} stop={:?} kind={}: rs={got:?} py={:?}",
                c["text"],
                stop,
                s(c, "kind"),
                c["out"]
            ));
        }
    }
    for c in data["style"].as_array().unwrap() {
        let got = shape::style_line(s(c, "text"), c.get("skill"), s(c, "tag"));
        if got != s(c, "out") {
            bad.push(format!(
                "style {:?} {:?}: rs={got:?} py={:?}",
                c["text"], c["skill"], c["out"]
            ));
        }
    }
    for c in data["sanitize"].as_array().unwrap() {
        let got = shape::sanitize_spoken(s(c, "text"));
        if got != s(c, "out") {
            bad.push(format!(
                "sanitize {:?}: rs={got:?} py={:?}",
                c["text"], c["out"]
            ));
        }
    }
    let floors = data["floor"].as_array().unwrap();
    assert!(floors.iter().any(|c| s(c, "address") == "Sir"));
    for c in floors {
        let persona = BundledPersonas.load(s(c, "persona"));
        assert_eq!(persona.address, s(c, "address"));
        let got = floor::floor_text(
            s(c, "kind"),
            s(c, "neutral"),
            &persona.address,
            s(c, "project"),
        );
        if got != s(c, "out") {
            bad.push(format!(
                "floor {:?}: rs={got:?} py={:?}",
                c["neutral"], c["out"]
            ));
        } else if shape::sanitize_spoken(&got) != s(c, "spoken") {
            bad.push(format!(
                "floor spoken {:?}: rs={:?} py={:?}",
                c["neutral"],
                shape::sanitize_spoken(&got),
                c["spoken"]
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} differ:\n{}",
        bad.len(),
        bad[..bad.len().min(25)].join("\n")
    );
}
