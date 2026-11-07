//! Word times from whisper.cpp's DTW token alignment (T2.2d).
//!
//! whisper.cpp 1.8.3 can time every text token by dynamic time warping over the
//! cross-attention of a model's alignment heads (`dtw_token_timestamps`,
//! `whisper_exp_compute_token_level_timestamps_dtw` in `src/whisper.cpp`, a port of
//! openai/whisper `timing.py`: the decoder is run once more over the window's text tokens,
//! the alignment heads' cross-attention is normalised, median-filtered (width 7), averaged and
//! aligned to the 20 ms audio frames). It needs the model's alignment heads, which differ per
//! model (whisper.cpp's `g_aheads` presets, copied from openai/whisper `_ALIGNMENT_HEADS`),
//! and cannot run with flash attention (whisper.cpp then disables DTW with a warning, so
//! [`crate::WhisperTranscriber::load`] turns flash attention off explicitly).
//!
//! [`ALIGNMENT_HEADS`] pins the preset of every whisper model of `models/manifest.toml`
//! (checked against the manifest by a test) together with the text-decoder shape the preset
//! is for, which is checked against the loaded model, so a renamed file cannot run with
//! another model's heads.
//!
//! DTW stays opt-in (T2.2d, `docs/word-times.md`); it must not become a default, whatever a
//! measurement shows, until the following whisper.cpp 1.8.3 defect is fixed upstream or worked
//! around:
//!
//! - Out-of-bounds read: the placement loop of `whisper_exp_compute_token_level_timestamps_dtw`
//!   (`src/whisper.cpp` about lines 8925-8948) advances to the next segment after every
//!   placed token, `++seg_i; tok_i = seg_i->tokens.begin();`, without checking for
//!   `result_all.end()`. When the window's last segment ends on a text token, which is what
//!   the trailing-text branch of `whisper_full_with_state` (about lines 7680-7705) produces
//!   when a window's text ends without a closing timestamp token, placing that last token
//!   dereferences `result_all.end()` (undefined behaviour: a read past the vector).
//! - Cost: the same function creates a ggml context of `dtw_mem_size` (whisper-rs default
//!   128 MiB) per window of every call and frees it again ("FIXME: Allocating mem everytime"),
//!   besides the extra decoder pass. That is part of DTW's slowdown on short calls.

use whisper_rs::DtwModelPreset;

/// How [`crate::WhisperTranscriber`] times words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordTiming {
    /// whisper.cpp's token-level timestamps (`token_timestamps`,
    /// `whisper_exp_compute_token_level_timestamps`): a word spans its first token's `t0` to
    /// its last token's `t1`.
    TokenTimestamps,
    /// DTW over the model's alignment heads ([`ALIGNMENT_HEADS`]), with word boundaries
    /// placed as openai/whisper `timing.py` places them: a word starts at the DTW time of
    /// the token before it and ends at that of its own last token (whisper.cpp's `t_dtw` of
    /// a token is the jump into the decoder row that predicts the next token).
    Dtw,
}

/// A whisper.cpp alignment-head preset (`enum whisper_alignment_heads_preset`), for the
/// models Earshot uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadsPreset {
    /// `WHISPER_AHEADS_TINY_EN` (8 heads).
    TinyEn,
    /// `WHISPER_AHEADS_BASE_EN` (5 heads).
    BaseEn,
    /// `WHISPER_AHEADS_SMALL_EN` (19 heads).
    SmallEn,
    /// `WHISPER_AHEADS_LARGE_V3_TURBO` (6 heads).
    LargeV3Turbo,
}

impl HeadsPreset {
    pub(crate) fn whisper_rs(self) -> DtwModelPreset {
        match self {
            HeadsPreset::TinyEn => DtwModelPreset::TinyEn,
            HeadsPreset::BaseEn => DtwModelPreset::BaseEn,
            HeadsPreset::SmallEn => DtwModelPreset::SmallEn,
            HeadsPreset::LargeV3Turbo => DtwModelPreset::LargeV3Turbo,
        }
    }
}

/// The alignment heads of one whisper model and the text-decoder shape they belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlignmentHeads {
    /// Model name as in `models/manifest.toml` (`whisper/ggml-<name>.bin`).
    pub model: &'static str,
    /// whisper.cpp preset of its alignment heads.
    pub preset: HeadsPreset,
    /// Text-decoder layers of the model (`whisper_model_n_text_layer`).
    pub text_layers: i32,
    /// Attention heads per text-decoder layer (`whisper_model_n_text_head`).
    pub text_heads: i32,
    /// Multilingual vocabulary (`whisper_is_multilingual`).
    pub multilingual: bool,
}

/// Alignment heads of every whisper model in `models/manifest.toml`. Quantised files keep
/// their model's architecture and heads (quantisation changes weights, not layers). Shapes
/// from openai/whisper `model.py` `ModelDimensions` of each checkpoint: tiny 4 x 6, base
/// 6 x 8, small 12 x 12, large-v3-turbo 4 x 20 (32 encoder layers, 4 decoder layers).
pub const ALIGNMENT_HEADS: &[AlignmentHeads] = &[
    AlignmentHeads {
        model: "tiny.en",
        preset: HeadsPreset::TinyEn,
        text_layers: 4,
        text_heads: 6,
        multilingual: false,
    },
    AlignmentHeads {
        model: "base.en",
        preset: HeadsPreset::BaseEn,
        text_layers: 6,
        text_heads: 8,
        multilingual: false,
    },
    AlignmentHeads {
        model: "small.en",
        preset: HeadsPreset::SmallEn,
        text_layers: 12,
        text_heads: 12,
        multilingual: false,
    },
    AlignmentHeads {
        model: "small.en-q5_1",
        preset: HeadsPreset::SmallEn,
        text_layers: 12,
        text_heads: 12,
        multilingual: false,
    },
    AlignmentHeads {
        model: "large-v3-turbo",
        preset: HeadsPreset::LargeV3Turbo,
        text_layers: 4,
        text_heads: 20,
        multilingual: true,
    },
    AlignmentHeads {
        model: "large-v3-turbo-q5_0",
        preset: HeadsPreset::LargeV3Turbo,
        text_layers: 4,
        text_heads: 20,
        multilingual: true,
    },
];

/// The alignment heads for a model id: the manifest name (`base.en`) or the file stem
/// (`ggml-base.en`, as [`earshot_core::asr::BackendInfo::model`] reports it). Matched
/// exactly; `None` for a model without pinned heads.
#[must_use]
pub fn alignment_heads(model: &str) -> Option<&'static AlignmentHeads> {
    let name = model.strip_prefix("ggml-").unwrap_or(model);
    ALIGNMENT_HEADS.iter().find(|h| h.model == name)
}

/// Checks a loaded model's text-decoder shape against the pinned heads.
///
/// # Errors
/// A message naming the difference when the model is not the one the heads belong to.
pub(crate) fn check_shape(
    heads: &AlignmentHeads,
    text_layers: i32,
    text_heads: i32,
    multilingual: bool,
) -> Result<(), String> {
    let want = (heads.text_layers, heads.text_heads, heads.multilingual);
    let got = (text_layers, text_heads, multilingual);
    if want == got {
        Ok(())
    } else {
        Err(format!(
            "the alignment heads of {} are for (text layers, heads, multilingual) = {want:?}, \
             but the model file has {got:?}",
            heads.model
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whisper model names of `models/manifest.toml` (`whisper/ggml-<name>.bin`).
    fn manifest_whisper_models() -> Vec<String> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/manifest.toml");
        let text = std::fs::read_to_string(&path).expect("read models/manifest.toml");
        let doc: toml::Table = text.parse().expect("parse models/manifest.toml");
        let mut names: Vec<String> = doc["artefact"]
            .as_array()
            .expect("[[artefact]] entries")
            .iter()
            .filter_map(|a| a.get("path")?.as_str())
            .filter_map(|p| p.strip_prefix("whisper/ggml-")?.strip_suffix(".bin"))
            .map(str::to_owned)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn dtw_alignment_heads_cover_exactly_the_manifest_whisper_models() {
        let mut pinned: Vec<String> = ALIGNMENT_HEADS.iter().map(|h| h.model.to_owned()).collect();
        pinned.sort();
        let manifest = manifest_whisper_models();
        assert!(!manifest.is_empty(), "no whisper models in the manifest");
        assert_eq!(pinned, manifest);
    }

    #[test]
    fn dtw_alignment_heads_are_pinned_per_model() {
        let pins: Vec<(&str, HeadsPreset, i32, i32, bool)> = ALIGNMENT_HEADS
            .iter()
            .map(|h| {
                (
                    h.model,
                    h.preset,
                    h.text_layers,
                    h.text_heads,
                    h.multilingual,
                )
            })
            .collect();
        assert_eq!(
            pins,
            [
                ("tiny.en", HeadsPreset::TinyEn, 4, 6, false),
                ("base.en", HeadsPreset::BaseEn, 6, 8, false),
                ("small.en", HeadsPreset::SmallEn, 12, 12, false),
                ("small.en-q5_1", HeadsPreset::SmallEn, 12, 12, false),
                ("large-v3-turbo", HeadsPreset::LargeV3Turbo, 4, 20, true),
                (
                    "large-v3-turbo-q5_0",
                    HeadsPreset::LargeV3Turbo,
                    4,
                    20,
                    true
                ),
            ]
        );
    }

    #[test]
    fn dtw_alignment_heads_match_model_ids_exactly() {
        assert_eq!(alignment_heads("base.en").unwrap().model, "base.en");
        assert_eq!(alignment_heads("ggml-base.en").unwrap().model, "base.en");
        assert_eq!(
            alignment_heads("ggml-small.en-q5_1").unwrap().preset,
            HeadsPreset::SmallEn
        );
        for unknown in ["base", "ggml-base", "large-v3", "base.en.bin", "", "ggml-"] {
            assert!(alignment_heads(unknown).is_none(), "{unknown}");
        }
    }

    #[test]
    fn dtw_shape_check_refuses_another_model() {
        let base = alignment_heads("base.en").unwrap();
        assert!(check_shape(base, 6, 8, false).is_ok());
        let e = check_shape(base, 4, 6, false).unwrap_err();
        assert!(e.contains("base.en") && e.contains("(4, 6, false)"), "{e}");
        assert!(check_shape(base, 6, 8, true).is_err());
    }
}
