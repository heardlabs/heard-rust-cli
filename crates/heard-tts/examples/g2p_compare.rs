//! Side-by-side G2P report: misaki (default) vs espeak (opt-in), with
//! latencies and, when the model is present, WAVs to listen to.
//!
//! ```sh
//! # phonemes + G2P latency only
//! cargo run --release -p heard-tts --features kokoro --example g2p_compare
//! # + espeak column, + WAVs and end-to-end latency
//! HEARD_KOKORO_MODELS_DIR=/path/to/models \
//!   cargo run --release -p heard-tts --features espeak --example g2p_compare -- OUT_DIR
//! ```
//!
//! Nothing is played. WAVs are written to `OUT_DIR` (default: a temp dir).
//! Peak memory is measured from outside, with `/usr/bin/time -l`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use heard_tts::kokoro::{G2p, KokoroTts, MisakiG2p};
use heard_tts::Tts;

const SPIKE: &[&str] = &[
    "The build is green and the tests all pass.",
    "Heard speaks your agent's work out loud, so you can look away.",
    "Kokoro runs locally; nothing leaves this machine.",
];

const EXTRA: &[&str] = &[
    "I refactored the JSON config and opened a PR on GitHub.",
    "Claude ran npm install, then cargo clippy passed with 0 warnings.",
    "The API returned HTTP 404 at 3:45 PM.",
    "Fixed the off-by-one in tokenize_utf8 and bumped v2.1.0.",
    "Heard read the README.md file aloud.",
];

const VOICE: &str = "af_heart";

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn time_g2p(g: &dyn G2p, text: &str, runs: usize) -> (Duration, Duration, Duration) {
    let _ = g.phonemize(text, "en-us"); // warm
    let v: Vec<Duration> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let _ = g.phonemize(text, "en-us").expect("g2p");
            t.elapsed()
        })
        .collect();
    let min = *v.iter().min().expect("runs");
    let max = *v.iter().max().expect("runs");
    (median(v), min, max)
}

fn main() {
    let out_dir = std::env::args().nth(1).map_or_else(
        || std::env::temp_dir().join("heard-g2p-compare"),
        PathBuf::from,
    );
    std::fs::create_dir_all(&out_dir).expect("out dir");

    let t = Instant::now();
    let misaki = MisakiG2p::new();
    let _ = misaki.phonemize("warm", "en-us");
    println!("misaki construct + first call: {:?}", t.elapsed());

    #[cfg(feature = "espeak")]
    let espeak = {
        let e = heard_tts::kokoro::EspeakCliG2p::new();
        e.is_available().then_some(e)
    };

    println!("\n## Phonemes\n");
    for text in SPIKE.iter().chain(EXTRA) {
        println!("text:   {text}");
        println!(
            "misaki: {}",
            misaki.phonemize(text, "en-us").expect("misaki")
        );
        #[cfg(feature = "espeak")]
        if let Some(e) = &espeak {
            println!("espeak: {}", e.phonemize(text, "en-us").expect("espeak"));
        }
        println!();
    }

    println!("## G2P latency per sentence (median / min / max)\n");
    for (i, text) in SPIKE.iter().enumerate() {
        let (m, lo, hi) = time_g2p(&misaki, text, 1000);
        println!("sentence {i} misaki: {m:?} / {lo:?} / {hi:?} over 1000 runs");
        #[cfg(feature = "espeak")]
        if let Some(e) = &espeak {
            let (m, lo, hi) = time_g2p(e, text, 20);
            println!("sentence {i} espeak: {m:?} / {lo:?} / {hi:?} over 20 runs");
        }
    }

    let Some(models) = std::env::var("HEARD_KOKORO_MODELS_DIR")
        .ok()
        .map(PathBuf::from)
    else {
        println!("\n(no HEARD_KOKORO_MODELS_DIR — skipping synthesis)");
        return;
    };

    println!("\n## End-to-end synth (Tts::synth: G2P + graph + trim), median of 5\n");
    let tts = KokoroTts::new(&models).expect("model present");
    let t = Instant::now();
    let _ = tts
        .synth(SPIKE[0], VOICE, 1.0, "en-us")
        .expect("warm-up synth");
    println!("model load + first synth: {:?}", t.elapsed());

    for (i, text) in SPIKE.iter().enumerate() {
        let times: Vec<Duration> = (0..5)
            .map(|_| {
                let t = Instant::now();
                let _ = tts.synth(text, VOICE, 1.0, "en-us").expect("synth");
                t.elapsed()
            })
            .collect();
        let m = median(times);
        let audio = tts.synth(text, VOICE, 1.0, "en-us").expect("synth");
        let pcm = audio.as_pcm().expect("pcm");
        let path = out_dir.join(format!("misaki-{i}.wav"));
        audio.write(&path).expect("write wav");
        println!(
            "sentence {i} misaki: median {m:?}, {:.3}s audio (trimmed), {} samples -> {}",
            pcm.duration_secs(),
            pcm.samples.len(),
            path.display()
        );
    }

    #[cfg(feature = "espeak")]
    if espeak.is_some() {
        let tts = KokoroTts::with_g2p(&models, Box::new(heard_tts::kokoro::EspeakCliG2p::new()))
            .expect("model present");
        let _ = tts
            .synth(SPIKE[0], VOICE, 1.0, "en-us")
            .expect("warm-up synth");
        for (i, text) in SPIKE.iter().enumerate() {
            let times: Vec<Duration> = (0..5)
                .map(|_| {
                    let t = Instant::now();
                    let _ = tts.synth(text, VOICE, 1.0, "en-us").expect("synth");
                    t.elapsed()
                })
                .collect();
            let m = median(times);
            let audio = tts.synth(text, VOICE, 1.0, "en-us").expect("synth");
            let pcm = audio.as_pcm().expect("pcm");
            let path = out_dir.join(format!("espeak-{i}.wav"));
            audio.write(&path).expect("write wav");
            println!(
                "sentence {i} espeak: median {m:?}, {:.3}s audio (trimmed), {} samples -> {}",
                pcm.duration_secs(),
                pcm.samples.len(),
                path.display()
            );
        }
    }
}
