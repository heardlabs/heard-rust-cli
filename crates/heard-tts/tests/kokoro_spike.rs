//! The Kokoro spike: does the ONNX graph actually speak, under `ort`?
//!
//! **Gated twice.** Nothing here runs unless the crate is built with
//! `--features kokoro` *and* `HEARD_KOKORO_MODELS_DIR` points at a directory
//! holding `kokoro-v1.0.onnx` and `voices-v1.0.bin` at their pinned sizes. CI
//! without the 325 MB model runs the whole rest of the suite and skips these,
//! which is the point of the gate: the model is opt-in here exactly as it is
//! for a user.
//!
//! Nothing is ever played. Every test writes a WAV to a temp directory and
//! asserts on its bytes.
//!
//! The three sentences and the phoneme strings they compare against were
//! captured from `kokoro_onnx.Tokenizer`.

#![cfg(feature = "kokoro")]

use std::path::PathBuf;
use std::time::Instant;

#[cfg(feature = "espeak")]
use heard_tts::kokoro::{g2p::G2p, EspeakCliG2p};
use heard_tts::kokoro::{KokoroTts, PhonemesAsGiven, SAMPLE_RATE};
use heard_tts::Tts;

/// The spike corpus, with the phonemes Python's `kokoro_onnx` produced for it.
const CORPUS: &[(&str, &str)] = &[
    (
        "The build is green and the tests all pass.",
        "ðə bˈɪld ɪz ɡɹˈiːn ænd ðə tˈɛsts ˈɔːl pˈæs.",
    ),
    (
        "Heard speaks your agent's work out loud, so you can look away.",
        "hˈɜːd spˈiːks jʊɹ ˈeɪdʒənts wˈɜːk ˈaʊt lˈaʊd, sˌoʊ juː kæn lˈʊk ɐwˈeɪ.",
    ),
    (
        "Kokoro runs locally; nothing leaves this machine.",
        "kəkˈɔːɹoʊ ɹˈʌnz lˈoʊkəli; nˈʌθɪŋ lˈiːvz ðɪs məʃˈiːn.",
    ),
];

/// The voice the spike measures with — Kokoro's default in the upstream demos.
const VOICE: &str = "af_heart";

fn models_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var("HEARD_KOKORO_MODELS_DIR").ok()?);
    heard_tts::select::kokoro_is_downloaded(&dir).then_some(dir)
}

fn out_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("heard-tts-kokoro-spike");
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn the_graph_speaks_the_corpus_from_pythons_own_phonemes() {
    let Some(dir) = models_dir() else { return };

    // PhonemesAsGiven takes G2P out of the comparison entirely: whatever comes
    // out is the ONNX side alone.
    let tts = KokoroTts::with_g2p(&dir, Box::new(PhonemesAsGiven)).expect("model present");

    let load_start = Instant::now();
    let names = tts.voice_names().expect("voices load");
    eprintln!(
        "model + voices loaded in {:?}; {} voices",
        load_start.elapsed(),
        names.len()
    );
    assert!(names.contains(&VOICE.to_string()), "voices: {names:?}");
    assert!(
        names.len() > 20,
        "expected the full voice pack, got {names:?}"
    );

    for (i, (text, phonemes)) in CORPUS.iter().enumerate() {
        let started = Instant::now();
        let pcm = tts
            .synth_phonemes(phonemes, VOICE, 1.0, text)
            .expect("synthesis should succeed");
        let elapsed = started.elapsed();

        assert_eq!(pcm.sample_rate, SAMPLE_RATE);
        assert!(
            pcm.duration_secs() > 0.5,
            "sentence {i} produced only {:.3}s",
            pcm.duration_secs()
        );

        // Speech, not silence: a real utterance has samples well off zero and
        // a non-trivial RMS. An all-zero buffer would pass a length check.
        let peak = pcm.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        let rms =
            (pcm.samples.iter().map(|s| s * s).sum::<f32>() / pcm.samples.len() as f32).sqrt();
        assert!(peak > 0.05, "sentence {i} peaks at {peak}, that is silence");
        assert!(rms > 0.005, "sentence {i} RMS {rms}, that is silence");

        let path = out_dir().join(format!("rust-{i}.wav"));
        pcm.write_wav(&path).expect("write wav");
        let size = std::fs::metadata(&path).expect("stat").len();
        assert_eq!(size as usize, 44 + pcm.samples.len() * 2);

        eprintln!(
            "sentence {i}: {:.3}s audio, {:?} latency, {} Hz, {} bytes -> {}",
            pcm.duration_secs(),
            elapsed,
            pcm.sample_rate,
            size,
            path.display()
        );
    }
}

#[cfg(feature = "espeak")]
#[test]
fn the_full_path_speaks_when_espeak_is_available() {
    let Some(dir) = models_dir() else { return };
    let g2p = EspeakCliG2p::new();
    if !g2p.is_available() {
        return;
    }

    let tts = KokoroTts::with_g2p(&dir, Box::new(EspeakCliG2p::new())).expect("model present");
    for (i, (text, want_phonemes)) in CORPUS.iter().enumerate() {
        // The G2P agrees with Python's before the model ever sees it.
        assert_eq!(&g2p.phonemize(text, "en-us").unwrap(), want_phonemes);

        let started = Instant::now();
        let audio = tts.synth(text, VOICE, 1.0, "en-us").expect("synth");
        let elapsed = started.elapsed();

        let pcm = audio.as_pcm().expect("Kokoro returns PCM");
        assert_eq!(pcm.sample_rate, SAMPLE_RATE);
        assert!(pcm.duration_secs() > 0.5);

        let path = out_dir().join(format!("rust-e2e-{i}.wav"));
        audio.write(&path).expect("write");
        eprintln!(
            "e2e sentence {i}: {:.3}s audio in {elapsed:?} -> {}",
            pcm.duration_secs(),
            path.display()
        );
    }
}

#[test]
fn speed_changes_the_duration_in_the_direction_it_should() {
    let Some(dir) = models_dir() else { return };
    let tts = KokoroTts::with_g2p(&dir, Box::new(PhonemesAsGiven)).expect("model present");
    let (text, phonemes) = CORPUS[0];

    let slow = tts.synth_phonemes(phonemes, VOICE, 0.8, text).unwrap();
    let fast = tts.synth_phonemes(phonemes, VOICE, 1.5, text).unwrap();
    assert!(
        fast.duration_secs() < slow.duration_secs(),
        "1.5x ({:.3}s) should be shorter than 0.8x ({:.3}s)",
        fast.duration_secs(),
        slow.duration_secs()
    );
}

/// The parity pin: the Rust graph output equals `kokoro-onnx`'s, sample count
/// and waveform both.
///
/// Captured from `Kokoro.create(..., is_phonemes=True, trim=False)` — G2P out
/// of the picture on both sides, and `trim=False` because `create` defaults to
/// `trim=True` and runs a librosa silence-trim over the model's output. That
/// trim is a post-process, not part of the graph, and it is what made the
/// first comparison look like a 20-30% duration divergence when the two sides
/// were in fact producing identical audio. It is NOT ported here — see the
/// report.
#[test]
fn the_waveform_matches_kokoro_onnx_sample_for_sample() {
    let Some(dir) = models_dir() else { return };
    let tts = KokoroTts::with_g2p(&dir, Box::new(PhonemesAsGiven)).expect("model present");

    // (n_samples, peak, rms) from the Python, untrimmed.
    let expected: &[(usize, f32, f32)] = &[
        (69_600, 0.462_257, 0.066_817),
        (93_600, 0.415_269, 0.069_948),
        (90_600, 0.522_181, 0.070_416),
    ];

    for (i, ((text, phonemes), (n, want_peak, want_rms))) in
        CORPUS.iter().zip(expected.iter()).enumerate()
    {
        let pcm = tts.synth_phonemes(phonemes, VOICE, 1.0, text).unwrap();

        // Exact: the graph is deterministic, so a different length means a
        // different input, not a different float unit.
        assert_eq!(
            pcm.samples.len(),
            *n,
            "sentence {i}: Rust produced {} samples, kokoro-onnx produced {n}",
            pcm.samples.len()
        );

        let peak = pcm.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        let rms = (pcm
            .samples
            .iter()
            .map(|s| f64::from(*s) * f64::from(*s))
            .sum::<f64>()
            / pcm.samples.len() as f64)
            .sqrt() as f32;

        // The two sides run DIFFERENT ONNX Runtime builds — `ort` 2.0.0-rc.13
        // wraps ORT 1.28, the Python venv has onnxruntime 1.30.0 — so kernel
        // selection and fused-op scheduling differ and the last few bits of
        // each sample with them. RMS, an average over tens of thousands of
        // samples, stays within 1e-3. The peak is a max over a single sample
        // and so is the most sensitive statistic there is: the worst observed
        // gap is 1.05e-3 on 0.415, i.e. 0.25%. 5e-3 is loose enough for that
        // and still tight enough that a genuinely wrong style vector, token
        // sequence or padding — all of which change the waveform grossly —
        // fails.
        assert!(
            (peak - want_peak).abs() < 5e-3,
            "sentence {i}: peak {peak} vs Python {want_peak}"
        );
        assert!(
            (rms - want_rms).abs() < 1e-3,
            "sentence {i}: RMS {rms} vs Python {want_rms}"
        );
        eprintln!("sentence {i}: {n} samples, peak {peak:.6}, rms {rms:.6} — matches Python");
    }
}

/// Median latency over repeated runs, for the report's numbers.
///
/// Single-shot timings on a laptop are dominated by scheduling noise — the
/// first pass of the spike had the same sentence at 1.37 s and 2.16 s across
/// two runs. Off by default; set `HEARD_KOKORO_BENCH=1` to run it.
#[test]
fn median_latency_over_repeats() {
    if std::env::var("HEARD_KOKORO_BENCH").is_err() {
        return;
    }
    let Some(dir) = models_dir() else { return };
    let tts = KokoroTts::with_g2p(&dir, Box::new(PhonemesAsGiven)).expect("model present");

    // One warm-up so the first run's lazy allocation is not counted.
    let _ = tts
        .synth_phonemes(CORPUS[0].1, VOICE, 1.0, CORPUS[0].0)
        .unwrap();

    for (i, (text, phonemes)) in CORPUS.iter().enumerate() {
        let mut times: Vec<f64> = (0..5)
            .map(|_| {
                let t = Instant::now();
                let pcm = tts.synth_phonemes(phonemes, VOICE, 1.0, text).unwrap();
                let secs = t.elapsed().as_secs_f64();
                assert!(!pcm.samples.is_empty());
                secs
            })
            .collect();
        times.sort_by(f64::total_cmp);
        let pcm = tts.synth_phonemes(phonemes, VOICE, 1.0, text).unwrap();
        let audio = pcm.duration_secs();
        let median = times[times.len() / 2];
        eprintln!(
            "BENCH sentence {i}: median {median:.3}s over {} runs (min {:.3}, max {:.3}); \
             audio {audio:.3}s; RTF {:.3}",
            times.len(),
            times[0],
            times[times.len() - 1],
            median / audio
        );
    }
}

#[test]
fn an_unknown_voice_is_a_sentence_not_a_panic() {
    let Some(dir) = models_dir() else { return };
    let tts = KokoroTts::with_g2p(&dir, Box::new(PhonemesAsGiven)).expect("model present");
    let err = tts
        .synth_phonemes("ðə bˈɪld", "af_nonesuch", 1.0, "the build")
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("af_nonesuch"), "{msg}");
    assert!(msg.contains("did you mean"), "{msg}");
}

#[test]
fn two_voices_produce_different_audio_for_the_same_words() {
    let Some(dir) = models_dir() else { return };
    let tts = KokoroTts::with_g2p(&dir, Box::new(PhonemesAsGiven)).expect("model present");
    let (text, phonemes) = CORPUS[0];

    let names = tts.voice_names().unwrap();
    let Some(other) = names.iter().find(|n| n.as_str() != VOICE) else {
        return;
    };

    let a = tts.synth_phonemes(phonemes, VOICE, 1.0, text).unwrap();
    let b = tts.synth_phonemes(phonemes, other, 1.0, text).unwrap();
    // The style vector genuinely reaches the graph: same tokens, different
    // voice, different waveform.
    assert_ne!(a.samples, b.samples, "{VOICE} and {other} sound identical");
}

/// The default path: `KokoroTts::new` phonemises with misaki in process, and
/// what comes out is real 24 kHz speech of a plausible length, trimmed.
///
/// "Plausible length" is measured against the Python/espeak reference: the
/// same sentence, untrimmed, was 2.900 / 3.900 / 3.775 s. misaki's phonemes
/// differ, so the length does too — but by tens of percent at most, not by
/// the factor a dropped word or a garbage token run would produce.
#[test]
fn the_default_misaki_path_speaks_24khz_audio_of_plausible_length() {
    let Some(dir) = models_dir() else { return };
    let tts = KokoroTts::new(&dir).expect("model present");
    let reference_secs = [2.900, 3.900, 3.775];

    for (i, ((text, _), reference)) in CORPUS.iter().zip(reference_secs).enumerate() {
        let raw = tts.synth_pcm(text, VOICE, 1.0).expect("raw synth");
        let audio = tts.synth(text, VOICE, 1.0, "en-us").expect("synth");
        let pcm = audio.as_pcm().expect("Kokoro returns PCM");

        assert_eq!(pcm.sample_rate, SAMPLE_RATE);
        assert_eq!(raw.sample_rate, 24_000);

        let raw_secs = raw.duration_secs();
        assert!(
            (raw_secs / reference - 1.0).abs() < 0.35,
            "sentence {i}: misaki gave {raw_secs:.3}s raw, espeak reference {reference:.3}s"
        );

        let peak = pcm.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        let rms =
            (pcm.samples.iter().map(|s| s * s).sum::<f32>() / pcm.samples.len() as f32).sqrt();
        assert!(peak > 0.05, "sentence {i} peaks at {peak}, that is silence");
        assert!(rms > 0.005, "sentence {i} RMS {rms}, that is silence");

        // The silence trim still applies on the misaki path: `synth` returns
        // exactly the trimmed interval of the raw graph output.
        let (start, end) = heard_tts::trim::trim_interval(&raw.samples);
        assert!(
            end - start < raw.samples.len(),
            "sentence {i}: nothing was trimmed"
        );
        assert_eq!(
            pcm.samples.as_slice(),
            &raw.samples[start..end],
            "sentence {i}"
        );

        let path = out_dir().join(format!("misaki-{i}.wav"));
        audio.write(&path).expect("write");
        let bytes = std::fs::read(&path).expect("read back");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
            24_000
        );
        eprintln!(
            "misaki sentence {i}: raw {raw_secs:.3}s, trimmed {:.3}s -> {}",
            pcm.duration_secs(),
            path.display()
        );
    }
}
