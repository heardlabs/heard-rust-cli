//! The 54 voices baked into `voices-v1.0.bin` (read from the pinned file's
//! zip listing), with a human label for pickers.
//!
//! The free voice's phonemiser (misaki) is English-only, so the American and
//! British voices are the ones that sound right; the rest are accepted but
//! flagged.

/// Every voice id in `voices-v1.0.bin`.
pub const KOKORO_VOICES: [&str; 54] = [
    "af_alloy",
    "af_aoede",
    "af_bella",
    "af_heart",
    "af_jessica",
    "af_kore",
    "af_nicole",
    "af_nova",
    "af_river",
    "af_sarah",
    "af_sky",
    "am_adam",
    "am_echo",
    "am_eric",
    "am_fenrir",
    "am_liam",
    "am_michael",
    "am_onyx",
    "am_puck",
    "am_santa",
    "bf_alice",
    "bf_emma",
    "bf_isabella",
    "bf_lily",
    "bm_daniel",
    "bm_fable",
    "bm_george",
    "bm_lewis",
    "ef_dora",
    "em_alex",
    "em_santa",
    "ff_siwis",
    "hf_alpha",
    "hf_beta",
    "hm_omega",
    "hm_psi",
    "if_sara",
    "im_nicola",
    "jf_alpha",
    "jf_gongitsune",
    "jf_nezumi",
    "jf_tebukuro",
    "jm_kumo",
    "pf_dora",
    "pm_alex",
    "pm_santa",
    "zf_xiaobei",
    "zf_xiaoni",
    "zf_xiaoxiao",
    "zf_xiaoyi",
    "zm_yunjian",
    "zm_yunxi",
    "zm_yunxia",
    "zm_yunyang",
];

/// The default Kokoro voice (`kokoro_voice` in the core defaults).
pub const DEFAULT_VOICE: &str = "bm_george";

/// Is this a known voice id?
pub fn is_voice(id: &str) -> bool {
    KOKORO_VOICES.contains(&id)
}

/// American or British: the voices the English phonemiser suits.
pub fn is_english(id: &str) -> bool {
    id.starts_with('a') || id.starts_with('b')
}

/// `bm_george` → "George · British male".
pub fn label(id: &str) -> String {
    let mut chars = id.chars();
    let accent = match chars.next() {
        Some('a') => "American",
        Some('b') => "British",
        Some('e') => "Spanish",
        Some('f') => "French",
        Some('h') => "Hindi",
        Some('i') => "Italian",
        Some('j') => "Japanese",
        Some('p') => "Portuguese (BR)",
        Some('z') => "Mandarin",
        _ => "?",
    };
    let gender = match chars.next() {
        Some('f') => "female",
        Some('m') => "male",
        _ => "",
    };
    let name = id.split_once('_').map(|(_, n)| n).unwrap_or(id);
    let mut cap = name.to_string();
    if let Some(first) = cap.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    let extra = if is_english(id) {
        ""
    } else {
        " (English text sounds accented)"
    };
    format!("{cap} · {accent} {gender}{extra}")
}

/// English voices first, then the rest, each in file order.
pub fn picker_order() -> Vec<&'static str> {
    let mut v: Vec<&str> = KOKORO_VOICES
        .iter()
        .copied()
        .filter(|v| is_english(v))
        .collect();
    v.extend(KOKORO_VOICES.iter().copied().filter(|v| !is_english(v)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_read_well() {
        assert_eq!(label("bm_george"), "George · British male");
        assert_eq!(label("af_heart"), "Heart · American female");
        assert!(label("jf_alpha").contains("accented"));
    }

    #[test]
    fn english_first() {
        let o = picker_order();
        assert_eq!(o.len(), 54);
        assert!(o[..28].iter().all(|v| is_english(v)));
    }
}
