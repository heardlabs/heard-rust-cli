//! Reading `voices-v1.0.bin`, which is a NumPy `.npz`.
//!
//! The Python does `np.load(voices_path)` and then `self.voices[name]`, so the
//! file format is not documented anywhere in `kokoro.py` — it is whatever
//! `np.savez` wrote. In practice: a **zip archive**, one `NAME.npy` member per
//! voice, each a `float32` array of shape `[510, 1, 256]` — one 256-wide style
//! vector per possible phoneme count, which is why `_style_for` indexes it by
//! token count.
//!
//! `.npy` itself is a short header (magic, version, then a Python-dict literal
//! giving dtype, order and shape) followed by raw little-endian data. Parsing
//! it here is ~60 lines and avoids a dependency whose job would be to read one
//! file format this crate knows the exact shape of. The header is *not* trusted
//! blindly: dtype, byte order, `fortran_order` and the total element count are
//! all checked, because a wrong assumption here would not crash — it would
//! synthesise noise.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use super::KokoroError;

/// The style-vector width the model takes. `style` input is `[1, 256]`.
pub const STYLE_DIM: usize = 256;

/// Every voice in `voices-v1.0.bin`, keyed by name (`af_heart`, `bm_george`…).
#[derive(Debug, Clone)]
pub struct Voices {
    /// `name -> rows`, each row [`STYLE_DIM`] wide. Row `n - 1` is the style
    /// for `n` tokens.
    voices: BTreeMap<String, Vec<f32>>,
    rows_per_voice: usize,
}

impl Voices {
    /// Parse the `.npz`.
    ///
    /// # Errors
    /// [`KokoroError::Voices`] if the file is not a zip, holds no usable
    /// arrays, or holds one whose dtype or shape is not what the model needs.
    pub fn load(path: &Path) -> Result<Self, KokoroError> {
        let file = std::fs::File::open(path)
            .map_err(|e| KokoroError::Voices(format!("opening {}: {e}", path.display())))?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| {
            KokoroError::Voices(format!("{} is not a .npz zip: {e}", path.display()))
        })?;

        let mut voices = BTreeMap::new();
        let mut rows_per_voice = 0usize;

        for i in 0..zip.len() {
            let mut entry = zip
                .by_index(i)
                .map_err(|e| KokoroError::Voices(format!("zip entry {i}: {e}")))?;
            let name = entry.name().to_string();
            let Some(voice) = name.strip_suffix(".npy") else {
                continue;
            };
            let voice = voice.to_string();

            let mut raw = Vec::new();
            entry
                .read_to_end(&mut raw)
                .map_err(|e| KokoroError::Voices(format!("reading {name}: {e}")))?;

            let (values, shape) = parse_npy_f32(&raw, &name)?;
            let rows = values.len() / STYLE_DIM;
            if values.len() % STYLE_DIM != 0 {
                return Err(KokoroError::Voices(format!(
                    "{name}: {} values is not a multiple of the {STYLE_DIM}-wide style vector \
                     (shape {shape:?})",
                    values.len()
                )));
            }
            if rows_per_voice == 0 {
                rows_per_voice = rows;
            }
            voices.insert(voice, values);
        }

        if voices.is_empty() {
            return Err(KokoroError::Voices(format!(
                "{} holds no .npy arrays — a truncated or wrong file",
                path.display()
            )));
        }
        Ok(Self {
            voices,
            rows_per_voice,
        })
    }

    /// Voice names, sorted. `Kokoro.get_voices`.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.voices.keys().cloned().collect()
    }

    /// How many style rows each voice carries (510 upstream).
    #[must_use]
    pub fn rows_per_voice(&self) -> usize {
        self.rows_per_voice
    }

    /// The style vector for a voice at `token_count` tokens.
    ///
    /// `_style_for`: "one style vector per phoneme count, so n phonemes use row
    /// n - 1", clamped to the last row. `token_count` of 0 takes row 0, which
    /// is what `min(0, len) - 1` would underflow to in Rust and wrap to in
    /// Python — so it is handled explicitly rather than left to arithmetic.
    ///
    /// # Errors
    /// [`KokoroError::UnknownVoice`] if the name is not in the file.
    pub fn style_for(&self, voice: &str, token_count: usize) -> Result<&[f32], KokoroError> {
        let rows = self.voices.get(voice).ok_or_else(|| {
            let mut near: Vec<&str> = self
                .voices
                .keys()
                .map(String::as_str)
                .filter(|n| voice.len() >= 2 && n.starts_with(&voice[..2]))
                .take(5)
                .collect();
            near.sort_unstable();
            KokoroError::UnknownVoice {
                voice: voice.to_string(),
                near: near.join(", "),
            }
        })?;

        let total_rows = rows.len() / STYLE_DIM;
        let idx = token_count.clamp(1, total_rows) - 1;
        Ok(&rows[idx * STYLE_DIM..(idx + 1) * STYLE_DIM])
    }
}

/// Parse a `.npy` holding little-endian `float32`, returning the values and the
/// declared shape.
fn parse_npy_f32(raw: &[u8], name: &str) -> Result<(Vec<f32>, Vec<usize>), KokoroError> {
    let bad = |msg: String| KokoroError::Voices(format!("{name}: {msg}"));

    if raw.len() < 10 || &raw[..6] != b"\x93NUMPY" {
        return Err(bad("not a .npy (bad magic)".into()));
    }
    let major = raw[6];
    // v1 has a 2-byte header length, v2/v3 have 4.
    let (header_len, header_start) = if major == 1 {
        (u16::from_le_bytes([raw[8], raw[9]]) as usize, 10usize)
    } else {
        if raw.len() < 12 {
            return Err(bad("truncated v2 header".into()));
        }
        (
            u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]) as usize,
            12usize,
        )
    };

    let header_end = header_start
        .checked_add(header_len)
        .filter(|e| *e <= raw.len())
        .ok_or_else(|| bad("header runs past end of file".into()))?;
    let header = String::from_utf8_lossy(&raw[header_start..header_end]);

    // The header is a Python dict literal. Three fields matter, and each one
    // is checked rather than assumed: a silent mismatch here is noise, not a
    // crash.
    if !(header.contains("'<f4'") || header.contains("\"<f4\"")) {
        return Err(bad(format!(
            "expected little-endian float32 ('<f4'), header says {}",
            header.trim()
        )));
    }
    if header.contains("'fortran_order': True") {
        return Err(bad(
            "Fortran-ordered array; this reader assumes C order".into()
        ));
    }

    let shape = parse_shape(&header).ok_or_else(|| bad("no shape in header".into()))?;
    let expected: usize = shape.iter().product();

    let data = &raw[header_end..];
    if data.len() != expected * 4 {
        return Err(bad(format!(
            "shape {shape:?} wants {} bytes, file has {}",
            expected * 4,
            data.len()
        )));
    }

    let values = data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect();
    Ok((values, shape))
}

/// Pull `(510, 1, 256)` out of the header dict.
fn parse_shape(header: &str) -> Option<Vec<usize>> {
    let start = header.find("'shape':").map(|i| i + "'shape':".len())?;
    let rest = &header[start..];
    let open = rest.find('(')?;
    let close = rest[open..].find(')')? + open;
    let inner = &rest[open + 1..close];
    let dims: Vec<usize> = inner
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if dims.is_empty() {
        return None;
    }
    Some(dims)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a v1 `.npy` the way `np.save` would.
    fn npy(shape: &[usize], values: &[f32], dtype: &str, fortran: bool) -> Vec<u8> {
        let dims: Vec<String> = shape.iter().map(ToString::to_string).collect();
        let shape_str = if shape.len() == 1 {
            format!("({},)", dims[0])
        } else {
            format!("({})", dims.join(", "))
        };
        let dict = format!(
            "{{'descr': '{dtype}', 'fortran_order': {}, 'shape': {shape_str}, }}",
            if fortran { "True" } else { "False" }
        );
        let mut header = dict.into_bytes();
        // np pads the header to a 64-byte boundary with spaces and a newline.
        while (10 + header.len() + 1) % 64 != 0 {
            header.push(b' ');
        }
        header.push(b'\n');

        let mut out = Vec::new();
        out.extend_from_slice(b"\x93NUMPY");
        out.push(1);
        out.push(0);
        out.extend_from_slice(&(header.len() as u16).to_le_bytes());
        out.extend_from_slice(&header);
        for v in values {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    #[test]
    fn a_well_formed_npy_round_trips() {
        let values: Vec<f32> = (0..6).map(|i| i as f32).collect();
        let raw = npy(&[3, 2], &values, "<f4", false);
        let (got, shape) = parse_npy_f32(&raw, "x.npy").unwrap();
        assert_eq!(got, values);
        assert_eq!(shape, vec![3, 2]);
    }

    #[test]
    fn a_three_dimensional_shape_parses() {
        // 510 rows x 1 x 4 — the real file's shape, narrowed in the last dim.
        let values = vec![0.5f32; 510 * 4];
        let raw = npy(&[510, 1, 4], &values, "<f4", false);
        let (_, shape) = parse_npy_f32(&raw, "v.npy").unwrap();
        assert_eq!(shape, vec![510, 1, 4]);
    }

    #[test]
    fn a_one_dimensional_shape_parses() {
        let raw = npy(&[4], &[1.0, 2.0, 3.0, 4.0], "<f4", false);
        let (got, shape) = parse_npy_f32(&raw, "v.npy").unwrap();
        assert_eq!(shape, vec![4]);
        assert_eq!(got.len(), 4);
    }

    #[test]
    fn the_wrong_dtype_is_refused_rather_than_reinterpreted() {
        // float64 bytes read as float32 would be silent noise, which is the
        // whole reason this is checked.
        let raw = npy(&[2], &[1.0, 2.0], "<f8", false);
        let err = parse_npy_f32(&raw, "v.npy").unwrap_err();
        assert!(err.to_string().contains("float32"), "{err}");
    }

    #[test]
    fn big_endian_is_refused() {
        let raw = npy(&[2], &[1.0, 2.0], ">f4", false);
        assert!(parse_npy_f32(&raw, "v.npy").is_err());
    }

    #[test]
    fn fortran_order_is_refused() {
        let raw = npy(&[2, 2], &[1.0, 2.0, 3.0, 4.0], "<f4", true);
        let err = parse_npy_f32(&raw, "v.npy").unwrap_err();
        assert!(err.to_string().contains("Fortran"), "{err}");
    }

    #[test]
    fn a_shape_that_disagrees_with_the_data_is_refused() {
        let mut raw = npy(&[4], &[1.0, 2.0, 3.0, 4.0], "<f4", false);
        raw.truncate(raw.len() - 4); // lose one value
        let err = parse_npy_f32(&raw, "v.npy").unwrap_err();
        assert!(err.to_string().contains("wants"), "{err}");
    }

    #[test]
    fn a_file_that_is_not_an_npy_is_refused() {
        assert!(parse_npy_f32(b"not an npy at all", "v.npy").is_err());
        assert!(parse_npy_f32(b"", "v.npy").is_err());
    }

    #[test]
    fn style_row_is_token_count_minus_one_clamped() {
        let rows = 4usize;
        let mut values = Vec::new();
        for r in 0..rows {
            values.extend(std::iter::repeat_n(r as f32, STYLE_DIM));
        }
        let mut voices = BTreeMap::new();
        voices.insert("af_test".to_string(), values);
        let v = Voices {
            voices,
            rows_per_voice: rows,
        };

        assert_eq!(v.style_for("af_test", 1).unwrap()[0], 0.0);
        assert_eq!(v.style_for("af_test", 3).unwrap()[0], 2.0);
        assert_eq!(v.style_for("af_test", 4).unwrap()[0], 3.0);
        // Past the end clamps to the last row rather than panicking.
        assert_eq!(v.style_for("af_test", 9_999).unwrap()[0], 3.0);
        // Zero tokens takes row 0 instead of underflowing.
        assert_eq!(v.style_for("af_test", 0).unwrap()[0], 0.0);
        assert_eq!(v.style_for("af_test", 1).unwrap().len(), STYLE_DIM);
    }

    #[test]
    fn an_unknown_voice_names_itself_and_suggests_neighbours() {
        let mut voices = BTreeMap::new();
        voices.insert("af_heart".to_string(), vec![0.0; STYLE_DIM]);
        voices.insert("af_bella".to_string(), vec![0.0; STYLE_DIM]);
        voices.insert("bm_george".to_string(), vec![0.0; STYLE_DIM]);
        let v = Voices {
            voices,
            rows_per_voice: 1,
        };
        let err = v.style_for("af_nope", 1).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("af_nope"), "{msg}");
        assert!(msg.contains("af_bella"), "{msg}");
        assert!(!msg.contains("bm_george"), "{msg}");
    }
}
