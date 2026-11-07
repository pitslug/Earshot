//! `cargo xtask bench-meeting` (T7.4c): the benchmark meeting shipped with the app, made
//! reproducibly from the fetched AMI fixtures.
//!
//! The meeting is [`SPAN_START_S`]..[`SPAN_END_S`] of AMI meeting [`MEETING`] as a call:
//! the mic track is the Project Manager's headset, the reference ("the other side of the
//! call") the sum of the other three headsets over the same span (the rule of
//! `sum_eval::call_channels`). No echo is simulated. Both tracks are levelled to the same
//! loudness while speech is active ([`level`]: peak normalisation left the
//! reference about 7 dB under the mic because one far-end headset clips) with integer
//! arithmetic only, so the 16-bit samples, and through the project's FLAC writer (`earshot_store::FlacTrackWriter`, single-threaded flacenc) the
//! files, are the same on every host. The span was chosen before generating (2026-11-05)
//! as the 150 s window (10 s steps) of ES2004a with the most words from both sides in the
//! AMI word annotations (Project Manager 282, the others 308).
//!
//! Without `--check` the files are written to [`OUT_DIR`] with a NOTICE (the AMI
//! attribution and licence, CC BY 4.0); with it they are generated into a temp folder and
//! compared byte for byte with the committed ones. The SHA-256 of each committed file is
//! pinned in [`MIC_SHA256`] / [`REFERENCE_SHA256`] and checked by
//! `bench_meeting_committed_files_match_their_pins` (no fixtures needed).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::fixtures::{AMI_ANNOTATIONS_DIR, AMI_MEETINGS_XML};
use crate::sum_eval::{ami_headset_roles, call_channels, read_wav_16k_mono};

/// The AMI meeting.
pub const MEETING: &str = "ES2004a";
/// Start of the span (seconds into the meeting).
pub const SPAN_START_S: usize = 570;
/// End of the span (seconds into the meeting).
pub const SPAN_END_S: usize = 720;
/// Sample rate of the AMI headsets and of the output.
pub const RATE: usize = 16_000;
/// The largest peak a levelled track may have: -3 dBFS of 16-bit full scale
/// (`round(32767 * 10^(-3/20))`).
pub const PEAK_TARGET: i64 = 23_197;
/// The loudness both tracks are levelled to while active: RMS at -20 dBFS (`round(32767 / 10)`),
/// lowered for both if a track's peak would pass [`PEAK_TARGET`].
pub const ACTIVE_RMS_TARGET: i64 = 3_277;
/// Frames the active loudness is measured over (30 ms).
const FRAME: usize = 480;
/// A frame is active when its power is at least the loudest frame's divided by this (-30 dB).
const ACTIVE_BELOW_LOUDEST: u128 = 1_000;
/// Where the files go, relative to the repo root.
pub const OUT_DIR: &str = "apps/desktop/src-tauri/resources/benchmark";
/// The mic track's file name.
pub const MIC_FILE: &str = "mic.flac";
/// The reference track's file name.
pub const REFERENCE_FILE: &str = "reference.flac";
/// The attribution file's name.
pub const NOTICE_FILE: &str = "NOTICE.md";
/// SHA-256 of the committed mic track.
pub const MIC_SHA256: &str = "b4a3c214c33e1424dfd0e93d28f416976e7083a7603f96a22df2eb2fd80ffaea";
/// SHA-256 of the committed reference track.
pub const REFERENCE_SHA256: &str =
    "0f3963e42b08473c4a4b4ed6b94e098629a2cc1224b8af156eefebd2fae96c26";

/// The attribution written beside the audio.
#[must_use]
pub fn notice() -> String {
    format!(
        "# Benchmark meeting\n\
         \n\
         The two tracks in this folder are the benchmark meeting of Settings > Benchmark \
         (T7.4c), made by `cargo xtask bench-meeting` from the AMI Meeting Corpus.\n\
         \n\
         - Source: AMI Meeting Corpus, meeting {MEETING}, from {SPAN_START_S} s to \
         {SPAN_END_S} s (individual headset recordings `{MEETING}.Headset-0.wav` to \
         `{MEETING}.Headset-3.wav`), https://groups.inf.ed.ac.uk/ami/corpus/\n\
         - Authors: J. Carletta et al., \"The AMI Meeting Corpus: A Pre-announcement\", \
         MLMI 2005; University of Edinburgh and the AMI Consortium.\n\
         - Licence: Creative Commons Attribution 4.0 International (CC BY 4.0), \
         https://creativecommons.org/licenses/by/4.0/ (corpus licence: \
         https://groups.inf.ed.ac.uk/ami/corpus/license.shtml).\n\
         - Changes: cut to the span above; `{MIC_FILE}` is the Project Manager's headset, \
         `{REFERENCE_FILE}` the sum of the other three headsets; both tracks levelled to the \
         same loudness while speech is active and stored as 16 kHz 16-bit mono FLAC.\n"
    )
}

/// The RMS of `samples` over its active frames (at most [`ACTIVE_BELOW_LOUDEST`] below the
/// loudest 30 ms frame), so pauses do not lower it; 0 for silence.
#[must_use]
pub fn active_rms(samples: &[i64]) -> i64 {
    let power = |frame: &[i64]| -> u128 {
        frame
            .iter()
            .map(|&s| u128::from(s.unsigned_abs()).pow(2))
            .sum()
    };
    // Only whole frames are compared: a short last frame has less power by its length alone.
    let (frames, _) = samples.as_chunks::<FRAME>();
    let loudest = frames.iter().map(|f| power(f)).max().unwrap_or(0);
    if loudest == 0 {
        return 0;
    }
    let active: Vec<u128> = frames
        .iter()
        .map(|f| power(f))
        .filter(|&p| p * ACTIVE_BELOW_LOUDEST >= loudest)
        .collect();
    let mean = active.iter().sum::<u128>() / (active.len() * FRAME) as u128;
    i64::try_from(mean.isqrt()).unwrap_or(i64::MAX)
}

/// `samples` scaled by `num / den`, rounded half away from zero.
fn scale(samples: &[i64], num: i64, den: i64) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| {
            let q = (i128::from(s.abs()) * i128::from(num) * 2 + i128::from(den))
                / (i128::from(den) * 2);
            let v = if s < 0 { -q } else { q };
            i16::try_from(v).unwrap_or(if v < 0 { i16::MIN } else { i16::MAX })
        })
        .collect()
}

/// Both tracks scaled to the same active loudness: [`ACTIVE_RMS_TARGET`], or less when a
/// track's peak would otherwise pass [`PEAK_TARGET`] (nothing clips). Silence stays silence.
#[must_use]
pub fn level(a: &[i64], b: &[i64]) -> (Vec<i16>, Vec<i16>) {
    let peak = |t: &[i64]| t.iter().map(|s| s.abs()).max().unwrap_or(0);
    let mut target = ACTIVE_RMS_TARGET;
    for t in [a, b] {
        if active_rms(t) > 0 {
            target = target.min(PEAK_TARGET * active_rms(t) / peak(t));
        }
    }
    let one = |t: &[i64]| match active_rms(t) {
        0 => vec![0; t.len()],
        rms => scale(t, target, rms),
    };
    (one(a), one(b))
}

/// The two tracks (mic, reference) from the meeting's headsets `headset(channel)` and the
/// AMI `meetings.xml` text.
///
/// # Errors
/// The roles cannot be read, a headset is shorter than the span.
pub fn tracks(
    meetings_xml: &str,
    headset: &mut dyn FnMut(u8) -> Result<Vec<i16>>,
) -> Result<(Vec<i16>, Vec<i16>)> {
    let (mic, far) = call_channels(&ami_headset_roles(meetings_xml, MEETING)?)?;
    let (start, end) = (SPAN_START_S * RATE, SPAN_END_S * RATE);
    let span = |c: u8, h: &mut dyn FnMut(u8) -> Result<Vec<i16>>| -> Result<Vec<i16>> {
        let all = h(c)?;
        ensure!(
            all.len() >= end,
            "headset {c} has {} samples, the span ends at {end}",
            all.len()
        );
        Ok(all[start..end].to_vec())
    };
    let mic_track: Vec<i64> = span(mic, headset)?.into_iter().map(i64::from).collect();
    let mut sum = vec![0i64; end - start];
    for c in far {
        for (s, x) in sum.iter_mut().zip(span(c, headset)?) {
            *s += i64::from(x);
        }
    }
    Ok(level(&mic_track, &sum))
}

/// Writes `samples` as FLAC at `path` with the project's writer.
fn write_flac(path: &Path, samples: &[i16]) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    }
    let mut w = earshot_store::FlacTrackWriter::create(path, RATE as u32)
        .with_context(|| format!("create {}", path.display()))?;
    let audio: Vec<f32> = samples
        .iter()
        .map(|&s| earshot_core::audio::i16_to_f32(s))
        .collect();
    w.write(&audio).context("encode FLAC")?;
    w.finish().context("finish FLAC")?;
    Ok(())
}

/// SHA-256 of `bytes` as lower-case hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Generates the files into `dir` from the fixture cache `fixtures`; returns the SHA-256 of
/// (mic, reference).
///
/// # Errors
/// Missing fixtures (with the fetch command), I/O and encoding errors.
pub fn generate(fixtures: &Path, dir: &Path) -> Result<(String, String)> {
    let fetch = "run `cargo xtask fetch-fixtures --set ami`";
    let xml_path = fixtures.join(AMI_ANNOTATIONS_DIR).join(AMI_MEETINGS_XML);
    let xml: String = std::fs::read(&xml_path)
        .with_context(|| format!("{}: {fetch}", xml_path.display()))?
        .into_iter()
        .map(char::from)
        .collect();
    let mut headset = |c: u8| -> Result<Vec<i16>> {
        let p = fixtures.join(format!("ami/audio/{MEETING}.Headset-{c}.wav"));
        let bytes = std::fs::read(&p).with_context(|| format!("{}: {fetch}", p.display()))?;
        read_wav_16k_mono(&bytes).with_context(|| p.display().to_string())
    };
    let (mic, reference) = tracks(&xml, &mut headset)?;
    for (name, t) in [(MIC_FILE, &mic), (REFERENCE_FILE, &reference)] {
        let wide: Vec<i64> = t.iter().map(|&s| i64::from(s)).collect();
        let peak = wide.iter().map(|s| s.abs()).max().unwrap_or(0);
        println!("  {name}: active RMS {}, peak {peak}", active_rms(&wide));
    }
    std::fs::create_dir_all(dir)?;
    write_flac(&dir.join(MIC_FILE), &mic)?;
    write_flac(&dir.join(REFERENCE_FILE), &reference)?;
    std::fs::write(dir.join(NOTICE_FILE), notice())?;
    Ok((
        sha256_hex(&std::fs::read(dir.join(MIC_FILE))?),
        sha256_hex(&std::fs::read(dir.join(REFERENCE_FILE))?),
    ))
}

/// `cargo xtask bench-meeting [--check]` (module docs).
///
/// # Errors
/// Missing fixtures; with `check`, any difference from the committed files.
pub fn run(root: &Path, check: bool) -> Result<()> {
    let fixtures = crate::fixtures::cache_dir()?;
    let out = root.join(OUT_DIR);
    if !check {
        let (mic, reference) = generate(&fixtures, &out)?;
        println!("bench-meeting: wrote {}", out.display());
        println!("  {MIC_FILE} sha256 {mic}");
        println!("  {REFERENCE_FILE} sha256 {reference}");
        return Ok(());
    }
    let tmp: PathBuf = root
        .join("target")
        .join(format!("bench-meeting-check-{}", std::process::id()));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    let result = generate(&fixtures, &tmp).and_then(|_| {
        for name in [MIC_FILE, REFERENCE_FILE, NOTICE_FILE] {
            let fresh = std::fs::read(tmp.join(name))?;
            let committed = std::fs::read(out.join(name))
                .with_context(|| format!("{} is not committed", out.join(name).display()))?;
            if fresh != committed {
                bail!("{name} differs from a fresh generation; run `cargo xtask bench-meeting`");
            }
        }
        Ok(())
    });
    let _ = std::fs::remove_dir_all(&tmp);
    result?;
    println!("bench-meeting: the committed files equal a fresh generation");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Speech-like test audio: a square wave of `amp` for `active` frames, then `quiet` frames
    /// of silence.
    fn talk(amp: i64, active: usize, quiet: usize) -> Vec<i64> {
        let mut v: Vec<i64> = (0..active * FRAME)
            .map(|i| if i % 2 == 0 { amp } else { -amp })
            .collect();
        v.resize((active + quiet) * FRAME, 0);
        v
    }

    fn as_i64(t: &[i16]) -> Vec<i64> {
        t.iter().map(|&s| i64::from(s)).collect()
    }

    #[test]
    fn bench_meeting_active_loudness_ignores_pauses_and_quiet_frames() {
        assert_eq!(active_rms(&talk(1000, 10, 0)), 1000);
        // Pauses do not lower it.
        assert_eq!(active_rms(&talk(1000, 10, 90)), 1000);
        // Frames more than 30 dB under the loudest are not active (20 is -34 dB).
        let mut mixed = talk(1000, 10, 0);
        mixed.extend(talk(20, 90, 0));
        assert_eq!(active_rms(&mixed), 1000);
        assert_eq!(active_rms(&[0; 4800]), 0);
        assert_eq!(active_rms(&[]), 0);
    }

    #[test]
    fn bench_meeting_level_gives_both_tracks_the_same_loudness() {
        // A loud talker with long pauses and a quiet one without any.
        let (a, b) = level(&talk(8000, 10, 90), &talk(500, 100, 0));
        assert_eq!(active_rms(&as_i64(&a)), ACTIVE_RMS_TARGET);
        assert_eq!(active_rms(&as_i64(&b)), ACTIVE_RMS_TARGET);
        // Pauses stay silent.
        assert!(a[10 * FRAME..].iter().all(|&s| s == 0));
    }

    #[test]
    fn bench_meeting_level_lowers_both_tracks_rather_than_clip() {
        // One click 40 times the track's level would pass the peak limit at the target.
        let mut clicky = talk(1000, 100, 0);
        clicky[5] = 40_000;
        let plain = talk(10_000, 100, 0);
        let (a, b) = level(&clicky, &plain);
        let peak = a.iter().map(|s| i64::from(*s).abs()).max().unwrap();
        assert!(peak <= PEAK_TARGET, "{peak}");
        assert!(peak > PEAK_TARGET * 95 / 100, "{peak}");
        let (ra, rb) = (active_rms(&as_i64(&a)), active_rms(&as_i64(&b)));
        assert!(ra < ACTIVE_RMS_TARGET, "{ra}");
        assert!((ra - rb).abs() <= 1, "{ra} {rb}");
    }

    #[test]
    fn bench_meeting_level_rounds_symmetrically_and_keeps_silence() {
        assert_eq!(scale(&[3, -3, 1, -1], 1, 2), vec![2, -2, 1, -1]);
        let (a, b) = level(&[0; 960], &talk(1000, 2, 0));
        assert!(a.iter().all(|&s| s == 0));
        assert_eq!(active_rms(&as_i64(&b)), ACTIVE_RMS_TARGET);
    }

    #[test]
    fn bench_meeting_tracks_are_the_pm_against_the_sum_of_the_others() {
        // ES2004a's roles: the Project Manager is on headset 1.
        let xml = r#"<meeting nite:id="m" observation="ES2004a">
            <speaker channel="1" role="PM"/><speaker channel="2" role="ID"/>
            <speaker channel="0" role="UI"/><speaker channel="3" role="ME"/></meeting>"#;
        let len = SPAN_END_S * RATE;
        let mut headset = |c: u8| -> Result<Vec<i16>> {
            let mut v = vec![0i16; len];
            // One marker sample per headset inside the span, at its own position.
            v[SPAN_START_S * RATE + usize::from(c)] = 1000 * (i16::from(c) + 1);
            Ok(v)
        };
        let (mic, reference) = tracks(xml, &mut headset).unwrap();
        assert_eq!(mic.len(), (SPAN_END_S - SPAN_START_S) * RATE);
        assert_eq!(reference.len(), mic.len());
        assert!(
            mic[1] != 0 && mic[0] == 0 && mic[2] == 0 && mic[3] == 0,
            "headset 1 alone: {:?}",
            &mic[..4]
        );
        assert_eq!(reference[1], 0, "headset 1 is not in the reference");
        // Headsets 0, 2, 3 (1000, 3000, 4000) summed, their ratios kept.
        assert!(reference[0] > 0, "{:?}", &reference[..4]);
        assert!((i64::from(reference[2]) - 3 * i64::from(reference[0])).abs() <= 3);
        assert!((i64::from(reference[3]) - 4 * i64::from(reference[0])).abs() <= 4);
        let mut short = |_: u8| -> Result<Vec<i16>> { Ok(vec![0; len - 1]) };
        assert!(tracks(xml, &mut short).is_err());
    }

    #[test]
    fn bench_meeting_committed_files_match_their_pins() {
        let dir = crate::repo_root().join(OUT_DIR);
        for (name, pin) in [(MIC_FILE, MIC_SHA256), (REFERENCE_FILE, REFERENCE_SHA256)] {
            let bytes = std::fs::read(dir.join(name)).unwrap();
            assert_eq!(sha256_hex(&bytes), pin, "{name}");
            let info = crate::flac::parse(&bytes).unwrap();
            assert_eq!(
                (info.sample_rate, info.channels, info.bits_per_sample),
                (16_000, 1, 16),
                "{name}"
            );
            assert_eq!(
                info.total_samples,
                ((SPAN_END_S - SPAN_START_S) * RATE) as u64,
                "{name}"
            );
        }
        let notice = std::fs::read_to_string(dir.join(NOTICE_FILE)).unwrap();
        assert_eq!(notice, super::notice());
        let total: u64 = [MIC_FILE, REFERENCE_FILE, NOTICE_FILE]
            .iter()
            .map(|n| std::fs::metadata(dir.join(n)).unwrap().len())
            .sum();
        assert!(total < 5_000_000, "{total} bytes");
    }
}
