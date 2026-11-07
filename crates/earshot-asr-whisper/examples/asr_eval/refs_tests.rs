//! Tests of `refs.rs`, kept out of it so that only `asr_eval` runs them: `model_eval`,
//! `diar_eval` and `cargo xtask t613` compile `refs.rs` by path and must not compile (or run
//! again) its tests. They use only `refs`' public items.

use std::path::Path;

use earshot_core::asr::{Transcription, Word};
use earshot_core::audio::PIPELINE_RATE;
use earshot_core::metrics::wer::WerCounts;

use super::refs::*;

#[test]
fn asr_eval_aec_seeds_start_at_the_scenario_seed_and_stop_at_enough_words() {
    let words = |seed: u64| ((), if seed == 103 { 35 } else { 40 });
    let seeds: Vec<u64> = render_seeds(103, 1_000, 300, 32, words)
        .unwrap()
        .into_iter()
        .map(|(s, ())| s)
        .collect();
    // 35 + 7 x 40 = 315 >= 300; 35 + 6 x 40 = 275 is not enough.
    assert_eq!(
        seeds,
        [103, 1_103, 2_103, 3_103, 4_103, 5_103, 6_103, 7_103]
    );
    // Exactly the minimum stops.
    assert_eq!(
        render_seeds(1, 10, 300, 32, |_| ((), 100)).unwrap().len(),
        3
    );
    // Too few words within the cap is an error, not a smaller set.
    assert!(render_seeds(1, 10, 300, 4, |_| ((), 10)).is_err());
}

const XML: &str = r#"<?xml version="1.0" encoding="ISO-8859-1" standalone="yes"?>
<nite:root nite:id="IS1009a.B.words" xmlns:nite="http://nite.sourceforge.net/">
   <w nite:id="IS1009a.B.words0" starttime="66.63" endtime="67.39">Okay</w>
   <w nite:id="IS1009a.B.words1" starttime="67.39" endtime="67.39" punc="true">.</w>
   <w nite:id="IS1009a.B.words10" starttime="69.36" endtime="69.85">I&#39;m</w>
   <disfmarker nite:id="IS1009a.B.words15" starttime="71.39" endtime="71.39"/>
   <vocalsound nite:id="x" starttime="72.0" endtime="72.5" type="laugh"/>
   <w nite:id="ES2004a.A.words15" starttime="80.5" endtime="80.65" trunc="true">Tu</w>
   <w nite:id="x2" starttime="81.0" endtime="81.5">L_C_D_</w>
</nite:root>
"#;

#[test]
fn asr_eval_ami_words_follow_the_reference_rules() {
    let w = parse_ami_words(XML).unwrap();
    let texts: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
    assert_eq!(texts, ["Okay", "I'm", "LCD"]);
    assert_eq!(w[0].start, 66.63);
    assert!(parse_ami_words("<w nite:id=\"a\">x</w>").is_err());
    assert!(parse_ami_words("<w starttime=\"1\">x").is_err());
}

#[test]
fn asr_eval_ami_reference_merges_speakers_by_time_inside_the_uem() {
    let w = |start: f64, t: &str| AmiWord {
        start,
        text: t.into(),
    };
    let a = vec![w(1.0, "a1"), w(3.0, "a3"), w(10.0, "late")];
    let b = vec![w(0.5, "early"), w(2.0, "b2"), w(3.0, "b3")];
    assert_eq!(ami_reference(&[a, b], 1.0, 10.0), "a1 b2 a3 b3");
}

#[test]
fn asr_eval_ami_channels_map_headsets_to_speakers() {
    let xml = r#"<meetings>
   <meeting nite:id="meet_49" observation="ES2004a" duration="1186">
      <speaker nite:id="ES2004a_2" channel="1" nxt_agent="B" global_name="FEE013"/>
      <speaker nite:id="ES2004a_3" channel="2" nxt_agent="C" global_name="MEE014"/>
      <speaker nite:id="ES2004a_1" channel="0" nxt_agent="A" global_name="MEO015"/>
      <speaker nite:id="ES2004a_4" channel="3" nxt_agent="D" global_name="FEE016"/>
   </meeting>
   <meeting nite:id="meet_50" observation="ES2004b" duration="2333">
      <speaker nite:id="ES2004b_2" channel="0" nxt_agent="B"/>
   </meeting>
</meetings>"#;
    let got = parse_ami_channels(xml, "ES2004a").unwrap();
    let want: Vec<(u8, String)> = vec![
        (0, "A".into()),
        (1, "B".into()),
        (2, "C".into()),
        (3, "D".into()),
    ];
    assert_eq!(got, want);
    // The mapping is read from the table, not assumed to be channel n -> letter n.
    let permuted = xml
        .replace(
            "channel=\"0\" nxt_agent=\"A\"",
            "channel=\"0\" nxt_agent=\"X\"",
        )
        .replace(
            "channel=\"2\" nxt_agent=\"C\"",
            "channel=\"2\" nxt_agent=\"A\"",
        )
        .replace("nxt_agent=\"X\"", "nxt_agent=\"C\"");
    let got = parse_ami_channels(&permuted, "ES2004a").unwrap();
    assert_eq!(got[0], (0, "C".to_owned()));
    assert_eq!(got[2], (2, "A".to_owned()));
    // Speakers never leak in from the next meeting; an incomplete meeting is an error.
    assert!(parse_ami_channels(xml, "ES2004b").is_err());
    assert!(parse_ami_channels(xml, "IS1009a").is_err());
}

#[test]
fn asr_eval_ami_segments_are_read_with_their_channel() {
    // Lines as in the official segments/IS1009a.B.segments.xml (v1.6.2), one out of order.
    let xml = r#"<?xml version="1.0" encoding="ISO-8859-1" standalone="yes"?>
<nite:root nite:id="IS1009a.B.segs" xmlns:nite="http://nite.sourceforge.net/">
   <segment nite:id="IS1009a.sync.156" channel="1" transcriber_start="283.776" transcriber_end="285.12">
      <nite:child href="IS1009a.B.words.xml#id(IS1009a.B.words40)"/>
   </segment>
   <segment nite:id="IS1009a.sync.154" channel="1" transcriber_start="66.608" transcriber_end="80.88">
      <nite:child href="IS1009a.B.words.xml#id(IS1009a.B.words0)..id(IS1009a.B.words39)"/>
   </segment>
</nite:root>"#;
    let segs = parse_ami_segments(xml).unwrap();
    assert_eq!(segs.channel, 1);
    assert_eq!(segs.spans, vec![(66.608, 80.88), (283.776, 285.12)]);
    // Mixed channels, a missing time, an empty span or no segment at all are errors.
    let mixed = xml.replacen("channel=\"1\"", "channel=\"2\"", 1);
    assert!(parse_ami_segments(&mixed).is_err());
    let no_end = xml.replacen(" transcriber_end=\"285.12\"", "", 1);
    assert!(parse_ami_segments(&no_end).is_err());
    let empty = xml.replacen(
        "transcriber_end=\"285.12\"",
        "transcriber_end=\"283.776\"",
        1,
    );
    assert!(parse_ami_segments(&empty).is_err());
    assert!(parse_ami_segments("<nite:root/>").is_err());
}

#[test]
fn asr_eval_ami_parse_cases_shared_with_fetch_fixtures() {
    // The same cases run against xtask's `fixtures::ami_headset_speakers` and
    // `fixtures::ami_segments` (`fixtures_ami_parse_cases_shared_with_asr_eval`).
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|d| d.join("fixtures/ami/parse_cases.toml"))
        .find(|p| p.is_file())
        .expect("fixtures/ami/parse_cases.toml above the crate");
    let text = std::fs::read_to_string(&path).unwrap();
    let cases: toml::Table = toml::from_str(&text).unwrap();
    let s = |v: &toml::Value, k: &str| v[k].as_str().unwrap().to_owned();
    let error = |v: &toml::Value| v.get("error").and_then(toml::Value::as_bool) == Some(true);
    let meetings = cases["meetings"].as_array().unwrap();
    let segments = cases["segments"].as_array().unwrap();
    assert!(meetings.len() >= 2 && segments.len() >= 2);
    for c in meetings {
        let (name, got) = (
            s(c, "name"),
            parse_ami_channels(&s(c, "xml"), &s(c, "meeting")),
        );
        if error(c) {
            assert!(got.is_err(), "{name}: {got:?}");
            continue;
        }
        let heads = c["headsets"].as_array().unwrap().iter();
        let spks = c["speakers"].as_array().unwrap().iter();
        let want: Vec<(u8, String)> = heads
            .zip(spks)
            .map(|(h, a)| {
                (
                    u8::try_from(h.as_integer().unwrap()).unwrap(),
                    a.as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(got.unwrap(), want, "{name}");
    }
    for c in segments {
        let (name, got) = (s(c, "name"), parse_ami_segments(&s(c, "xml")));
        if error(c) {
            assert!(got.is_err(), "{name}: {got:?}");
            continue;
        }
        let spans: Vec<(f64, f64)> = c["spans"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                let p = p.as_array().unwrap();
                (p[0].as_float().unwrap(), p[1].as_float().unwrap())
            })
            .collect();
        let want = AmiSegments {
            channel: u8::try_from(c["channel"].as_integer().unwrap()).unwrap(),
            spans,
        };
        assert_eq!(got.unwrap(), want, "{name}");
    }
}

#[test]
fn asr_eval_ami_segment_references_score_each_segment_as_an_utterance() {
    let w = |start: f64, t: &str| AmiWord {
        start,
        text: t.into(),
    };
    let words = vec![
        w(0.2, "before_uem"),
        w(0.8, "one"),
        w(1.0, "edge"),
        w(2.5, "two"),
        w(2.0, "first"),
        w(5.0, "gap"),
        w(9.7, "three"),
        w(10.2, "after_uem"),
        w(12.5, "outside_uem"),
    ];
    // Unsorted spans; UEM [0.75, 10.0) clips the first and third, drops the fourth.
    let spans = [(2.0, 3.0), (0.5, 1.0), (9.5, 10.5), (12.0, 13.0)];
    let r = segment_references(&words, &spans, 0.75, 10.0).unwrap();
    assert_eq!(r.spans, vec![(0.75, 1.0), (2.0, 3.0), (9.5, 10.0)]);
    // Start-inclusive, end-exclusive; words in time order within a segment.
    assert_eq!(r.references, ["one", "first two", "three"]);
    // "edge" starts at a segment end and "gap" between segments: kept as outside
    // words (scored as deletions); words outside the UEM are not scored at all.
    assert_eq!(r.outside, ["edge", "gap"]);
    // A segment with no words keeps an empty reference (its decode is all insertions).
    let r = segment_references(&words, &[(3.0, 4.0)], 0.0, 20.0).unwrap();
    assert_eq!(r.references, [""]);
    // Overlapping segments of one speaker would score a word twice: an error.
    assert!(segment_references(&words, &[(2.0, 3.0), (2.4, 2.6)], 0.0, 20.0).is_err());
}

#[test]
fn asr_eval_ami_segment_audio_is_rounded_and_clamped() {
    let rate = 16_000.0;
    assert_eq!(span_samples((0.75, 1.0), rate, 1_000_000), 12_000..16_000);
    // Times round to the nearest sample.
    assert_eq!(span_samples((0.000_03, 0.000_09), rate, 100), 0..1);
    // Clamped to the audio; empty past its end (the caller fails on that).
    assert_eq!(span_samples((2.0, 3.0), rate, 40_000), 32_000..40_000);
    assert_eq!(span_samples((9.5, 10.0), rate, 40_000), 40_000..40_000);
}

const HEADSET_WORDS: &str = r#"<nite:root>
   <w nite:id="a" starttime="0.2" endtime="0.4">before</w>
   <w nite:id="b" starttime="1.1" endtime="1.3">alpha</w>
   <w nite:id="c" starttime="1.5" endtime="1.7">beta</w>
   <w nite:id="d" starttime="2.5" endtime="2.7">gap</w>
   <w nite:id="e" starttime="3.1" endtime="3.3">gamma</w>
</nite:root>"#;
const HEADSET_SEGMENTS: &str = r#"<nite:root>
   <segment nite:id="s2" channel="2" transcriber_start="3.0" transcriber_end="3.5"></segment>
   <segment nite:id="s1" channel="2" transcriber_start="1.0" transcriber_end="2.0"></segment>
</nite:root>"#;

fn headset() -> AmiHeadset {
    AmiHeadset::new(
        "IS1009a",
        (2, "C"),
        (0.5, 10.0),
        HEADSET_WORDS,
        HEADSET_SEGMENTS,
        (AmiMic::Ihm, vec![0.0; 16_000 * 4].into()),
    )
    .unwrap()
}

#[test]
fn asr_eval_ami_headset_plans_one_utterance_per_segment() {
    let h = headset();
    assert_eq!(h.plan.spans, vec![(1.0, 2.0), (3.0, 3.5)]);
    assert_eq!(h.plan.references, ["alpha beta", "gamma"]);
    assert_eq!(h.plan.outside, ["gap"]);
    assert_eq!(h.ranges, vec![16_000..32_000, 48_000..56_000]);
    assert_eq!(
        h.segment_audio().map(<[f32]>::len).collect::<Vec<_>>(),
        [16_000, 8_000]
    );
    let inside: Vec<&str> = h.segment_words().iter().map(|w| w.text.as_str()).collect();
    assert_eq!(inside, ["alpha", "beta", "gamma"]);
    // Segments on another headset than meetings.xml says, or past the audio: errors.
    let wrong = AmiHeadset::new(
        "IS1009a",
        (1, "C"),
        (0.5, 10.0),
        HEADSET_WORDS,
        HEADSET_SEGMENTS,
        (AmiMic::Ihm, vec![0.0; 16_000 * 4].into()),
    );
    assert!(wrong.is_err());
    let short = AmiHeadset::new(
        "IS1009a",
        (2, "C"),
        (0.5, 10.0),
        HEADSET_WORDS,
        HEADSET_SEGMENTS,
        (AmiMic::Ihm, vec![0.0; 16_000 * 2].into()),
    );
    assert!(short.is_err());
}

/// A fixtures directory with one AMI meeting (four speakers, each with one segment and
/// one word), headset `n` filled with `n + 1`, and the array channel `Array1-01` filled
/// with 9 and holding `array_channels` channels; removed on drop.
struct FakeAmi(std::path::PathBuf);

impl FakeAmi {
    fn new(tag: &str, array_channels: u16) -> Self {
        use earshot_core::audio::{WavFormat, write_wav};
        let dir =
            std::env::temp_dir().join(format!("earshot-asr-eval-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |rel: &str, bytes: &[u8]| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        };
        // Headset n carries speaker letter "DCBA"[n], so the table is really read.
        let letters = ["D", "C", "B", "A"];
        let mut xml = String::from(
            "<meetings>\n<meeting nite:id=\"m\" observation=\"IS1009a\" duration=\"5\">\n",
        );
        for (n, l) in letters.iter().enumerate() {
            xml.push_str(&format!(
                "<speaker nite:id=\"s{n}\" channel=\"{n}\" nxt_agent=\"{l}\"/>\n"
            ));
            let start = n as f64;
            write(
                    &format!("ami/annotations/words/IS1009a.{l}.words.xml"),
                    format!(
                        "<nite:root><w nite:id=\"w\" starttime=\"{}\" endtime=\"{}\">word{l}</w>\
                         <w nite:id=\"x\" starttime=\"4.5\" endtime=\"4.6\">late{l}</w></nite:root>",
                        start + 0.2,
                        start + 0.4
                    )
                    .as_bytes(),
                );
            write(
                    &format!("ami/annotations/segments/IS1009a.{l}.segments.xml"),
                    format!(
                        "<nite:root><segment nite:id=\"g\" channel=\"{n}\" \
                         transcriber_start=\"{start}\" transcriber_end=\"{}\"></segment></nite:root>",
                        start + 0.5
                    )
                    .as_bytes(),
                );
        }
        xml.push_str("</meeting>\n</meetings>\n");
        write(
            "ami/annotations/corpusResources/meetings.xml",
            xml.as_bytes(),
        );
        write("ami/uem/IS1009a.uem", b"IS1009a 1 0.000 5.000\n");
        let wav = |channels: u16, value: f32| {
            let mut v = Vec::new();
            let samples = vec![value; 16_000 * 5 * usize::from(channels)];
            write_wav(
                &mut v,
                PIPELINE_RATE,
                channels,
                &samples,
                WavFormat::Float32,
            )
            .unwrap();
            v
        };
        for n in 0..4u8 {
            write(
                &format!("ami/audio/IS1009a.Headset-{n}.wav"),
                &wav(1, f32::from(n) + 1.0),
            );
        }
        write("ami/audio/IS1009a.Array1-01.wav", &wav(array_channels, 9.0));
        FakeAmi(dir)
    }
}

impl Drop for FakeAmi {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn asr_eval_ami_sdm_decodes_every_speakers_segments_from_the_array_channel() {
    let fake = FakeAmi::new("sdm", 1);
    let ihm = load_ami(&fake.0, &["IS1009a"], AmiMic::Ihm).unwrap();
    let sdm = load_ami(&fake.0, &["IS1009a"], AmiMic::Sdm).unwrap();
    assert_eq!(ihm, load_ami_ihm(&fake.0, &["IS1009a"]).unwrap());
    assert_eq!(sdm.len(), 4);
    for (i, s) in ihm.iter().zip(&sdm) {
        // Same speakers, headset check, oracle segments, references and sample ranges.
        assert_eq!(
            (&i.speaker, i.headset, &i.plan, &i.ranges, i.uem),
            (&s.speaker, s.headset, &s.plan, &s.ranges, s.uem)
        );
        assert_eq!(i.channel, format!("Headset-{}", i.headset));
        assert_eq!(s.channel, "Array1-01");
        let value = f32::from(i.headset) + 1.0;
        assert!(i.segment_audio().all(|a| a.iter().all(|&x| x == value)));
        // Every SDM segment comes from the one shared far-field channel.
        assert!(s.segment_audio().all(|a| a.iter().all(|&x| x == 9.0)));
    }
    let speakers: Vec<&str> = sdm.iter().map(|h| h.speaker.as_str()).collect();
    assert_eq!(speakers, ["D", "C", "B", "A"]);
    assert_eq!(sdm[2].plan.references, ["wordB"]);
    assert_eq!(sdm[2].plan.outside, ["lateB"]);
    // Scoring is the IHM rule unchanged.
    assert_eq!(sdm[2].score(&["wordB"]), ihm[2].score(&["wordB"]));
    // The array channel is loaded once per meeting and shared, not copied per speaker.
    assert!(
        sdm.windows(2)
            .all(|w| std::sync::Arc::ptr_eq(&w[0].audio, &w[1].audio))
    );
}

#[test]
fn asr_eval_ami_sdm_needs_one_channel_and_names_the_fetch_command() {
    let stereo = FakeAmi::new("sdm-stereo", 2);
    let e = load_ami(&stereo.0, &["IS1009a"], AmiMic::Sdm).unwrap_err();
    assert!(e.contains("Array1-01") && e.contains("channel"), "{e}");
    // A headset recording is still mono, so IHM loads.
    assert!(load_ami(&stereo.0, &["IS1009a"], AmiMic::Ihm).is_ok());
    let missing = FakeAmi::new("sdm-missing", 1);
    std::fs::remove_file(missing.0.join("ami/audio/IS1009a.Array1-01.wav")).unwrap();
    let e = load_ami(&missing.0, &["IS1009a"], AmiMic::Sdm).unwrap_err();
    assert!(e.contains(FETCH_AMI), "{e}");
}

#[test]
fn asr_eval_ami_headset_score_counts_outside_words_as_deletions() {
    let h = headset();
    let s = h.score(&["alpha beta", "gama"]).unwrap();
    assert_eq!(s.segments[0].errors(), 0);
    assert_eq!(
        (s.segments[1].substitutions, s.segments[1].ref_words),
        (1, 1)
    );
    assert_eq!((s.outside.deletions, s.outside.ref_words), (1, 1));
    let t = s.total();
    assert_eq!((t.errors(), t.ref_words), (2, 4));
    // One decode per segment, no more and no less.
    assert!(h.score(&["alpha beta"]).is_err());
    // No outside words: an empty outside bucket.
    let mut inside_only = h.clone();
    inside_only.plan.outside.clear();
    assert_eq!(
        inside_only.score(&["", ""]).unwrap().outside,
        WerCounts::default()
    );
}

#[test]
fn asr_eval_hypothesis_text_is_the_words_production_keeps() {
    use earshot_core::asr::{BackendInfo, BackendKind, MeetingTime, RawSegment};
    let seg = |text: &str, words: Vec<Word>| RawSegment {
        start: MeetingTime::from_millis(0),
        end: MeetingTime::from_millis(1_000),
        text: text.into(),
        words,
    };
    let t = Transcription {
        segments: vec![
            seg(
                "[BLANK_AUDIO]",
                vec![Word::new("[BLANK_AUDIO]", 0, 900, 0.9)],
            ),
            seg(
                " Okay, (buzzing) so.",
                vec![
                    Word::new("Okay,", 0, 300, 0.9),
                    Word::new("(buzzing)", 300, 600, 0.9),
                    Word::new("so.", 600, 900, 0.9),
                ],
            ),
        ],
        language: Some("en".into()),
        backend: BackendInfo {
            kind: BackendKind::Cpu,
            device: "cpu".into(),
            model: "m".into(),
        },
    };
    assert_eq!(hypothesis_text(&kept_words(&t)), "Okay, so.");
    assert_eq!(hypothesis_text(&[]), "");
}

#[test]
fn asr_eval_uem_and_latin1() {
    assert_eq!(
        parse_uem("IS1009a 1 0.000 838.833313\n", "IS1009a").unwrap(),
        (0.0, 838.833313)
    );
    assert!(parse_uem("ES2004a 1 0.000 1.0\n", "IS1009a").is_err());
    assert!(parse_uem("IS1009a 1 2 1\n", "IS1009a").is_err());
    assert!(parse_uem("IS1009a 1 0 1\nIS1009a 1 1 2\n", "IS1009a").is_err());
    assert_eq!(latin1(&[b'a', 0xe9]), "a\u{e9}");
}

#[test]
fn asr_eval_streams_cover_every_utterance_once() {
    let d = [20.0, 20.0, 25.0, 30.0, 31.0, 10.0];
    let s = group_streams(&d, 60.0);
    assert_eq!(s, [0..3, 3..6]);
    // A long remainder is its own stream.
    assert_eq!(group_streams(&[61.0, 40.0], 60.0), [0..1, 1..2]);
    assert_eq!(group_streams(&[10.0], 60.0), std::slice::from_ref(&(0..1)));
    assert!(group_streams(&[], 60.0).is_empty());
    for (d, t) in [(vec![7.0; 50], 60.0), (vec![3.5, 12.0, 61.0, 1.0], 60.0)] {
        let s = group_streams(&d, t);
        let covered: Vec<usize> = s.iter().flat_map(Clone::clone).collect();
        assert_eq!(covered, (0..d.len()).collect::<Vec<_>>());
    }
}

#[test]
fn asr_eval_windows_split_gaps_and_cover_between_spans() {
    let w = windows(&[100..200, 300..400, 1000..1100], 1200, 50);
    assert_eq!(w, [50..250, 250..700, 700..1150]);
    let single = windows(std::slice::from_ref(&(10..20)), 25, 50);
    assert_eq!(single, std::slice::from_ref(&(0..25)));
    assert!(windows(&[], 10, 5).is_empty());
}
