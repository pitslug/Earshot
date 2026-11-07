//! The two-track meetings `cargo xtask t613` replays (T6.13), generated into the fixture
//! cache (`<EARSHOT_FIXTURES>/generated/t613/<name>/`: `mic.wav`, `loopback.wav`,
//! `truth.json`), never committed.
//!
//! - **LibriSpeech** (`librispeech-<N>min`, the timing meeting and a quality input): a call
//!   of `N` minutes between "Me" and several remote speakers taking turns, built from whole
//!   LibriSpeech test-clean utterances (public domain audiobooks, CC BY 4.0 corpus), so the
//!   reference words and speaker turns of each track are exact and no audio is used twice
//!   (tiled audio inflates speaker counts, private notes). Speakers are test-clean's
//!   in order of total audio (the smoke speakers 61 and 121 left out): the first is "Me",
//!   then as many remote speakers (at least two) as it takes to fill the meeting. The turn
//!   schedule ([`schedule`]) is seeded: turns of one to three utterances, a speaker other
//!   than the last one picked in proportion to their remaining audio, 0.3-2.0 s between
//!   turns, a 5-15 s pause every 12th turn, and one turn change in ten between "Me" and the
//!   far end overlapping by 0.3-1.0 s (double talk). Each utterance is scaled to the same
//!   RMS, as the call simulator does.
//! - **AMI** (`ami-<meeting>`, quality only): a pinned AMI meeting as a call. "Me" is the
//!   participant on headset 0; the far end is the sum of the other three headsets. Each
//!   headset is kept only inside its wearer's annotated segments (padded by 0.2 s, 20 ms
//!   fades), which removes the headset crosstalk (whole headsets score 223% WER, T2.5b);
//!   overlapping speech stays. References: the AMI words of the wearer(s), read with the
//!   `asr_eval` rule (`refs::parse_ami_words`), and the segments as speaker turns. The far
//!   track's reference merges the three headsets' words by start time and is not clipped to
//!   the UEM, and each track is aligned whole: its WER is for comparing the two cleanups,
//!   not comparable with `asr.wer_ami_ihm` (per segment, per headset, UEM-clipped).
//!
//! Both are then rendered through the call simulator (`earshot_core::sim`) in the room and
//! with the levels of `acceptance/m6_meeting.toml` (laptop speakers 5 dB below the user, 90
//! ms output latency, loudspeaker soft clip 3, room noise 35 dB down), with the composed far
//! end as the loopback and its echo in the mic. The simulator takes each track as a single
//! clip, so its levels are relative to the whole track's RMS; [`whole_track_levels`] turns
//! the m6 speech-level settings into whole-track ones from each track's speech duty cycle.
//! Clock drift is 0 ppm: the replay delivers exact timestamps, and drift handling is not
//! what T6.13 measures.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use earshot_core::audio::{PIPELINE_RATE, WavFormat, read_wav, write_wav};
use earshot_core::sim::{Scenario, Setup, SpeechPool, simulate};
use rand_chacha::ChaCha8Rng;
use rand_core::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};

use super::ami_refs;
use crate::fixtures;

/// Changes whenever the generated audio or truth would change; a cached meeting with
/// another recipe is generated again.
pub const RECIPE: &str = "t613-meeting-1";

/// Seed of the LibriSpeech turn schedule.
pub const SCHEDULE_SEED: u64 = 6_130;

/// Seed of the simulator's RNG (noise).
pub const SIM_SEED: u64 = 613;

/// LibriSpeech speakers never used here: the smoke speakers (m6 and the model check clip).
pub const EXCLUDED_SPEAKERS: [u32; 2] = [61, 121];

/// The pipeline rate in Hz.
const RATE: f64 = 16_000.0;

/// Fewest remote speakers in a LibriSpeech meeting.
pub const MIN_REMOTE: usize = 2;

/// One speaker turn of the reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefTurn {
    /// Speaker label.
    pub speaker: String,
    /// Start (s).
    pub start_s: f64,
    /// End (s).
    pub end_s: f64,
}

/// One reference word (the start of its utterance where the corpus has no word times).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefWord {
    /// Start (s).
    pub start_s: f64,
    /// The word.
    pub text: String,
}

/// The reference of one track.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackTruth {
    /// Speaker turns.
    pub turns: Vec<RefTurn>,
    /// Reference words.
    pub words: Vec<RefWord>,
}

impl TrackTruth {
    /// The reference text: the words in order of their start (ties keep their order).
    #[must_use]
    pub fn text(&self) -> String {
        let mut w: Vec<&RefWord> = self.words.iter().collect();
        w.sort_by(|a, b| a.start_s.total_cmp(&b.start_s));
        w.iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Seconds of speech (turns summed).
    #[must_use]
    pub fn speech_s(&self) -> f64 {
        self.turns.iter().map(|t| t.end_s - t.start_s).sum()
    }
}

/// What a generated meeting holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Truth {
    /// [`RECIPE`] when generated.
    pub recipe: String,
    /// Meeting name (the directory).
    pub name: String,
    /// Where the speech comes from.
    pub source: String,
    /// Length (s).
    pub duration_s: f64,
    /// The mic ("Me").
    pub mic: TrackTruth,
    /// The far end (loopback).
    pub reference: TrackTruth,
    /// The simulator settings used (whole-track levels).
    pub scenario: serde_json::Value,
}

/// A generated meeting on disk.
#[derive(Debug, Clone)]
pub struct Meeting {
    /// Its directory.
    pub dir: PathBuf,
    /// The truth.
    pub truth: Truth,
}

impl Meeting {
    /// The mic recording.
    #[must_use]
    pub fn mic(&self) -> PathBuf {
        self.dir.join("mic.wav")
    }

    /// The loopback recording.
    #[must_use]
    pub fn loopback(&self) -> PathBuf {
        self.dir.join("loopback.wav")
    }
}

/// One utterance a schedule can place.
#[derive(Debug, Clone, PartialEq)]
pub struct Utt {
    /// Corpus id.
    pub id: String,
    /// Length (s).
    pub seconds: f64,
}

/// A placed utterance: speaker index (0 = "Me"), utterance index within that speaker, start
/// (s).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    /// Speaker index (0 is "Me").
    pub speaker: usize,
    /// Utterance index within the speaker.
    pub utt: usize,
    /// Start (s).
    pub start_s: f64,
}

fn uniform(rng: &mut ChaCha8Rng) -> f64 {
    (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

/// The turn schedule of a meeting of `duration_s` (module docs) over `speakers` (index 0 is
/// "Me", each speaker's utterances in corpus order and used at most once). Stops before the
/// first utterance that would end within 0.5 s of the end, or when the speech runs out.
#[must_use]
pub fn schedule(speakers: &[Vec<Utt>], duration_s: f64, seed: u64) -> Vec<Placed> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut next = vec![0usize; speakers.len()];
    let remaining = |s: usize, next: &[usize]| -> f64 {
        speakers[s][next[s]..].iter().map(|u| u.seconds).sum()
    };
    let mut placed = Vec::new();
    let mut t = 0.5;
    let mut prev: Option<usize> = None;
    let mut turns = 0usize;
    loop {
        let mut candidates: Vec<usize> = (0..speakers.len())
            .filter(|&s| next[s] < speakers[s].len() && Some(s) != prev)
            .collect();
        if candidates.is_empty() {
            candidates = prev
                .filter(|&p| next[p] < speakers[p].len())
                .into_iter()
                .collect();
        }
        if candidates.is_empty() {
            return placed;
        }
        let weights: Vec<f64> = candidates.iter().map(|&s| remaining(s, &next)).collect();
        let total: f64 = weights.iter().sum();
        let mut r = uniform(&mut rng) * total;
        let mut s = *candidates.last().unwrap_or(&0);
        for (&c, w) in candidates.iter().zip(&weights) {
            if r < *w {
                s = c;
                break;
            }
            r -= w;
        }
        if let Some(p) = prev {
            let crosses = (p == 0) != (s == 0);
            if crosses && uniform(&mut rng) < 0.1 {
                t -= 0.3 + 0.7 * uniform(&mut rng);
            } else {
                t += 0.3 + 1.7 * uniform(&mut rng);
            }
            turns += 1;
            if turns.is_multiple_of(12) {
                t += 5.0 + 10.0 * uniform(&mut rng);
            }
        }
        let n = 1 + (rng.next_u64() % 3) as usize;
        for k in 0..n {
            let Some(u) = speakers[s].get(next[s]) else {
                break;
            };
            if t + u.seconds > duration_s - 0.5 {
                return placed;
            }
            placed.push(Placed {
                speaker: s,
                utt: next[s],
                start_s: t,
            });
            t += u.seconds;
            next[s] += 1;
            if k + 1 < n {
                t += 0.15 + 0.35 * uniform(&mut rng);
            }
        }
        prev = Some(s);
    }
}

/// The end of the last placed utterance (s).
#[must_use]
pub fn schedule_end(speakers: &[Vec<Utt>], placed: &[Placed]) -> f64 {
    placed
        .iter()
        .map(|p| p.start_s + speakers[p.speaker][p.utt].seconds)
        .fold(0.0, f64::max)
}

/// Whole-track simulator settings for a meeting whose far end speaks `duty_far` and whose
/// near end speaks `duty_near` of the time (each in (0, 1]), from `base`'s speech-level
/// settings (module docs): the simulator measures echo, near-end and soft-clip levels over
/// the whole single-clip track, so a level meant for speech is shifted by its track's duty
/// cycle. Returns `(echo_level_dbfs, ser_db, softclip, noise_snr_db)`.
#[must_use]
pub fn whole_track_levels(
    base: &Scenario,
    duty_far: f64,
    duty_near: f64,
) -> (f64, f64, Option<f64>, Option<f64>) {
    let db = |x: f64| 10.0 * x.log10();
    (
        base.echo_level_dbfs + db(duty_far),
        base.ser_db + db(duty_near) - db(duty_far),
        base.softclip.map(|c| c / duty_far.sqrt()),
        base.noise_snr_db.map(|s| s + db(duty_near)),
    )
}

/// Fraction of `n` samples inside `spans` (merged).
#[must_use]
pub fn duty(spans: &[(usize, usize)], n: usize) -> f64 {
    let mut s: Vec<(usize, usize)> = spans.iter().map(|&(a, b)| (a.min(n), b.min(n))).collect();
    s.sort_unstable();
    let mut covered = 0usize;
    let mut end = 0usize;
    for (a, b) in s {
        let a = a.max(end);
        if b > a {
            covered += b - a;
            end = b;
        }
    }
    covered as f64 / n.max(1) as f64
}

fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
}

/// The simulated call: `far` (loopback) and its echo plus `near` through the m6 room in
/// the mic, with the m6 levels made whole-track (`duty_*` from the speech spans).
///
/// # Errors
/// An invalid scenario.
pub fn render(
    name: &str,
    setup: &Setup,
    base: &Scenario,
    far: Vec<f32>,
    near: Vec<f32>,
    (duty_far, duty_near): (f64, f64),
) -> Result<(Vec<f32>, Vec<f32>, Scenario)> {
    ensure!(far.len() == near.len(), "tracks of different length");
    ensure!(
        duty_far > 0.0 && duty_near > 0.0,
        "{name}: both tracks need speech"
    );
    let (echo_level_dbfs, ser_db, softclip, noise_snr_db) =
        whole_track_levels(base, duty_far, duty_near);
    let scn = Scenario {
        name: name.to_owned(),
        description: format!(
            "{name}: m6 room and levels, whole-track (duty far {duty_far:.3}, near {duty_near:.3})"
        ),
        informational: false,
        duration_s: far.len() as f64 / RATE,
        seed: SIM_SEED,
        far_speech: true,
        near_speech: true,
        near_start_s: 0.0,
        echo: true,
        echo_from_s: None,
        echo_until_s: None,
        delay_ms: base.delay_ms,
        delay_jump: None,
        drift_ppm: 0.0,
        softclip,
        echo_level_dbfs,
        ser_db,
        noise_snr_db,
        path_change_s: None,
        near_silent_s: Vec::new(),
        stereo: None,
    };
    let (far, near) = (vec![far], vec![near]);
    let pool = SpeechPool {
        far: &far,
        near: &near,
    };
    let mut rng = ChaCha8Rng::seed_from_u64(SIM_SEED);
    let out = simulate(&scn, setup, &pool, &mut rng).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((out.mic, out.reference, scn))
}

fn write_mono(path: &Path, samples: &[f32]) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    write_wav(
        std::io::BufWriter::new(file),
        PIPELINE_RATE,
        1,
        samples,
        WavFormat::Float32,
    )
    .with_context(|| format!("cannot write {}", path.display()))
}

/// The directory of generated meeting `name` in the fixture cache.
///
/// # Errors
/// No fixture cache directory.
pub fn meeting_dir(name: &str) -> Result<PathBuf> {
    Ok(fixtures::cache_dir()?
        .join("generated")
        .join("t613")
        .join(name))
}

/// The cached meeting `name` when its recipe is current.
fn cached(dir: &Path) -> Option<Meeting> {
    let truth: Truth = serde_json::from_slice(&std::fs::read(dir.join("truth.json")).ok()?).ok()?;
    (truth.recipe == RECIPE && dir.join("mic.wav").is_file() && dir.join("loopback.wav").is_file())
        .then(|| Meeting {
            dir: dir.to_owned(),
            truth,
        })
}

fn store(dir: &Path, mic: &[f32], loopback: &[f32], truth: Truth) -> Result<Meeting> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    // truth.json last: a meeting is complete only once it exists.
    let _ = std::fs::remove_file(dir.join("truth.json"));
    write_mono(&dir.join("mic.wav"), mic)?;
    write_mono(&dir.join("loopback.wav"), loopback)?;
    std::fs::write(dir.join("truth.json"), serde_json::to_vec_pretty(&truth)?)
        .with_context(|| format!("cannot write {}", dir.display()))?;
    Ok(Meeting {
        dir: dir.to_owned(),
        truth,
    })
}

/// LibriSpeech transcripts of one chapter directory (`<spk>-<chap>.trans.txt`).
fn chapter_texts(dir: &Path) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).with_context(|| format!("cannot list {}", dir.display()))? {
        let p = e?.path();
        if p.to_string_lossy().ends_with(".trans.txt") {
            let text = std::fs::read_to_string(&p)?;
            for l in text.lines() {
                if let Some((id, words)) = l.split_once(' ') {
                    out.push((id.to_owned(), words.to_owned()));
                }
            }
        }
    }
    Ok(out)
}

/// test-clean speakers by total audio, most first (ties by id), without
/// [`EXCLUDED_SPEAKERS`]: `(speaker, utterances in corpus order)`.
fn ranked_speakers(corpus: &[fixtures::Utterance]) -> Vec<(u32, Vec<&fixtures::Utterance>)> {
    let mut by: std::collections::BTreeMap<u32, Vec<&fixtures::Utterance>> = Default::default();
    for u in corpus {
        if !EXCLUDED_SPEAKERS.contains(&u.speaker) {
            by.entry(u.speaker).or_default().push(u);
        }
    }
    let mut v: Vec<(u32, Vec<&fixtures::Utterance>)> = by.into_iter().collect();
    let total = |u: &[&fixtures::Utterance]| u.iter().map(|u| u.seconds).sum::<f64>();
    v.sort_by(|a, b| total(&b.1).total_cmp(&total(&a.1)).then(a.0.cmp(&b.0)));
    v
}

/// Generates (or takes from the cache) the LibriSpeech meeting of `minutes`.
///
/// # Errors
/// Missing LibriSpeech test-clean (run `cargo xtask fetch-fixtures --set librispeech`), not
/// enough speech, or write errors.
pub fn librispeech(root: &Path, minutes: u32, regenerate: bool) -> Result<Meeting> {
    let name = format!("librispeech-{minutes}min");
    let dir = meeting_dir(&name)?;
    if !regenerate && let Some(m) = cached(&dir) {
        return Ok(m);
    }
    let cache = fixtures::cache_dir()?;
    let split = "test-clean";
    let corpus = fixtures::scan_split(&cache, split).with_context(
        || "LibriSpeech test-clean is missing: run `cargo xtask fetch-fixtures --set librispeech`",
    )?;
    let ranked = ranked_speakers(&corpus);
    let duration_s = f64::from(minutes) * 60.0;
    let to_utts = |u: &[&fixtures::Utterance]| -> Vec<Utt> {
        u.iter()
            .map(|u| Utt {
                id: u.id(),
                seconds: u.seconds,
            })
            .collect()
    };
    // As many remote speakers as it takes to fill the meeting.
    let mut remote = MIN_REMOTE;
    let (speakers, placed) = loop {
        ensure!(
            remote < ranked.len(),
            "test-clean has too few speakers for {minutes} min"
        );
        let speakers: Vec<Vec<Utt>> = ranked[..=remote].iter().map(|(_, u)| to_utts(u)).collect();
        let placed = schedule(&speakers, duration_s, SCHEDULE_SEED);
        if schedule_end(&speakers, &placed) >= duration_s - 60.0_f64.min(duration_s / 10.0) {
            break (speakers, placed);
        }
        remote += 1;
    };
    eprintln!(
        "t613: {name}: Me = LibriSpeech speaker {}, remote {:?}, {} utterances",
        ranked[0].0,
        ranked[1..=remote].iter().map(|r| r.0).collect::<Vec<_>>(),
        placed.len()
    );
    let n = (duration_s * RATE).round() as usize;
    let (mut near, mut far) = (vec![0.0f32; n], vec![0.0f32; n]);
    let (mut mic_t, mut ref_t) = (TrackTruth::default(), TrackTruth::default());
    let (mut near_spans, mut far_spans) = (Vec::new(), Vec::new());
    let mut texts: std::collections::BTreeMap<String, String> = Default::default();
    for p in &placed {
        let u = &speakers[p.speaker][p.utt];
        let (spk, chap) = {
            let mut it = u.id.split('-');
            (
                it.next().unwrap_or_default().to_owned(),
                it.next().unwrap_or_default().to_owned(),
            )
        };
        if !texts.contains_key(&u.id) {
            let chapter = fixtures::split_dir(&cache, split).join(&spk).join(&chap);
            texts.extend(chapter_texts(&chapter)?);
        }
        let text = texts
            .get(&u.id)
            .with_context(|| format!("no transcript for {}", u.id))?
            .clone();
        let audio = crate::echo_nl::decode_flac(&fixtures::utterance_path(&cache, split, &u.id)?)?;
        let g = {
            let r = rms(&audio);
            if r > 0.0 { (0.1 / r) as f32 } else { 0.0 }
        };
        let start = (p.start_s * RATE).round() as usize;
        let end = (start + audio.len()).min(n);
        let (dst, spans, truth) = if p.speaker == 0 {
            (&mut near, &mut near_spans, &mut mic_t)
        } else {
            (&mut far, &mut far_spans, &mut ref_t)
        };
        for (d, s) in dst[start..end].iter_mut().zip(&audio) {
            *d += s * g;
        }
        spans.push((start, end));
        let label = if p.speaker == 0 {
            format!("me-{spk}")
        } else {
            format!("remote-{spk}")
        };
        truth.turns.push(RefTurn {
            speaker: label,
            start_s: start as f64 / RATE,
            end_s: end as f64 / RATE,
        });
        truth.words.extend(text.split_whitespace().map(|w| RefWord {
            start_s: start as f64 / RATE,
            text: w.to_owned(),
        }));
    }
    let file = crate::accept::m6::meeting_file(root)?;
    let (mic, loopback, scn) = render(
        &name,
        &file.setup,
        &file.meeting,
        far,
        near,
        (duty(&far_spans, n), duty(&near_spans, n)),
    )?;
    let truth = Truth {
        recipe: RECIPE.into(),
        name: name.clone(),
        source: format!(
            "LibriSpeech {split} (https://www.openslr.org/12, CC BY 4.0, LibriVox public domain); \
             Me = speaker {}, remote = {:?}",
            ranked[0].0,
            ranked[1..=remote].iter().map(|r| r.0).collect::<Vec<_>>()
        ),
        duration_s,
        mic: mic_t,
        reference: ref_t,
        scenario: scenario_json(&scn),
    };
    store(&dir, &mic, &loopback, truth)
}

fn scenario_json(scn: &Scenario) -> serde_json::Value {
    serde_json::json!({
        "delay_ms": scn.delay_ms,
        "drift_ppm": scn.drift_ppm,
        "softclip": scn.softclip,
        "echo_level_dbfs": scn.echo_level_dbfs,
        "ser_db": scn.ser_db,
        "noise_snr_db": scn.noise_snr_db,
        "seed": scn.seed,
        "description": scn.description,
    })
}

/// `spans` (s) padded by `pad` s, merged, as sample ranges within `n`.
fn padded(spans: &[(f64, f64)], pad: f64, n: usize) -> Vec<(usize, usize)> {
    let mut v: Vec<(usize, usize)> = spans
        .iter()
        .map(|&(a, b)| {
            (
                (((a - pad).max(0.0)) * RATE) as usize,
                (((b + pad) * RATE) as usize).min(n),
            )
        })
        .filter(|(a, b)| a < b)
        .collect();
    v.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// A gain mask: 1 inside `spans`, 0 outside, with `fade` sample linear ramps inside each
/// span's edges.
#[must_use]
pub fn mask(spans: &[(usize, usize)], n: usize, fade: usize) -> Vec<f32> {
    let mut m = vec![0.0f32; n];
    for &(a, b) in spans {
        let len = b - a;
        let f = fade.min(len / 2).max(1);
        for (i, v) in m[a..b].iter_mut().enumerate() {
            let edge = i.min(len - 1 - i);
            *v = v.max(if edge >= f {
                1.0
            } else {
                (edge + 1) as f32 / f as f32
            });
        }
    }
    m
}

/// One AMI headset: audio, its wearer's segments (s), words and letter.
type Headset = (Vec<f32>, Vec<(f64, f64)>, Vec<ami_refs::AmiWord>, String);

/// Generates (or takes from the cache) AMI meeting `meeting` as a call (module docs).
///
/// # Errors
/// Missing AMI fixtures (run `cargo xtask fetch-fixtures --set ami`), malformed
/// annotations, or write errors.
pub fn ami(root: &Path, meeting: &str, regenerate: bool) -> Result<Meeting> {
    let name = format!("ami-{meeting}");
    let dir = meeting_dir(&name)?;
    if !regenerate && let Some(m) = cached(&dir) {
        return Ok(m);
    }
    let cache = fixtures::cache_dir()?;
    let ann = cache.join(fixtures::AMI_ANNOTATIONS_DIR);
    let missing = || "AMI fixtures are missing: run `cargo xtask fetch-fixtures --set ami`";
    let xml = std::fs::read(ann.join(fixtures::AMI_MEETINGS_XML)).with_context(missing)?;
    let headsets = fixtures::ami_headset_speakers(&ami_refs::latin1(&xml), meeting)?;
    let mut tracks: Vec<Headset> = Vec::new();
    for (channel, letter) in &headsets {
        let wav = cache.join(format!("ami/audio/{meeting}.Headset-{channel}.wav"));
        let audio = read_wav(std::io::BufReader::new(
            std::fs::File::open(&wav).with_context(missing)?,
        ))
        .with_context(|| format!("cannot read {}", wav.display()))?;
        ensure!(
            audio.rate().hz() == 16_000,
            "{} is not 16 kHz",
            wav.display()
        );
        let seg_xml = std::fs::read(ann.join(format!("segments/{meeting}.{letter}.segments.xml")))
            .with_context(missing)?;
        let (c, spans) = fixtures::ami_segments(&ami_refs::latin1(&seg_xml))?;
        ensure!(
            c == *channel,
            "{meeting}.{letter}: segments on headset {c}, expected {channel}"
        );
        let words_xml = std::fs::read(ann.join(format!("words/{meeting}.{letter}.words.xml")))
            .with_context(missing)?;
        let words = ami_refs::parse_ami_words(&ami_refs::latin1(&words_xml))
            .map_err(|e| anyhow::anyhow!("{meeting}.{letter} words: {e}"))?;
        tracks.push((audio.to_mono(), spans, words, letter.clone()));
    }
    let n = tracks.iter().map(|t| t.0.len()).min().unwrap_or(0);
    ensure!(n > 0, "{meeting}: empty headsets");
    let (mut near, mut far) = (vec![0.0f32; n], vec![0.0f32; n]);
    let (mut mic_t, mut ref_t) = (TrackTruth::default(), TrackTruth::default());
    let (mut near_spans, mut far_spans) = (Vec::new(), Vec::new());
    let fade = (0.02 * RATE) as usize;
    for (k, (audio, spans, words, letter)) in tracks.iter().enumerate() {
        let keep = padded(spans, 0.2, n);
        let m = mask(&keep, n, fade);
        let active: Vec<f32> = keep
            .iter()
            .flat_map(|&(a, b)| audio[a..b].iter().copied())
            .collect();
        let r = rms(&active);
        if r == 0.0 {
            bail!("{meeting}.{letter}: silent headset");
        }
        let g = (0.1 / r) as f32;
        let (dst, all, truth) = if k == 0 {
            (&mut near, &mut near_spans, &mut mic_t)
        } else {
            (&mut far, &mut far_spans, &mut ref_t)
        };
        for ((d, s), w) in dst.iter_mut().zip(&audio[..n]).zip(&m) {
            *d += s * g * w;
        }
        all.extend(keep);
        let label = format!("{meeting}-{letter}");
        truth.turns.extend(spans.iter().map(|&(a, b)| RefTurn {
            speaker: label.clone(),
            start_s: a,
            end_s: b,
        }));
        truth.words.extend(words.iter().map(|w| RefWord {
            start_s: w.start,
            text: w.text.clone(),
        }));
    }
    let file = crate::accept::m6::meeting_file(root)?;
    let (mic, loopback, scn) = render(
        &name,
        &file.setup,
        &file.meeting,
        far,
        near,
        (duty(&far_spans, n), duty(&near_spans, n)),
    )?;
    let truth = Truth {
        recipe: RECIPE.into(),
        name,
        source: format!(
            "AMI Meeting Corpus {meeting} individual headsets (CC BY 4.0); Me = speaker {} on \
             headset 0, far end = the other three headsets",
            tracks[0].3
        ),
        duration_s: n as f64 / RATE,
        mic: mic_t,
        reference: ref_t,
        scenario: scenario_json(&scn),
    };
    store(&dir, &mic, &loopback, truth)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speakers() -> Vec<Vec<Utt>> {
        // "Me" and three remote speakers with 2-12 s utterances.
        (0..4)
            .map(|s| {
                (0..60)
                    .map(|i| Utt {
                        id: format!("{s}-{i}"),
                        seconds: 2.0 + f64::from((i * 7 + s * 3) % 11),
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn t613_schedule_is_seeded_and_fills_the_meeting_without_reuse() {
        let sp = speakers();
        let a = schedule(&sp, 900.0, 7);
        assert_eq!(a, schedule(&sp, 900.0, 7), "same seed, same schedule");
        assert_ne!(a, schedule(&sp, 900.0, 8));
        let end = schedule_end(&sp, &a);
        assert!(end <= 899.5 && end > 850.0, "ends at {end}");
        // Each utterance once, in corpus order per speaker.
        for s in 0..sp.len() {
            let utts: Vec<usize> = a.iter().filter(|p| p.speaker == s).map(|p| p.utt).collect();
            assert_eq!(utts, (0..utts.len()).collect::<Vec<_>>());
        }
        // Everyone speaks; "Me" gets a share in line with the audio.
        let me = a.iter().filter(|p| p.speaker == 0).count() as f64 / a.len() as f64;
        assert!((0.1..0.45).contains(&me), "Me share {me}");
    }

    #[test]
    fn t613_schedule_never_overlaps_a_track_with_itself_and_sometimes_double_talks() {
        let sp = speakers();
        let a = schedule(&sp, 3_600.0, SCHEDULE_SEED);
        let span = |p: &Placed| (p.start_s, p.start_s + sp[p.speaker][p.utt].seconds);
        let mut overlaps = 0;
        for (i, p) in a.iter().enumerate() {
            for q in &a[i + 1..] {
                let (pa, pb) = span(p);
                let (qa, qb) = span(q);
                if pa < qb && qa < pb {
                    assert_ne!(
                        p.speaker == 0,
                        q.speaker == 0,
                        "same-track overlap: {p:?} {q:?}"
                    );
                    overlaps += 1;
                }
            }
        }
        assert!(overlaps > 0, "some double talk between Me and the far end");
        // Long pauses exist (stretches close at pauses).
        let mut sorted: Vec<(f64, f64)> = a.iter().map(span).collect();
        sorted.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut end = 0.0f64;
        let mut long = 0;
        for (s, e) in sorted {
            if s - end >= 5.0 {
                long += 1;
            }
            end = end.max(e);
        }
        assert!(long >= 5, "{long} pauses of 5 s or more");
    }

    #[test]
    fn t613_schedule_stops_when_the_speech_runs_out() {
        let sp: Vec<Vec<Utt>> = (0..2)
            .map(|s| {
                vec![Utt {
                    id: format!("{s}"),
                    seconds: 3.0,
                }]
            })
            .collect();
        let a = schedule(&sp, 600.0, 1);
        assert_eq!(a.len(), 2);
        assert!(schedule_end(&sp, &a) < 10.0);
    }

    #[test]
    fn t613_duty_merges_overlapping_spans() {
        assert_eq!(duty(&[(0, 10), (5, 20), (30, 40)], 100), 0.3);
        assert_eq!(duty(&[], 100), 0.0);
        assert_eq!(duty(&[(90, 200)], 100), 0.1);
    }

    #[test]
    fn t613_mask_is_one_inside_zero_outside_with_ramps() {
        let m = mask(&[(10, 30)], 40, 4);
        assert!(m[..10].iter().all(|&v| v == 0.0));
        assert!(m[30..].iter().all(|&v| v == 0.0));
        assert!(m[14..26].iter().all(|&v| v == 1.0));
        assert!(m[10] > 0.0 && m[10] < 1.0 && m[29] > 0.0 && m[29] < 1.0);
    }

    /// Active RMS (dB) of `x` over `spans` (samples).
    fn active_db(x: &[f32], spans: &[std::ops::Range<usize>]) -> f64 {
        20.0 * earshot_core::sim::active_rms(x, spans).log10()
    }

    #[test]
    fn t613_whole_track_levels_give_the_m6_speech_levels() {
        // A 40 s call from the smoke clips with the near end active about a third of the
        // time and the far end about half: rendered as single whole-track clips, the echo's
        // speech level, the speech SER and the noise level come out as the m6 settings ask
        // for speech.
        let root = crate::repo_root();
        let (far_clips, near_clips) = crate::echo_nl::smoke_speech(&root).unwrap();
        let n = 40 * 16_000;
        let (mut far, mut near) = (vec![0.0f32; n], vec![0.0f32; n]);
        let (mut far_spans, mut near_spans) = (Vec::new(), Vec::new());
        let place =
            |dst: &mut Vec<f32>, spans: &mut Vec<(usize, usize)>, clip: &[f32], at: usize| {
                let g = (0.1 / rms(clip)) as f32;
                let end = (at + clip.len()).min(n);
                for (d, s) in dst[at..end].iter_mut().zip(clip) {
                    *d = s * g;
                }
                spans.push((at, end));
            };
        place(&mut far, &mut far_spans, &far_clips[0], 16_000);
        place(&mut far, &mut far_spans, &far_clips[1], 22 * 16_000);
        place(&mut near, &mut near_spans, &near_clips[0], 9 * 16_000);
        place(&mut near, &mut near_spans, &near_clips[1], 33 * 16_000);
        let file = crate::accept::m6::meeting_file(&root).unwrap();
        let (df, dn) = (duty(&far_spans, n), duty(&near_spans, n));
        assert!(df > 0.2 && dn > 0.1 && df < 0.8 && dn < 0.6, "{df} {dn}");
        let (_, _, scn) = render(
            "levels",
            &file.setup,
            &file.meeting,
            far.clone(),
            near.clone(),
            (df, dn),
        )
        .unwrap();
        // Re-run the simulator to read its components.
        let pool_far = vec![far];
        let pool_near = vec![near];
        let mut rng = ChaCha8Rng::seed_from_u64(SIM_SEED);
        let out = simulate(
            &scn,
            &file.setup,
            &SpeechPool {
                far: &pool_far,
                near: &pool_near,
            },
            &mut rng,
        )
        .unwrap();
        let to_ranges = |s: &[(usize, usize)]| -> Vec<std::ops::Range<usize>> {
            s.iter().map(|&(a, b)| a..b).collect()
        };
        let delay = (file.meeting.delay_ms / 1_000.0 * 16_000.0) as usize;
        let arrived: Vec<std::ops::Range<usize>> = far_spans
            .iter()
            .map(|&(a, b)| a + delay..(b + delay).min(n))
            .collect();
        let echo_db = active_db(&out.echo, &arrived);
        assert!(
            (echo_db - file.meeting.echo_level_dbfs).abs() < 1.0,
            "echo speech level {echo_db:.2} dBFS, m6 {}",
            file.meeting.echo_level_dbfs
        );
        let near_db = active_db(&out.near_clean, &to_ranges(&near_spans));
        let ser = near_db - echo_db;
        assert!(
            (ser - file.meeting.ser_db).abs() < 1.0,
            "speech SER {ser:.2} dB, m6 {}",
            file.meeting.ser_db
        );
        let noise_db = 20.0 * rms(&out.noise).log10();
        let snr = near_db - noise_db;
        let want = file.meeting.noise_snr_db.unwrap();
        assert!(
            (snr - want).abs() < 1.0,
            "noise {snr:.2} dB below, m6 {want}"
        );
    }
}
