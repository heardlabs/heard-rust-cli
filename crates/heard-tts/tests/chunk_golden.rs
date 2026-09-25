//! The >510-phoneme chunker against the REAL kokoro-onnx 0.6.1 chunker.
//!
//! `tests/fixtures/chunk_golden.json` is produced by
//! `tests/fixtures/gen_chunk_golden.py` (dev-only, `python3 -B`) from
//! `kokoro_onnx.chunker.split_phonemes` / `pause_after` over the same inputs:
//! sentence, clause, word and mid-word cuts, the balancing pass, newlines,
//! and the pause each batch is followed by.

use heard_tts::chunk::{normalize, pause_after, split_phonemes_max, CLAUSE_PAUSE, SENTENCE_PAUSE};
use serde_json::Value;

#[test]
fn every_golden_case_matches_the_python_chunker() {
    let raw = include_str!("fixtures/chunk_golden.json");
    let cases: Vec<Value> = serde_json::from_str(raw).expect("fixture parses");
    assert!(cases.len() >= 10);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let input = case["input"].as_str().unwrap();
        let limit = case["max_length"].as_u64().unwrap() as usize;
        let normalized = normalize(input);
        assert_eq!(normalized, case["normalized"].as_str().unwrap(), "{name}");
        let got = split_phonemes_max(&normalized, limit);
        let want: Vec<String> = case["batches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_str().unwrap().to_string())
            .collect();
        assert_eq!(got, want, "{name}");
        for (b, p) in got.iter().zip(case["pauses"].as_array().unwrap()) {
            assert!(b.chars().count() <= limit, "{name}: {b}");
            assert_eq!(
                pause_after(b, SENTENCE_PAUSE, CLAUSE_PAUSE),
                p.as_f64().unwrap(),
                "{name}: {b}"
            );
        }
    }
}
