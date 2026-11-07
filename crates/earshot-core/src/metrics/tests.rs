//! Hand-computed reference cases for every metric.

use super::der::{AMI_COLLAR_PER_SIDE_S, DerConfig, Turn, der};
use super::erle::{ErleConfig, segmental_erle};
use super::hungarian::max_weight_assignment;
use super::normalize::normalize_english;
use super::sisdr::si_sdr;
use super::wer::{WerCounts, corpus_wer, word_errors, word_errors_normalized};

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

// ---------------------------------------------------------------- WER

#[test]
fn wer_identical_is_zero() {
    let c = word_errors("the cat sat", "the cat sat");
    assert_eq!(
        c,
        WerCounts {
            substitutions: 0,
            deletions: 0,
            insertions: 0,
            ref_words: 3
        }
    );
    assert_eq!(c.wer(), Some(0.0));
}

#[test]
fn wer_one_substitution() {
    // the cat sat -> the bat sat: 1 S over 3 ref words.
    let c = word_errors("the cat sat", "the bat sat");
    assert_eq!((c.substitutions, c.deletions, c.insertions), (1, 0, 0));
    assert!(close(c.wer().unwrap(), 1.0 / 3.0, 1e-12));
}

#[test]
fn wer_deletion_and_insertion() {
    // ref: a b c d ; hyp: a c d e -> delete b, insert e: 2 errors / 4.
    let c = word_errors("a b c d", "a c d e");
    assert_eq!((c.substitutions, c.deletions, c.insertions), (0, 1, 1));
    assert_eq!(c.errors(), 2);
    assert!(close(c.wer().unwrap(), 0.5, 1e-12));
}

#[test]
fn wer_mixed_breakdown() {
    // ref: the quick brown fox jumps ; hyp: the quack fox jumps over
    // the=the, quick->quack (S), brown (D), fox, jumps, over (I): S1 D1 I1 over 5.
    let c = word_errors("the quick brown fox jumps", "the quack fox jumps over");
    assert_eq!((c.substitutions, c.deletions, c.insertions), (1, 1, 1));
    assert!(close(c.wer().unwrap(), 3.0 / 5.0, 1e-12));
}

#[test]
fn wer_empty_hypothesis_is_all_deletions_and_can_exceed_one() {
    let c = word_errors("one two", "");
    assert_eq!((c.substitutions, c.deletions, c.insertions), (0, 2, 0));
    let c = word_errors("a", "x y z");
    assert_eq!(c.errors(), 3);
    assert!(close(c.wer().unwrap(), 3.0, 1e-12));
}

#[test]
fn wer_empty_reference_has_no_rate() {
    let c = word_errors("", "");
    assert_eq!(c.wer(), None);
    let c = word_errors("", "noise");
    assert_eq!(c.insertions, 1);
    assert_eq!(c.wer(), None);
}

#[test]
fn corpus_wer_is_total_errors_over_total_ref_words_not_mean_of_rates() {
    // utt1: 1 error / 1 word (WER 1.0); utt2: 0 errors / 9 words (WER 0).
    // Mean of rates would be 0.5; corpus WER is 1/10.
    let a = word_errors("hello", "yellow");
    let b = word_errors("a b c d e f g h i", "a b c d e f g h i");
    let total = corpus_wer([a, b]);
    assert_eq!(total.ref_words, 10);
    assert_eq!(total.errors(), 1);
    assert!(close(total.wer().unwrap(), 0.1, 1e-12));
}

#[test]
fn wer_normalized_applies_the_normaliser_to_both_sides() {
    let c = word_errors_normalized(
        "MISTER SMITH PAID TWENTY FIVE DOLLARS, DIDN'T HE?",
        "Mr. Smith paid 25 dollars didn't he",
    );
    assert_eq!(c.errors(), 0, "{c:?}");
}

// ---------------------------------------------------------------- normaliser

#[test]
fn normaliser_case_and_punctuation() {
    assert_eq!(normalize_english("Hello, World!"), "hello world");
    assert_eq!(normalize_english("  so...   what?  "), "so what");
}

#[test]
fn normaliser_removes_bracketed_and_fillers() {
    assert_eq!(
        normalize_english("[NOISE] hello (laughs) <unk> there"),
        "hello there"
    );
    assert_eq!(
        normalize_english("Um, I think, uh, yes. Hmm"),
        "i think yes"
    );
    // fillers only as whole words
    assert_eq!(normalize_english("umbrella"), "umbrella");
}

#[test]
fn normaliser_contractions() {
    assert_eq!(normalize_english("I won't go"), "i will not go");
    assert_eq!(normalize_english("you can't"), "you can not");
    assert_eq!(normalize_english("they're here"), "they are here");
    assert_eq!(normalize_english("didn't"), "did not");
    assert_eq!(normalize_english("I'm sure we'll"), "i am sure we will");
    assert_eq!(normalize_english("she's been"), "she has been");
    assert_eq!(normalize_english("let's go, gonna"), "let us go going to");
    assert_eq!(normalize_english("I've"), "i have");
}

#[test]
fn normaliser_titles() {
    assert_eq!(
        normalize_english("Dr. Who and Mr Jones"),
        "doctor who and mister jones"
    );
}

#[test]
fn normaliser_numbers() {
    assert_eq!(normalize_english("twenty five"), "25");
    assert_eq!(normalize_english("one hundred and five"), "105");
    assert_eq!(normalize_english("two thousand twenty four"), "2024");
    assert_eq!(normalize_english("nineteen ninety nine"), "1999");
    assert_eq!(normalize_english("three million"), "3000000");
    assert_eq!(
        normalize_english("the twenty first century"),
        "the 21st century"
    );
    assert_eq!(normalize_english("first and third"), "1st and 3rd");
    assert_eq!(normalize_english("fifty percent"), "50%");
    assert_eq!(normalize_english("three point one four"), "3.14");
    assert_eq!(normalize_english("1,000 people"), "1000 people");
    assert_eq!(normalize_english("$3.50 and 20%"), "$3.50 and 20%");
    // Whisper turns every ordinal word into digits, even "second" on its own
    // (checked against upstream `EnglishTextNormalizer`, see `whisper_reference.rs`).
    assert_eq!(normalize_english("wait a second"), "wait a 2nd");
    assert_eq!(normalize_english("twenty second"), "22nd");
}

#[test]
fn normaliser_spellings_and_diacritics() {
    assert_eq!(
        normalize_english("The colour of the centre"),
        "the color of the center"
    );
    assert_eq!(normalize_english("I realise"), "i realize");
    assert_eq!(normalize_english("café naïve"), "cafe naive");
}

#[test]
fn normaliser_symbols_not_attached_to_numbers_are_removed() {
    assert_eq!(
        normalize_english("a $ sign and % alone"),
        "a sign and alone"
    );
}

// ---------------------------------------------------------------- Hungarian

#[test]
fn hungarian_finds_max_weight_assignment() {
    // Rows x cols weights; best is r0->c1 (7), r1->c0 (6), r2->c2 (9) = 22.
    let w = vec![
        vec![5.0, 7.0, 1.0],
        vec![6.0, 4.0, 2.0],
        vec![1.0, 3.0, 9.0],
    ];
    let a = max_weight_assignment(&w);
    assert_eq!(a, vec![Some(1), Some(0), Some(2)]);
}

#[test]
fn hungarian_rectangular() {
    // 2 rows, 3 cols: r0->c2 (8), r1->c0 (5) = 13 beats r0->c0 + r1->c1 = 4+1.
    let w = vec![vec![4.0, 2.0, 8.0], vec![5.0, 1.0, 7.0]];
    assert_eq!(max_weight_assignment(&w), vec![Some(2), Some(0)]);
    // 3 rows, 1 col: only the heaviest row is assigned.
    let w = vec![vec![1.0], vec![3.0], vec![2.0]];
    assert_eq!(max_weight_assignment(&w), vec![None, Some(0), None]);
    // empty
    assert!(max_weight_assignment(&[]).is_empty());
}

// ---------------------------------------------------------------- DER

fn t(start: f64, end: f64, speaker: &str) -> Turn {
    Turn::new(start, end, speaker)
}

const NO_COLLAR: DerConfig = DerConfig::nist(0.0);

#[test]
fn der_perfect_with_different_labels_is_zero() {
    let r = [t(0.0, 5.0, "A"), t(5.0, 10.0, "B")];
    let h = [t(0.0, 5.0, "spk1"), t(5.0, 10.0, "spk0")];
    let d = der(&r, &h, &NO_COLLAR);
    assert!(close(d.total, 10.0, 1e-9));
    assert!(close(d.der(), 0.0, 1e-12), "{d:?}");
}

#[test]
fn der_confusion_one_hyp_speaker_for_two_ref_speakers() {
    // Hyp X covers A and B; X maps to one of them, other 10 s are confusion: 10/20.
    let r = [t(0.0, 10.0, "A"), t(10.0, 20.0, "B")];
    let h = [t(0.0, 20.0, "X")];
    let d = der(&r, &h, &NO_COLLAR);
    assert!(close(d.confusion, 10.0, 1e-9), "{d:?}");
    assert!(close(d.missed, 0.0, 1e-9));
    assert!(close(d.false_alarm, 0.0, 1e-9));
    assert!(close(d.der(), 0.5, 1e-12));
}

#[test]
fn der_missed_and_false_alarm() {
    // ref A [0,10]; hyp X [0,5], Y [12,14]. No UEM -> extent [0,14].
    // total 10; missed 5 (5..10); false alarm 2 (12..14). X->A, Y unmapped.
    let r = [t(0.0, 10.0, "A")];
    let h = [t(0.0, 5.0, "X"), t(12.0, 14.0, "Y")];
    let d = der(&r, &h, &NO_COLLAR);
    assert!(close(d.total, 10.0, 1e-9));
    assert!(close(d.missed, 5.0, 1e-9));
    assert!(close(d.false_alarm, 2.0, 1e-9));
    assert!(close(d.confusion, 0.0, 1e-9));
    assert!(close(d.der(), 0.7, 1e-12));
}

#[test]
fn der_scores_overlap() {
    // ref A [0,10] and B [5,10] overlap; hyp X [0,10] only.
    // total = 10 + 5 = 15; in [5,10] two ref speakers, one hyp: 5 s missed.
    let r = [t(0.0, 10.0, "A"), t(5.0, 10.0, "B")];
    let h = [t(0.0, 10.0, "X")];
    let d = der(&r, &h, &NO_COLLAR);
    assert!(close(d.total, 15.0, 1e-9));
    assert!(close(d.missed, 5.0, 1e-9));
    assert!(close(d.der(), 1.0 / 3.0, 1e-12));
}

#[test]
fn der_overlap_with_wrong_second_speaker_is_confusion() {
    // ref A [0,10], B [5,10]; hyp X [0,10], Y [5,10], Z... mapping X->A, Y->B: perfect.
    let r = [t(0.0, 10.0, "A"), t(5.0, 10.0, "B")];
    let h = [t(0.0, 10.0, "X"), t(5.0, 10.0, "Y")];
    assert!(close(der(&r, &h, &NO_COLLAR).der(), 0.0, 1e-12));
    // Same ref, hyp X [0,10], Y [5,7], Z [7,10]: Z cannot also map to B.
    // X->A (10), Y->B (2) or Z->B (3): best is Z->B; Y [5,7] is confusion 2 s.
    let h = [t(0.0, 10.0, "X"), t(5.0, 7.0, "Y"), t(7.0, 10.0, "Z")];
    let d = der(&r, &h, &NO_COLLAR);
    assert!(close(d.confusion, 2.0, 1e-9), "{d:?}");
    assert!(close(d.der(), 2.0 / 15.0, 1e-12));
}

#[test]
fn der_collar_forgives_boundary_errors() {
    // Hyp boundary is 0.1 s late. Without collar: 0.1 s confusion / 20.
    let r = [t(0.0, 10.0, "A"), t(10.0, 20.0, "B")];
    let h = [t(0.0, 10.1, "X"), t(10.1, 20.0, "Y")];
    let d = der(&r, &h, &NO_COLLAR);
    assert!(close(d.der(), 0.1 / 20.0, 1e-9), "{d:?}");
    // collar 0.5 (pyannote convention: +/-0.25 s around each reference boundary).
    // Removed: [0,0.25] at 0, [9.75,10.25] at 10, [19.75,20] at 20 -> total 19.
    let cfg = DerConfig::pyannote(0.5);
    let d = der(&r, &h, &cfg);
    assert!(close(d.total, 19.0, 1e-9), "{d:?}");
    assert!(close(d.der(), 0.0, 1e-12), "{d:?}");
}

#[test]
fn der_nist_collar_is_per_side_and_doubles_into_total_width() {
    assert_eq!(AMI_COLLAR_PER_SIDE_S, 0.25);
    let cfg = DerConfig::nist(AMI_COLLAR_PER_SIDE_S);
    assert_eq!(cfg, DerConfig::pyannote(0.5));
    assert_eq!(cfg.collar_total_s(), 0.5);
    assert_eq!(cfg.uem(), None);
    // Same scenario as above: +/-0.25 s forgives the 0.1 s late boundary.
    let r = [t(0.0, 10.0, "A"), t(10.0, 20.0, "B")];
    let h = [t(0.0, 10.1, "X"), t(10.1, 20.0, "Y")];
    let d = der(&r, &h, &cfg);
    assert!(close(d.total, 19.0, 1e-9), "{d:?}");
    assert!(close(d.der(), 0.0, 1e-12), "{d:?}");
    // A UEM can be attached without touching the collar.
    let uem = vec![(0.0, 5.0)];
    let with = DerConfig::nist(0.1).with_uem(uem.clone());
    assert_eq!(with.collar_total_s(), 0.2);
    assert_eq!(with.uem(), Some(uem.as_slice()));
}

#[test]
fn der_uem_limits_the_scored_region() {
    // Only [0,5] is scored. Errors after 5 s do not count.
    let r = [t(0.0, 10.0, "A")];
    let h = [t(0.0, 4.0, "X"), t(6.0, 10.0, "Y")];
    let cfg = DerConfig::nist(0.0).with_uem(vec![(0.0, 5.0)]);
    let d = der(&r, &h, &cfg);
    assert!(close(d.total, 5.0, 1e-9));
    assert!(close(d.missed, 1.0, 1e-9));
    assert!(close(d.der(), 0.2, 1e-12));
}

#[test]
fn der_same_speaker_overlapping_turns_count_once() {
    let r = [t(0.0, 6.0, "A"), t(4.0, 10.0, "A")];
    let h = [t(0.0, 10.0, "X")];
    let d = der(&r, &h, &NO_COLLAR);
    assert!(close(d.total, 10.0, 1e-9));
    assert!(close(d.der(), 0.0, 1e-12));
}

// ---------------------------------------------------------------- SI-SDR

#[test]
fn si_sdr_hand_case() {
    // s = [1,-1,1,-1] (zero mean, energy 4); n = 0.5*[1,1,-1,-1] (orthogonal to s,
    // zero mean, energy 1). est = s + n -> alpha = 1, SI-SDR = 10 log10(4/1).
    let s = [1.0f32, -1.0, 1.0, -1.0];
    let est = [1.5f32, -0.5, 0.5, -1.5];
    let v = si_sdr(&est, &s).unwrap();
    assert!(close(v, 10.0 * 4.0f64.log10(), 1e-6), "{v}");
}

#[test]
fn si_sdr_is_scale_invariant_and_mean_invariant() {
    let s = [1.0f32, -1.0, 1.0, -1.0];
    let est: Vec<f32> = [1.5f32, -0.5, 0.5, -1.5]
        .iter()
        .map(|x| 3.0 * x + 0.7)
        .collect();
    let v = si_sdr(&est, &s).unwrap();
    assert!(close(v, 10.0 * 4.0f64.log10(), 1e-5), "{v}");
}

#[test]
fn si_sdr_perfect_is_infinite_and_bad_input_is_none() {
    let s = [1.0f32, -2.0, 3.0, -2.0];
    assert_eq!(si_sdr(&s, &s), Some(f64::INFINITY));
    assert_eq!(si_sdr(&s[..3], &s), None, "length mismatch");
    assert_eq!(si_sdr(&[0.0; 4], &[0.0; 4]), None, "silent reference");
    assert_eq!(si_sdr(&[], &[]), None);
}

#[test]
fn si_sdr_silent_estimate_is_negative_infinity() {
    let s = [1.0f32, -2.0, 3.0, -2.0];
    assert_eq!(si_sdr(&[0.0; 4], &s), Some(f64::NEG_INFINITY));
    // A constant (DC-only) estimate is silent after mean removal.
    assert_eq!(si_sdr(&[0.5; 4], &s), Some(f64::NEG_INFINITY));
}

#[test]
fn si_sdr_near_silent_estimate_is_very_negative() {
    let s = [1.0f32, -2.0, 3.0, -2.0];
    let est: Vec<f32> = s.iter().map(|x| x * 1e-12).collect();
    let v = si_sdr(&est, &s).unwrap();
    assert!(v < -100.0, "{v}");
}

#[test]
fn si_sdr_quiet_but_correct_estimate_still_scores_normally() {
    // Just above the silence epsilon (energy per sample ~4.5e-18 vs 1e-20): SI-SDR is
    // scale invariant, so a quiet copy must still score far above any threshold (f32
    // rounding of the 1e-9 scale keeps it finite, about 158 dB, rather than +inf).
    let s = [1.0f32, -2.0, 3.0, -2.0];
    let est: Vec<f32> = s.iter().map(|x| x * 1e-9).collect();
    let v = si_sdr(&est, &s).unwrap();
    assert!(v > 100.0, "{v}");
}

// ---------------------------------------------------------------- ERLE

#[test]
fn erle_is_median_of_one_second_windows_after_two_seconds() {
    // sr = 100 Hz, 5 s of a +/-1 square wave at the mic. The output is the mic scaled by
    // a per-second gain: 1, 1 (excluded, t < 2 s), then 0.1, 0.01, 0.5 ->
    // ERLE 20 dB, 40 dB, 6.0206 dB; median = 20 dB.
    let sr = 100u32;
    let gains = [1.0f32, 1.0, 0.1, 0.01, 0.5];
    let mic: Vec<f32> = (0..500)
        .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
        .collect();
    let out: Vec<f32> = mic
        .iter()
        .enumerate()
        .map(|(i, x)| x * gains[i / 100])
        .collect();
    let r = segmental_erle(&mic, &out, sr, &ErleConfig::default()).unwrap();
    assert_eq!(r.windows_db.len(), 3);
    assert!(close(r.windows_db[0], 20.0, 1e-4));
    assert!(close(r.windows_db[1], 40.0, 1e-4));
    assert!(close(r.windows_db[2], 10.0 * 4.0f64.log10(), 1e-4));
    assert!(close(r.median_db, 20.0, 1e-4), "{r:?}");
}

#[test]
fn erle_even_window_count_takes_mean_of_middle_pair_and_drops_partial_window() {
    // 4.5 s at 10 Hz: windows [2,3) and [3,4) scored; the partial [4,4.5) is dropped.
    let sr = 10u32;
    let mic = vec![1.0f32; 45];
    let out: Vec<f32> = (0..45)
        .map(|i| match i / 10 {
            2 => 0.1,
            3 => 0.001,
            _ => 1.0,
        })
        .collect();
    let r = segmental_erle(&mic, &out, sr, &ErleConfig::default()).unwrap();
    assert_eq!(r.windows_db.len(), 2);
    assert!(close(r.median_db, (20.0 + 60.0) / 2.0, 1e-3), "{r:?}");
}

#[test]
fn erle_needs_windows_and_matching_lengths() {
    let cfg = ErleConfig::default();
    assert!(
        segmental_erle(&[1.0; 20], &[1.0; 20], 10, &cfg).is_none(),
        "under 3 s"
    );
    assert!(
        segmental_erle(&[1.0; 40], &[1.0; 30], 10, &cfg).is_none(),
        "length mismatch"
    );
    // Silent far end windows are skipped, a fully silent recording has no ERLE.
    assert!(segmental_erle(&[0.0; 50], &[0.0; 50], 10, &cfg).is_none());
}

#[test]
fn erle_time_to_target_needs_a_sustained_crossing() {
    use super::erle::{ConvergenceConfig, time_to_erle};
    let sr = 100u32;
    let cfg = ConvergenceConfig {
        window_s: 0.5,
        hop_s: 0.1,
        hold_s: 1.0,
        mic_floor: 1e-6,
    };
    let n = 600; // 6 s
    let mic = vec![1.0f32; n];
    // Output attenuation: 0 dB until 1 s, a lone 20 dB blip at 1.0-1.5 s, back to 0 dB,
    // then 20 dB from 3 s on.
    let out: Vec<f32> = (0..n)
        .map(|i| {
            if (100..150).contains(&i) || i >= 300 {
                0.1
            } else {
                1.0
            }
        })
        .collect();
    let t = time_to_erle(&mic, &out, sr, 0.0, 15.0, &cfg).unwrap();
    // The blip window is not sustained; the first window fully past 3 s ends at 3.5 s.
    assert!((t - 3.5).abs() < 1e-9, "{t}");
    // Measured from an event at 2 s.
    let t2 = time_to_erle(&mic, &out, sr, 2.0, 15.0, &cfg).unwrap();
    assert!((t2 - 1.5).abs() < 1e-9, "{t2}");
    // Never reached.
    assert_eq!(time_to_erle(&mic, &mic, sr, 0.0, 15.0, &cfg), None);
    // Silent mic windows are not scored.
    assert_eq!(
        time_to_erle(&[0.0; 100], &[0.0; 100], sr, 0.0, 15.0, &cfg),
        None
    );
    assert_eq!(time_to_erle(&mic, &out[..10], sr, 0.0, 15.0, &cfg), None);
}

#[test]
fn erle_time_to_target_holds_on_the_following_windows_only() {
    // T6.6b: the hold is the median of the windows after the crossing (the "following
    // 1 s"), not including the crossing window itself, and a true median (mean of the two
    // middle values for an even count).
    use super::erle::{ConvergenceConfig, time_to_erle};
    let sr = 100u32;
    let cfg = ConvergenceConfig {
        window_s: 0.1,
        hop_s: 0.1,
        hold_s: 1.0,
        mic_floor: 1e-6,
    };
    let mic = vec![1.0f32; 300];
    // 10 windows of 0 dB, the crossing window at 20 dB, then 5 at 16 dB and 5 at 13 dB
    // (median of the following 10: 14.5 dB), then 0 dB to the end.
    let db = |i: usize| match i / 10 {
        10 => 20.0,
        11..=15 => 16.0,
        16..=20 => 13.0,
        _ => 0.0,
    };
    let out: Vec<f32> = (0..300).map(|i| 10f32.powf(-db(i) / 20.0)).collect();
    assert_eq!(time_to_erle(&mic, &out, sr, 0.0, 15.0, &cfg), None);
    // With the following windows at 16 dB throughout the crossing holds.
    let out2: Vec<f32> = (0..300)
        .map(|i| {
            let d = if (110..210).contains(&i) { 16.0 } else { db(i) };
            10f32.powf(-d / 20.0)
        })
        .collect();
    let t = time_to_erle(&mic, &out2, sr, 0.0, 15.0, &cfg).unwrap();
    assert!((t - 1.1).abs() < 1e-9, "{t}");
}
