//! Replays `fixtures/daemon/narration_policy.json` and
//! `speech_shaping.json` (made by the fixture generator, not included, from
//! the Python reference implementation) through the Rust port: the feature sources and
//! `decide` exactly as `Daemon::route_event` builds them, the `_start_speech`
//! shaping, and the floor with the persona's address.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;

use heard_daemon::floor;
use heard_daemon::policy::{self, EventView, FeatureState, Features, Thunks};
use heard_daemon::shape;
use heard_daemon::{BundledPersonas, PersonaSource};
use heard_narrate::verbosity::{self, Cfg, Decision as Verbosity};
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

fn verb(v: &str) -> Verbosity {
    match v {
        "drop" => Verbosity::Drop,
        "digest" => Verbosity::Digest,
        _ => Verbosity::Speak,
    }
}

#[test]
fn every_policy_case_matches_the_python() {
    let data = fixture("narration_policy.json");
    let events = data["events"].as_array().expect("events");
    let configs = data["configs"].as_array().expect("configs");
    let states = data["states"].as_array().expect("states");
    let cases = data["cases"].as_array().expect("cases");
    assert!(cases.len() >= 5000);
    let rules: Vec<&str> = data["rules"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(rules, policy::RULES);

    let mut bad = Vec::new();
    for case in cases {
        let ev = &events[case["e"].as_u64().unwrap() as usize];
        let cfg_v = &configs[case["c"].as_u64().unwrap() as usize]["cfg"];
        let cfg: &Map<String, Value> = cfg_v.as_object().unwrap();
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
        let vcfg = Cfg(cfg_v);
        let density = st.get("density").and_then(Value::as_i64).unwrap_or(0);
        let t0 = Instant::now();
        let budget = RefCell::new(policy::Budget::default());
        if let Some(n) = st.get("budget_spent").and_then(Value::as_u64) {
            budget.borrow_mut().note(n as usize, t0);
        }
        let feats = Features::build(
            &view,
            cfg,
            FeatureState {
                persona_name: &persona.name,
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
                verbosity_pre: Box::new(|| {
                    calls.borrow_mut().push("verbosity_pre");
                    verbosity::classify_pre(&vcfg, view.tag, density)
                }),
                verbosity_post: Box::new(|| {
                    calls.borrow_mut().push("verbosity_post");
                    verbosity::classify_post(&vcfg, view.tag)
                }),
                verbosity_prose: Box::new(|| {
                    calls.borrow_mut().push("verbosity_prose");
                    verbosity::classify_prose(&vcfg)
                }),
                duplicate_tool_line: Box::new(|| {
                    calls.borrow_mut().push("duplicate_tool_line");
                    b(st, "dup_tool_line")
                }),
                budget_exhausted: Box::new(|| {
                    calls.borrow_mut().push("budget_exhausted");
                    let limit =
                        policy::budget_wpm(cfg.get("narration_volume").unwrap_or(&Value::from(-1)));
                    budget.borrow_mut().exhausted(limit, t0)
                }),
            },
        );
        let got = serde_json::json!({
            "mode": feats.mode, "speakup": feats.speakup_allowed, "critical": feats.critical,
            "fast_path": feats.fast_path, "focus_attention": feats.focus_attention,
            "focus_template": feats.focus_template, "focus_alert_text": feats.focus_alert_text,
        });
        let d = policy::decide(&feats);
        drop(feats);
        let mut diffs = Vec::new();
        for k in [
            "mode",
            "speakup",
            "critical",
            "fast_path",
            "focus_attention",
            "focus_template",
            "focus_alert_text",
        ] {
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
        let want_calls: Vec<&str> = case["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        if *calls.borrow() != want_calls {
            diffs.push(format!("calls rs={:?} py={want_calls:?}", calls.borrow()));
        }
        if policy::first_run_held(cfg) != b(case, "first_run_held") {
            diffs.push("first_run_held".into());
        }
        let speech = policy::focus_prompt_speech(&view, &persona.name);
        if speech != s(case, "focus_speech") {
            diffs.push(format!(
                "focus_speech rs={speech:?} py={:?}",
                case["focus_speech"]
            ));
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
    let table = data["table"].as_array().expect("table");
    let mut bad = Vec::new();
    for row in table {
        let f = &row["f"];
        let calls = RefCell::new(Vec::<&'static str>::new());
        let feats = Features {
            kind: s(f, "kind"),
            tag: s(f, "tag"),
            has_text: b(f, "has_text"),
            mode: match s(f, "mode") {
                "focus" => "focus",
                "companion" => "companion",
                _ => "copilot",
            },
            speakup_allowed: b(f, "speakup_allowed"),
            onboarded: b(f, "onboarded"),
            narrate_routine: b(f, "narrate_routine"),
            critical: b(f, "critical"),
            first_skill: b(f, "first_skill"),
            focus_attention: b(f, "focus_attention"),
            focus_template: b(f, "focus_template"),
            focus_alert_text: b(f, "focus_alert_text"),
            harness_enabled: b(f, "harness_enabled"),
            fast_path: b(f, "fast_path"),
            prompt_watch_owns: Box::new(|| {
                calls.borrow_mut().push("prompt_watch_owns");
                b(f, "t_prompt_watch_owns")
            }),
            verbosity_pre: Box::new(|| {
                calls.borrow_mut().push("verbosity_pre");
                verb(s(f, "t_verbosity_pre"))
            }),
            verbosity_post: Box::new(|| {
                calls.borrow_mut().push("verbosity_post");
                verb(s(f, "t_verbosity_post"))
            }),
            verbosity_prose: Box::new(|| {
                calls.borrow_mut().push("verbosity_prose");
                verb(s(f, "t_verbosity_prose"))
            }),
            duplicate_tool_line: Box::new(|| {
                calls.borrow_mut().push("duplicate_tool_line");
                b(f, "t_duplicate_tool_line")
            }),
            budget_exhausted: Box::new(|| {
                calls.borrow_mut().push("budget_exhausted");
                b(f, "t_budget_exhausted")
            }),
        };
        let d = policy::decide(&feats);
        drop(feats);
        let want_calls: Vec<&str> = row["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
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
    assert!(table.len() >= 1000);
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
