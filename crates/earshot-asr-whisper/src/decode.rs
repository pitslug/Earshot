//! Pure mappings between the core ASR options and whisper.cpp parameters, and word
//! assembly from whisper.cpp tokens. Nothing here touches a model, so it is unit-tested
//! without one.

use earshot_core::asr::{AsrError, DecodingOptions, DecodingStrategy, Language, Word};

use crate::dtw::WordTiming;

/// whisper.cpp's limit on parallel decoders (`WHISPER_MAX_DECODERS` in
/// whisper.cpp 1.8.3 `src/whisper.cpp`); a larger beam makes `whisper_full` fail.
pub(crate) const MAX_BEAM_SIZE: u32 = 8;

/// whisper.cpp search strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strategy {
    /// `WHISPER_SAMPLING_GREEDY` with `greedy.best_of`.
    Greedy {
        /// Candidates sampled per step; only used by whisper.cpp when temperature > 0.
        best_of: i32,
    },
    /// `WHISPER_SAMPLING_BEAM_SEARCH` with `beam_search.beam_size`.
    BeamSearch {
        /// Number of beams.
        beam_size: i32,
    },
}

/// The whisper.cpp sampling settings for one [`DecodingOptions`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DecodePlan {
    /// Search strategy.
    pub strategy: Strategy,
    /// `whisper_full_params.temperature`.
    pub temperature: f32,
    /// `whisper_full_params.temperature_inc`: 0 disables whisper.cpp's temperature fallback,
    /// otherwise a failed window is decoded again at +`temperature_inc` up to 1.0.
    pub temperature_inc: f32,
    /// `whisper_full_params.entropy_thold` (repetition check of the fallback).
    pub entropy_thold: f32,
    /// `whisper_full_params.logprob_thold` (confidence check of the fallback).
    pub logprob_thold: f32,
}

/// Maps [`DecodingOptions`] onto whisper.cpp exactly (PLAN.md 4.0).
///
/// Greedy uses `best_of = 1`: [`DecodingOptions`] has no `best_of`; at temperature 0
/// whisper.cpp runs a single greedy decoder, and a live fallback samples one candidate per
/// step at the higher temperature (live stays greedy). A beam-search retry also runs a
/// single sampled decoder: whisper-rs 0.16 cannot set `greedy.best_of` on beam-search
/// params, so whisper.cpp keeps -1 and uses `max(1, best_of)` = 1 decoder at a retry
/// temperature (whisper.cpp 1.8.3 `whisper_full_with_state`). This is a deviation from
/// openai/whisper (`best_of=5`); see [`earshot_core::asr::TemperatureFallback`].
///
/// [`DecodingOptions::fallback`] maps onto whisper.cpp's own fallback: `increment` ->
/// `temperature_inc`, `repetition_threshold` -> `entropy_thold` (whisper.cpp's repetition
/// measure, see [`earshot_core::asr::TemperatureFallback`]), `logprob_threshold` ->
/// `logprob_thold`. Without a fallback, `temperature_inc` is 0 and the thresholds keep
/// whisper.cpp's defaults, which then have no effect.
///
/// # Errors
/// [`AsrError::Config`] for a negative or non-finite temperature, a beam size outside
/// `1..=MAX_BEAM_SIZE`, or a fallback whose increment is not in `(0, 1]` or whose
/// thresholds are not finite.
pub(crate) fn decode_plan(options: &DecodingOptions) -> Result<DecodePlan, AsrError> {
    let temperature = options.temperature;
    if !temperature.is_finite() || temperature < 0.0 {
        return Err(AsrError::Config(format!(
            "temperature must be finite and >= 0, got {temperature}"
        )));
    }
    let strategy = match options.strategy {
        DecodingStrategy::Greedy => Strategy::Greedy { best_of: 1 },
        DecodingStrategy::BeamSearch { beam_size } => {
            if !(1..=MAX_BEAM_SIZE).contains(&beam_size) {
                return Err(AsrError::Config(format!(
                    "beam size must be in 1..={MAX_BEAM_SIZE}, got {beam_size}"
                )));
            }
            Strategy::BeamSearch {
                // In range by the check above.
                beam_size: i32::try_from(beam_size).unwrap_or(1),
            }
        }
    };
    let fallback = options.fallback;
    if let Some(f) = fallback {
        let ok = f.increment.is_finite()
            && f.increment > 0.0
            && f.increment <= 1.0
            && f.repetition_threshold.is_finite()
            && f.logprob_threshold.is_finite();
        if !ok {
            return Err(AsrError::Config(format!(
                "invalid temperature fallback {f:?}"
            )));
        }
    }
    let reference = earshot_core::asr::TemperatureFallback::WHISPER;
    Ok(DecodePlan {
        strategy,
        temperature,
        temperature_inc: fallback.map_or(0.0, |f| f.increment),
        entropy_thold: fallback.map_or(reference.repetition_threshold, |f| f.repetition_threshold),
        logprob_thold: fallback.map_or(reference.logprob_threshold, |f| f.logprob_threshold),
    })
}

/// The language string to hand to whisper.cpp, validated against the model.
///
/// - [`Language::Code`] is looked up in whisper.cpp's language table (codes such as `"en"`
///   or full names such as `"english"`) and returned as its canonical code.
/// - [`Language::Auto`] is `"auto"` (whisper.cpp detects the language) on a multilingual
///   model and `"en"` on an English-only (`.en`) model, which has no language detection.
/// - An English-only model rejects every language other than English.
///
/// `lookup` resolves a code or name to whisper.cpp's canonical code
/// (`whisper_lang_id` followed by `whisper_lang_str`).
///
/// # Errors
/// [`AsrError::Config`] for an unknown language or a non-English language on an
/// English-only model.
pub(crate) fn whisper_language(
    language: &Language,
    multilingual: bool,
    lookup: impl Fn(&str) -> Option<&'static str>,
) -> Result<&'static str, AsrError> {
    let code = match language {
        Language::Auto if multilingual => return Ok("auto"),
        Language::Auto => return Ok("en"),
        Language::Code(code) => code,
    };
    if code.is_empty() || code.contains('\0') {
        return Err(AsrError::Config(format!("invalid language {code:?}")));
    }
    let canonical = lookup(code)
        .ok_or_else(|| AsrError::Config(format!("whisper.cpp does not know language {code:?}")))?;
    if !multilingual && canonical != "en" {
        return Err(AsrError::Config(format!(
            "language {code:?} requested from an English-only model"
        )));
    }
    Ok(canonical)
}

/// One whisper.cpp output token as needed for word assembly.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Token<'a> {
    /// Raw token bytes (a token may end inside a multi-byte UTF-8 character).
    pub text: &'a [u8],
    /// Token-level start (`whisper_token_data.t0`), in centiseconds.
    pub t0: i64,
    /// Token-level end (`whisper_token_data.t1`), in centiseconds.
    pub t1: i64,
    /// DTW time (`whisper_token_data.t_dtw`), in centiseconds; -1 when DTW is off or did not
    /// reach the token.
    pub t_dtw: i64,
    /// Token probability (`whisper_token_data.p`).
    pub p: f32,
    /// Special or timestamp token (id >= `whisper_token_eot`): never part of a word.
    pub special: bool,
}

/// Converts whisper.cpp centiseconds to milliseconds, clamping negatives (whisper.cpp uses
/// -1 for "no timestamp") to 0.
pub(crate) fn centis_to_millis(cs: i64) -> u64 {
    u64::try_from(cs).unwrap_or(0) * 10
}

/// How [`assemble_words`] times the words of one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Timing {
    /// [`WordTiming::TokenTimestamps`].
    Tokens,
    /// [`WordTiming::Dtw`]; `preceding` is the `t_dtw` of the last text token before this
    /// segment in the same call ([`last_dtw`] of the previous segment), -1 for none.
    Dtw { preceding: i64 },
}

impl Timing {
    /// The timing of a segment after one whose last text token had DTW time `preceding`.
    pub(crate) fn of(timing: WordTiming, preceding: i64) -> Self {
        match timing {
            WordTiming::TokenTimestamps => Timing::Tokens,
            WordTiming::Dtw => Timing::Dtw { preceding },
        }
    }
}

/// `t_dtw` of the last text token of a segment, -1 when it has none.
pub(crate) fn last_dtw(tokens: &[Token<'_>]) -> i64 {
    tokens
        .iter()
        .rev()
        .find(|t| !t.special)
        .map_or(-1, |t| t.t_dtw)
}

/// Groups the tokens of one segment into words.
///
/// whisper's BPE vocabulary marks a word start with a leading space, so a token starting
/// with `b' '` opens a new word and every other token (sub-word pieces, attached
/// punctuation) extends the current one. Bytes are joined before UTF-8 decoding because a
/// token can split a multi-byte character. Word times follow `timing`:
///
/// - [`Timing::Tokens`]: a word spans its first token's `t0` to its last token's `t1`.
/// - [`Timing::Dtw`]: openai/whisper `whisper/timing.py` (`find_alignment`): the
///   DTW path runs over decoder rows `[no_timestamps, text tokens...]`, and the row of input
///   token i attends to the audio of the token it predicts, i + 1; a word starts at the jump
///   into the row before its first token (`jump_times[word_boundaries[:-1]]`) and ends at the
///   jump into the row of its last token (`jump_times[word_boundaries[1:]]`). whisper.cpp
///   stores each text token's own row jump as `t_dtw` (the "no_timestamps" row is never
///   stored), so a word starts at the preceding text token's `t_dtw` (the previous
///   segment's last one for a segment's first word, [`last_dtw`]) and ends at its own last
///   token's `t_dtw`. The first word of a call has no preceding row in whisper.cpp; openai
///   would start it at the window start (the "no_timestamps" row's jump), here it starts at
///   its token-level `t0`, the decoder's own segment start. A missing `t_dtw` (-1, DTW did
///   not reach the token) falls back to `t0` for a start and `t1` for an end.
///
/// Starts and ends are clamped into the segment `[seg_t0, seg_t1]`, starts kept in
/// non-decreasing order and every end at or after its start; the probability is the mean
/// token probability.
pub(crate) fn assemble_words(
    tokens: &[Token<'_>],
    seg_t0: i64,
    seg_t1: i64,
    timing: Timing,
) -> Vec<Word> {
    struct Pending {
        bytes: Vec<u8>,
        t0: i64,
        t1: i64,
        /// `t_dtw` of the text token before the word's first token.
        dtw_before: i64,
        /// `t_dtw` of the word's last token.
        dtw_last: i64,
        p_sum: f32,
        n: u32,
    }

    let mut preceding = match timing {
        Timing::Tokens => -1,
        Timing::Dtw { preceding } => preceding,
    };
    let mut pending: Vec<Pending> = Vec::new();
    for token in tokens.iter().filter(|t| !t.special) {
        let opens_word = token.text.first() == Some(&b' ');
        match pending.last_mut() {
            Some(w) if !opens_word => {
                w.bytes.extend_from_slice(token.text);
                w.t1 = token.t1;
                w.dtw_last = token.t_dtw;
                w.p_sum += token.p;
                w.n += 1;
            }
            _ => pending.push(Pending {
                bytes: token.text.to_vec(),
                t0: token.t0,
                t1: token.t1,
                dtw_before: preceding,
                dtw_last: token.t_dtw,
                p_sum: token.p,
                n: 1,
            }),
        }
        preceding = token.t_dtw;
    }
    let pending: Vec<(String, Pending)> = pending
        .into_iter()
        .filter_map(|w| {
            let text = String::from_utf8_lossy(&w.bytes).trim().to_owned();
            (!text.is_empty()).then_some((text, w))
        })
        .collect();

    let seg_end = seg_t1.max(seg_t0);
    let clamp = |t: i64| centis_to_millis(t.clamp(seg_t0, seg_end));
    // Starts, clamped and non-decreasing.
    let mut starts: Vec<u64> = Vec::with_capacity(pending.len());
    for (_, w) in &pending {
        let raw = match timing {
            Timing::Dtw { .. } if w.dtw_before >= 0 => w.dtw_before,
            Timing::Tokens | Timing::Dtw { .. } => w.t0,
        };
        let prev = starts.last().copied().unwrap_or(0);
        starts.push(clamp(raw).max(prev));
    }
    pending
        .into_iter()
        .enumerate()
        .map(|(i, (text, w))| {
            let start = starts[i];
            let end = match timing {
                Timing::Dtw { .. } if w.dtw_last >= 0 => clamp(w.dtw_last),
                Timing::Tokens | Timing::Dtw { .. } => clamp(w.t1),
            }
            .max(start);
            let probability = (w.p_sum / w.n as f32).clamp(0.0, 1.0);
            Word::new(text, start, end, probability)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(code: &str) -> Option<&'static str> {
        match code {
            "en" | "english" => Some("en"),
            "de" | "german" => Some("de"),
            _ => None,
        }
    }

    #[test]
    fn live_decoding_is_greedy_single_candidate_with_whisper_fallback() {
        let plan = decode_plan(&DecodingOptions::LIVE).unwrap();
        assert_eq!(plan.strategy, Strategy::Greedy { best_of: 1 });
        assert_eq!(plan.temperature, 0.0);
        assert_eq!(
            (plan.temperature_inc, plan.entropy_thold, plan.logprob_thold),
            (0.2, 2.4, -1.0)
        );
    }

    #[test]
    fn batch_decoding_is_beam_five_with_whisper_fallback() {
        let plan = decode_plan(&DecodingOptions::BATCH).unwrap();
        assert_eq!(plan.strategy, Strategy::BeamSearch { beam_size: 5 });
        assert_eq!(plan.temperature, 0.0);
        assert_eq!(
            (plan.temperature_inc, plan.entropy_thold, plan.logprob_thold),
            (0.2, 2.4, -1.0)
        );
    }

    #[test]
    fn temperature_is_passed_through_and_no_fallback_means_no_retry() {
        let plan = decode_plan(&DecodingOptions {
            strategy: DecodingStrategy::Greedy,
            temperature: 0.4,
            fallback: None,
        })
        .unwrap();
        assert_eq!(plan.temperature, 0.4);
        assert_eq!(plan.temperature_inc, 0.0);
    }

    #[test]
    fn invalid_decoding_options_are_config_errors() {
        for bad in [
            DecodingOptions {
                strategy: DecodingStrategy::BeamSearch { beam_size: 0 },
                temperature: 0.0,
                fallback: None,
            },
            DecodingOptions {
                strategy: DecodingStrategy::BeamSearch {
                    beam_size: MAX_BEAM_SIZE + 1,
                },
                temperature: 0.0,
                fallback: None,
            },
            DecodingOptions {
                strategy: DecodingStrategy::Greedy,
                temperature: -0.1,
                fallback: None,
            },
            DecodingOptions {
                strategy: DecodingStrategy::Greedy,
                temperature: f32::NAN,
                fallback: None,
            },
            DecodingOptions {
                strategy: DecodingStrategy::Greedy,
                temperature: f32::INFINITY,
                fallback: None,
            },
            DecodingOptions {
                fallback: Some(earshot_core::asr::TemperatureFallback {
                    increment: 0.0,
                    ..earshot_core::asr::TemperatureFallback::WHISPER
                }),
                ..DecodingOptions::LIVE
            },
            DecodingOptions {
                fallback: Some(earshot_core::asr::TemperatureFallback {
                    logprob_threshold: f32::NAN,
                    ..earshot_core::asr::TemperatureFallback::WHISPER
                }),
                ..DecodingOptions::LIVE
            },
        ] {
            assert!(
                matches!(decode_plan(&bad), Err(AsrError::Config(_))),
                "{bad:?}"
            );
        }
        let max = DecodingOptions {
            strategy: DecodingStrategy::BeamSearch {
                beam_size: MAX_BEAM_SIZE,
            },
            temperature: 0.0,
            fallback: None,
        };
        assert_eq!(
            decode_plan(&max).unwrap().strategy,
            Strategy::BeamSearch { beam_size: 8 }
        );
    }

    #[test]
    fn default_language_is_english_on_both_model_kinds() {
        assert_eq!(
            whisper_language(&Language::default(), true, lookup).unwrap(),
            "en"
        );
        assert_eq!(
            whisper_language(&Language::default(), false, lookup).unwrap(),
            "en"
        );
    }

    #[test]
    fn auto_detects_on_multilingual_and_is_english_on_en_models() {
        assert_eq!(
            whisper_language(&Language::Auto, true, lookup).unwrap(),
            "auto"
        );
        assert_eq!(
            whisper_language(&Language::Auto, false, lookup).unwrap(),
            "en"
        );
    }

    #[test]
    fn language_names_resolve_to_canonical_codes() {
        let german = Language::Code("german".into());
        assert_eq!(whisper_language(&german, true, lookup).unwrap(), "de");
        let english = Language::Code("english".into());
        assert_eq!(whisper_language(&english, false, lookup).unwrap(), "en");
    }

    #[test]
    fn unknown_or_unsupported_languages_are_config_errors() {
        let cases = [
            (Language::Code("xx".into()), true),
            (Language::Code(String::new()), true),
            (Language::Code("e\0n".into()), true),
            (Language::Code("de".into()), false),
        ];
        for (language, multilingual) in cases {
            assert!(
                matches!(
                    whisper_language(&language, multilingual, lookup),
                    Err(AsrError::Config(_))
                ),
                "{language:?} multilingual={multilingual}"
            );
        }
    }

    const TOKENS: Timing = Timing::Tokens;
    const DTW: Timing = Timing::Dtw { preceding: -1 };

    fn tok(text: &str, t0: i64, t1: i64, p: f32) -> Token<'_> {
        Token {
            text: text.as_bytes(),
            t0,
            t1,
            t_dtw: -1,
            p,
            special: false,
        }
    }

    fn special(text: &str, t: i64) -> Token<'_> {
        Token {
            special: true,
            ..tok(text, t, t, 1.0)
        }
    }

    fn dtw(text: &str, t0: i64, t1: i64, t_dtw: i64) -> Token<'_> {
        Token {
            t_dtw,
            ..tok(text, t0, t1, 1.0)
        }
    }

    fn texts(words: &[Word]) -> Vec<&str> {
        words.iter().map(|w| w.text.as_str()).collect()
    }

    fn spans(words: &[Word]) -> Vec<(u64, u64)> {
        words
            .iter()
            .map(|w| (w.start.as_millis(), w.end.as_millis()))
            .collect()
    }

    #[test]
    fn dtw_words_follow_the_openai_alignment_rows() {
        let tokens = [
            special("[_BEG_]", 0),
            dtw(" He", 10, 20, 14),
            dtw("llo", 20, 30, 22),
            dtw(",", 30, 31, 30),
            dtw(" world", 40, 60, 52),
            dtw(".", 60, 61, 70),
            special("[_TT_61]", 61),
        ];
        // whisper.cpp's t_dtw of a token is the DTW jump into that token's own decoder row,
        // which predicts the next token: a word starts at the preceding token's t_dtw and
        // ends at its own last token's (openai/whisper timing.py `find_alignment`). The first
        // word of the call has no preceding token and starts at its token-level t0.
        let words = assemble_words(&tokens, 0, 90, DTW);
        assert_eq!(texts(&words), ["Hello,", "world."]);
        assert_eq!(spans(&words), [(100, 300), (300, 700)]);
        // The first word of a later segment starts at the previous segment's last token.
        let after = Timing::Dtw { preceding: 8 };
        assert_eq!(
            spans(&assemble_words(&tokens, 0, 90, after)),
            [(80, 300), (300, 700)]
        );
        // The token-timestamp rule on the same tokens is unchanged.
        assert_eq!(
            spans(&assemble_words(&tokens, 0, 90, TOKENS)),
            [(100, 310), (400, 610)]
        );
    }

    #[test]
    fn dtw_words_without_a_dtw_time_fall_back_to_token_timestamps() {
        // whisper.cpp leaves t_dtw at -1 for a token the alignment did not reach.
        let tokens = [
            dtw(" a", 10, 20, 12),
            dtw(" b", 30, 40, -1),
            dtw(" c", 50, 60, 55),
        ];
        let words = assemble_words(&tokens, 0, 100, DTW);
        assert_eq!(spans(&words), [(100, 120), (120, 400), (500, 550)]);
    }

    #[test]
    fn dtw_word_times_are_clamped_into_the_segment_and_ordered() {
        let tokens = [
            dtw(" a", 0, 0, 5),
            dtw(" b", 0, 0, 40),
            dtw(" c", 0, 0, 30),
            dtw(" d", 0, 0, 500),
        ];
        let words = assemble_words(&tokens, 20, 100, DTW);
        // Times before the segment start at it, c's end (30) is before its start (40), d's
        // start (c's 30) is kept after c's, and d's end lies past the segment end.
        assert_eq!(
            spans(&words),
            [(200, 200), (200, 400), (400, 400), (400, 1000)]
        );
        // A preceding time from an earlier window lies before the segment.
        let late = Timing::Dtw { preceding: 5 };
        assert_eq!(
            spans(&assemble_words(&tokens[3..], 20, 100, late)),
            [(200, 1000)]
        );
    }

    #[test]
    fn dtw_whitespace_tokens_still_precede_the_next_word() {
        let tokens = [
            dtw(" a", 0, 10, 10),
            dtw(" ", 10, 20, 15),
            dtw(" b", 20, 30, 25),
        ];
        let words = assemble_words(&tokens, 0, 40, DTW);
        assert_eq!(texts(&words), ["a", "b"]);
        assert_eq!(spans(&words), [(0, 100), (150, 250)]);
    }

    #[test]
    fn dtw_last_text_token_feeds_the_next_segment() {
        let tokens = [
            dtw(" a", 0, 10, 10),
            dtw("b", 10, 20, 17),
            special("[_TT_20]", 20),
        ];
        assert_eq!(last_dtw(&tokens), 17);
        assert_eq!(last_dtw(&[special("[_BEG_]", 0)]), -1);
        assert_eq!(last_dtw(&[]), -1);
    }

    #[test]
    fn tokens_join_into_words_on_leading_spaces() {
        let tokens = [
            special("[_BEG_]", 0),
            tok(" He", 10, 20, 0.9),
            tok("llo", 20, 30, 0.7),
            tok(",", 30, 31, 0.5),
            tok(" world", 40, 60, 0.8),
            tok(".", 60, 61, 1.0),
            special("[_TT_61]", 61),
        ];
        let words = assemble_words(&tokens, 0, 100, TOKENS);
        assert_eq!(texts(&words), ["Hello,", "world."]);
        assert_eq!(words[0].start.as_millis(), 100);
        assert_eq!(words[0].end.as_millis(), 310);
        assert_eq!(words[1].start.as_millis(), 400);
        assert_eq!(words[1].end.as_millis(), 610);
        assert!((words[0].probability - 0.7).abs() < 1e-6);
        assert!((words[1].probability - 0.9).abs() < 1e-6);
    }

    #[test]
    fn first_token_without_space_still_starts_a_word() {
        let words = assemble_words(
            &[tok("Yes", 0, 10, 1.0), tok(" no", 10, 20, 1.0)],
            0,
            20,
            TOKENS,
        );
        assert_eq!(texts(&words), ["Yes", "no"]);
    }

    #[test]
    fn multibyte_characters_split_across_tokens_are_rejoined() {
        let e_acute = "\u{e9}".as_bytes();
        let first = [b" caf".as_slice(), &e_acute[..1]].concat();
        let tokens = [
            Token {
                text: &first,
                ..tok("", 0, 10, 1.0)
            },
            Token {
                text: &e_acute[1..],
                ..tok("", 10, 20, 1.0)
            },
        ];
        for timing in [TOKENS, DTW] {
            assert_eq!(
                texts(&assemble_words(&tokens, 0, 20, timing)),
                ["caf\u{e9}"]
            );
        }
    }

    #[test]
    fn word_times_are_clamped_into_the_segment_and_ordered() {
        let tokens = [
            tok(" a", -1, -1, 0.5),
            tok(" b", 50, 40, 0.5),
            tok(" c", 30, 35, 0.5),
            tok(" d", 90, 500, 0.5),
        ];
        let words = assemble_words(&tokens, 20, 100, TOKENS);
        assert_eq!(
            spans(&words),
            [(200, 200), (500, 500), (500, 500), (900, 1000)]
        );
    }

    #[test]
    fn whitespace_only_tokens_and_probabilities_are_sanitised() {
        for timing in [TOKENS, DTW] {
            let words = assemble_words(&[tok(" ", 0, 1, 0.5), tok(" ok", 1, 2, 1.5)], 0, 2, timing);
            assert_eq!(texts(&words), ["ok"]);
            assert_eq!(words[0].probability, 1.0);
            assert!(assemble_words(&[], 0, 0, timing).is_empty());
        }
    }

    #[test]
    fn centiseconds_convert_to_milliseconds() {
        assert_eq!(centis_to_millis(0), 0);
        assert_eq!(centis_to_millis(123), 1230);
        assert_eq!(centis_to_millis(-1), 0);
    }
}
