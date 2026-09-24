//! Leading/trailing silence trim — the port of `kokoro_onnx/trim.py`, which is
//! itself librosa's `effects.trim` extracted so `kokoro-onnx` does not need
//! librosa.
//!
//! # Why this exists
//!
//! `kokoro_onnx.Kokoro.create` takes `trim=True` **by default** and runs this
//! over every batch the graph returns. `heard/tts/kokoro.py` calls `create`
//! with the default, so the Python has always shipped trimmed audio. An early Rust
//! prototype did not, and returned ~25% more
//! samples per utterance — all of it the model's leading/trailing near-silence.
//!
//! # What it computes, exactly
//!
//! With the defaults `create` uses (`top_db=60`, `ref=np.max`,
//! `frame_length=2048`, `hop_length=512`) and a mono signal:
//!
//! 1. `rms`: pad `frame_length // 2` zeros each side (`center=True`,
//!    `pad_mode="constant"`), slice frames of 2048 every 512 samples, and take
//!    `sqrt(mean(x²))` per frame — all in `float32`.
//! 2. `amplitude_to_db(rms, ref=np.max, top_db=None)`: `10·log10(max(1e-10,
//!    rms²)) − 10·log10(max(1e-10, max(rms)²))`, in `float32`.
//! 3. A frame is non-silent when its dB is `> −top_db`.
//! 4. Keep `[first·hop, min(n, (last+1)·hop))`; nothing non-silent → empty.
//!
//! # Bit-exactness
//!
//! The per-frame mean is where a naive port drifts. NumPy reduces the frame
//! axis (contiguous after `np.square`, which keeps the view's stride order)
//! with the add identity `0.0` as the initial value followed by its
//! **pairwise** summation over all 2048 squares. [`pairwise_sum`] reproduces
//! that grouping, so the RMS values here are bit-identical to NumPy's — the
//! golden corpus (`fixtures/speech/trim.json`, generated from the real
//! `kokoro_onnx.trim`) pins the RMS of every frame, not only the cut points.
//! `log10` is the platform `log10f` on both sides; the corpus compares dB to
//! a tolerance and the cut indices exactly.

/// `trim(top_db=…)` default in `kokoro_onnx/trim.py`.
pub const TOP_DB: f32 = 60.0;
/// `frame_length` default.
pub const FRAME_LENGTH: usize = 2048;
/// `hop_length` default.
pub const HOP_LENGTH: usize = 512;
/// `power_to_db`'s `amin` after `amplitude_to_db` squares it (`1e-5 ** 2`).
const AMIN_POWER: f32 = 1e-10;
/// NumPy's `PW_BLOCKSIZE`.
const PW_BLOCKSIZE: usize = 128;

/// NumPy's `@TYPE@_pairwise_sum` for `float32` at unit stride.
fn pairwise_sum(a: &[f32]) -> f32 {
    let n = a.len();
    if n < 8 {
        let mut res = 0.0f32;
        for &v in a {
            res += v;
        }
        return res;
    }
    if n <= PW_BLOCKSIZE {
        let mut r = [0.0f32; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        while i < n - (n % 8) {
            for j in 0..8 {
                r[j] += a[i + j];
            }
            i += 8;
        }
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        while i < n {
            res += a[i];
            i += 1;
        }
        return res;
    }
    let mut n2 = n / 2;
    n2 -= n2 % 8;
    pairwise_sum(&a[..n2]) + pairwise_sum(&a[n2..])
}

/// `rms(y=y, frame_length, hop_length)` for mono input, flattened to one
/// value per frame.
#[must_use]
pub fn rms(y: &[f32], frame_length: usize, hop_length: usize) -> Vec<f32> {
    let pad = frame_length / 2;
    let mut padded = vec![0.0f32; y.len() + 2 * pad];
    padded[pad..pad + y.len()].copy_from_slice(y);
    if padded.len() < frame_length || hop_length == 0 {
        // `frame` raises ParameterError here; unreachable with centering on
        // (the padding alone is one frame), kept so a bad argument is empty
        // output rather than a panic.
        return Vec::new();
    }
    let n_frames = 1 + (padded.len() - frame_length) / hop_length;
    let mut squares = vec![0.0f32; frame_length];
    let mut out = Vec::with_capacity(n_frames);
    #[allow(clippy::cast_precision_loss)]
    let denom = frame_length as f32;
    for k in 0..n_frames {
        let frame = &padded[k * hop_length..k * hop_length + frame_length];
        for (s, &x) in squares.iter_mut().zip(frame) {
            *s = x * x;
        }
        // `np.add.reduce` starts from the identity, then adds the pairwise sum.
        let power = (0.0f32 + pairwise_sum(&squares)) / denom;
        out.push(power.sqrt());
    }
    out
}

/// `amplitude_to_db(rms, ref=np.max, top_db=None)`.
#[must_use]
pub fn amplitude_to_db(magnitude: &[f32]) -> Vec<f32> {
    let reference = magnitude.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let ref_power = reference * reference;
    let ref_db = 10.0f32 * AMIN_POWER.max(ref_power).log10();
    magnitude
        .iter()
        .map(|&m| {
            let p = m * m;
            10.0f32 * AMIN_POWER.max(p).log10() - ref_db
        })
        .collect()
}

/// The non-silent interval `[start, end)` of `y`, as `trim` returns it in
/// `index`. `(0, 0)` when nothing clears the threshold.
#[must_use]
pub fn trim_interval(y: &[f32]) -> (usize, usize) {
    trim_interval_with(y, TOP_DB, FRAME_LENGTH, HOP_LENGTH)
}

/// [`trim_interval`] with every librosa parameter explicit.
#[must_use]
pub fn trim_interval_with(
    y: &[f32],
    top_db: f32,
    frame_length: usize,
    hop_length: usize,
) -> (usize, usize) {
    let db = amplitude_to_db(&rms(y, frame_length, hop_length));
    let threshold = -top_db;
    let first = db.iter().position(|&d| d > threshold);
    let last = db.iter().rposition(|&d| d > threshold);
    match (first, last) {
        (Some(first), Some(last)) => {
            let start = first * hop_length;
            let end = y.len().min((last + 1) * hop_length);
            // `y[start:end]` with start > end is empty in NumPy.
            (start.min(end), end)
        }
        _ => (0, 0),
    }
}

/// `trim(y)[0]` — the trimmed samples.
#[must_use]
pub fn trim(y: &[f32]) -> &[f32] {
    let (start, end) = trim_interval(y);
    &y[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairwise_matches_sequential_on_small_exact_input() {
        let a: Vec<f32> = (0..300).map(|i| i as f32).collect();
        assert_eq!(pairwise_sum(&a), (0..300).sum::<i32>() as f32);
        assert_eq!(pairwise_sum(&[]), 0.0);
        assert_eq!(pairwise_sum(&[1.0, 2.0, 3.0]), 6.0);
    }

    #[test]
    fn empty_input_trims_to_empty() {
        assert_eq!(trim_interval(&[]), (0, 0));
        assert!(trim(&[]).is_empty());
    }

    #[test]
    fn all_zero_signal_is_not_trimmed() {
        // librosa's documented quirk: with ref=max, a uniformly silent signal
        // has nothing quieter than its own maximum, so nothing is cut.
        let y = vec![0.0f32; 5000];
        assert_eq!(trim_interval(&y), (0, 5000));
    }

    #[test]
    fn silence_around_a_burst_is_cut_on_hop_boundaries() {
        let mut y = vec![0.0f32; 24_000];
        for (i, s) in y.iter_mut().enumerate().skip(10_000).take(2_000) {
            *s = if i % 2 == 0 { 0.5 } else { -0.5 };
        }
        let (start, end) = trim_interval(&y);
        assert_eq!(start % HOP_LENGTH, 0);
        assert!((10_000 - FRAME_LENGTH + 1..=10_000).contains(&start));
        assert!((12_000..12_000 + FRAME_LENGTH).contains(&end));
        assert!(end - start < y.len() / 2);
    }
}
