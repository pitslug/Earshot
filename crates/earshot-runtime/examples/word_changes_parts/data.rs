//! The data of `docs/far-speakers/word-changes.md` (T6.9a), one copy shared by `word_changes`
//! and `edge_words` (T12.88): the LibriSpeech dev-clean two-speaker scenes and the AMI dev
//! words. Each including example defines `fail` and includes `refs` at its crate root.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use earshot_core::asr::{MeetingTime, Word};
use earshot_core::audio::{PIPELINE_RATE, read_wav};

use crate::fail;

/// Length of one dev scene (s).
pub const SCENE_S: usize = 240;
/// Pause between the utterances of a turn (ms).
pub const UTTERANCE_GAP_MS: usize = 200;
/// Scene sequence seed, plus the pair's index.
pub const SCENE_SEED: u64 = 6_930;
/// The first meeting of each AMI dev series.
pub const AMI_DEV: [&str; 5] = ["ES2003a", "ES2011a", "IS1008a", "TS3004a", "TS3006a"];
/// Letters a speaker's words file can have.
pub const AMI_LETTERS: [&str; 5] = ["A", "B", "C", "D", "E"];

/// Does `word` end a sentence (`.`, `?` or `!`, ignoring closing quotes and brackets)?
pub fn ends_sentence(word: &str) -> bool {
    word.trim_end_matches(['"', '\'', ')'])
        .ends_with(['.', '?', '!'])
}

/// The one-to-one map of `k` diarised speakers onto `true_k` true speakers with the most
/// agreement between `labels` and `truth`; `None` for a diarised speaker left unmapped.
pub fn best_map(
    labels: &[usize],
    truth: &[Option<usize>],
    k: usize,
    true_k: usize,
) -> Vec<Option<usize>> {
    let mut counts = vec![vec![0u64; true_k]; k];
    for (l, t) in labels.iter().zip(truth) {
        if let Some(t) = t {
            counts[*l][*t] += 1;
        }
    }
    fn search(
        k: usize,
        counts: &[Vec<u64>],
        used: &mut Vec<bool>,
        map: &mut Vec<Option<usize>>,
        score: u64,
        best: &mut (u64, Vec<Option<usize>>),
    ) {
        if k == counts.len() {
            if score > best.0 || best.1.is_empty() {
                *best = (score, map.clone());
            }
            return;
        }
        map[k] = None;
        search(k + 1, counts, used, map, score, best);
        for t in 0..used.len() {
            if !used[t] {
                used[t] = true;
                map[k] = Some(t);
                search(k + 1, counts, used, map, score + counts[k][t], best);
                used[t] = false;
                map[k] = None;
            }
        }
    }
    let mut best = (0, Vec::new());
    search(
        0,
        &counts,
        &mut vec![false; true_k],
        &mut vec![None; k],
        0,
        &mut best,
    );
    best.1
}

// ---- scenes ------------------------------------------------------------------------------

pub fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())))
        .map(|e| e.unwrap_or_else(|e| fail(e)).path())
        .collect();
    v.sort();
    v
}

fn read_flac(path: &Path) -> Vec<f32> {
    let mut r =
        claxon::FlacReader::open(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let scale = 1.0 / f64::from(1u32 << (r.streaminfo().bits_per_sample - 1));
    r.samples()
        .map(|s| {
            (f64::from(s.unwrap_or_else(|e| fail(format!("{}: {e}", path.display())))) * scale)
                as f32
        })
        .collect()
}

/// A speaker's utterances in path order, up to about `want` samples.
pub fn utterances(dir: &Path, want: usize) -> Vec<Vec<f32>> {
    let mut out = Vec::new();
    let mut held = 0;
    for chapter in sorted_entries(dir).into_iter().filter(|p| p.is_dir()) {
        for flac in sorted_entries(&chapter)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "flac"))
        {
            let u = read_flac(&flac);
            held += u.len();
            out.push(u);
            if held >= want {
                return out;
            }
        }
    }
    out
}

/// The 40 dev-clean speaker folders in path order (pairs are consecutive).
pub fn dev_clean_speakers(fixtures: &Path) -> Vec<PathBuf> {
    let root = fixtures.join("librispeech/dev-clean/LibriSpeech/dev-clean");
    let speakers: Vec<PathBuf> = sorted_entries(&root)
        .into_iter()
        .filter(|p| p.is_dir())
        .collect();
    if speakers.len() != 40 {
        fail(format!(
            "{}: expected 40 dev-clean speakers",
            root.display()
        ));
    }
    speakers
}

/// A deterministic draw in `[0, 1)`, independent of the audio.
fn draw(state: &mut u64) -> f64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    (*state >> 11) as f64 / (1u64 << 53) as f64
}

fn between(state: &mut u64, lo: f64, hi: f64) -> f64 {
    lo + (hi - lo) * draw(state)
}

/// One true turn, in samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrueTurn {
    pub speaker: usize,
    pub start: usize,
    pub end: usize,
}

/// The two-speaker scene of the report.
pub fn build_scene(pair: [&[Vec<f32>]; 2], seed: u64, want: usize) -> (Vec<f32>, Vec<TrueTurn>) {
    let hz = PIPELINE_RATE.hz() as f64;
    let inner_gap = PIPELINE_RATE.hz() as usize * UTTERANCE_GAP_MS / 1000;
    let mut state = seed;
    let mut next = [0usize, 0];
    let mut audio: Vec<f32> = Vec::with_capacity(want);
    let mut turns: Vec<TrueTurn> = Vec::new();
    let mut speaker = 0;
    while audio.len() < want {
        let target = (between(&mut state, 5.0, 30.0) * hz) as usize;
        let interrupted = draw(&mut state) < 0.2;
        let cut = between(&mut state, 0.3, 0.7);
        let overlaps = draw(&mut state) < 0.2;
        let overlap_s = between(&mut state, 0.2, 1.0);
        let silence_s = between(&mut state, 0.1, 0.8);
        let mut piece: Vec<f32> = Vec::new();
        while piece.len() < target && next[speaker] < pair[speaker].len() {
            if !piece.is_empty() {
                piece.resize(piece.len() + inner_gap, 0.0);
            }
            piece.extend_from_slice(&pair[speaker][next[speaker]]);
            next[speaker] += 1;
        }
        if piece.is_empty() {
            break;
        }
        if interrupted {
            let last = pair[speaker][next[speaker] - 1].len();
            let keep = ((last as f64) * cut) as usize;
            piece.truncate(piece.len() - last + keep);
        }
        let start = match turns.last() {
            None => 0,
            Some(prev) if overlaps => {
                let ov = ((overlap_s * hz) as usize).min((prev.end - prev.start) / 2);
                prev.end - ov
            }
            Some(prev) => prev.end + (silence_s * hz) as usize,
        };
        let end = start + piece.len();
        if audio.len() < end {
            audio.resize(end, 0.0);
        }
        for (a, p) in audio[start..end].iter_mut().zip(&piece) {
            *a += p;
        }
        turns.push(TrueTurn {
            speaker,
            start,
            end,
        });
        speaker = 1 - speaker;
    }
    audio.truncate(want);
    for t in &mut turns {
        t.end = t.end.min(want);
    }
    turns.retain(|t| t.start < t.end);
    let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if peak > 0.99 {
        let g = 0.99 / peak;
        audio.iter_mut().for_each(|x| *x *= g);
    }
    (audio, turns)
}

/// The true speaker at time `ms`: the one whose turn holds it; `None` in an overlap or in no
/// turn.
pub fn turn_at(ms: f64, turns: &[TrueTurn]) -> Option<usize> {
    let at = (ms * f64::from(PIPELINE_RATE.hz()) / 1000.0) as usize;
    let mut on = turns
        .iter()
        .filter(|t| t.start <= at && at < t.end)
        .map(|t| t.speaker);
    let first = on.next()?;
    if on.any(|s| s != first) {
        None
    } else {
        Some(first)
    }
}

// ---- AMI ---------------------------------------------------------------------------------

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let rest = &tag[at..];
    Some(&rest[..rest.find('"')?])
}

/// One speaker's words file under the report's rules; times in ms.
pub fn ami_words(xml: &str) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find("<w ") {
        rest = &rest[at..];
        let close = rest.find('>').unwrap_or_else(|| fail("unterminated <w>"));
        let tag = &rest[..close];
        if tag.ends_with('/') {
            rest = &rest[close + 1..];
            continue;
        }
        let body_end = rest
            .find("</w>")
            .unwrap_or_else(|| fail("<w> without </w>"));
        let body = rest[close + 1..body_end].trim().to_owned();
        let tag = tag.to_owned();
        rest = &rest[body_end + 4..];
        if attr(&tag, "punc") == Some("true") {
            if ends_sentence(&body)
                && let Some(last) = words.last_mut()
                && !ends_sentence(&last.text)
            {
                last.text.push('.');
            }
            continue;
        }
        if attr(&tag, "trunc") == Some("true") || body.is_empty() {
            continue;
        }
        let time = |name: &str| -> Option<u64> {
            attr(&tag, name)?
                .parse::<f64>()
                .ok()
                .map(|s| (s * 1000.0).round() as u64)
        };
        let (Some(start), Some(end)) = (time("starttime"), time("endtime")) else {
            continue;
        };
        words.push(Word {
            text: body,
            start: MeetingTime::from_millis(start),
            end: MeetingTime::from_millis(end.max(start)),
            probability: 1.0,
        });
    }
    words
}

/// A meeting's words across speakers in start-time order, with the speaker's position as truth.
pub fn ami_meeting_words(
    annotations: &Path,
    meeting: &str,
) -> (Vec<Word>, Vec<Option<usize>>, usize) {
    let mut all: Vec<(Word, usize)> = Vec::new();
    let mut speakers = 0;
    for letter in AMI_LETTERS {
        let p = annotations
            .join("words")
            .join(format!("{meeting}.{letter}.words.xml"));
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let xml = crate::refs::latin1(&bytes);
        all.extend(ami_words(&xml).into_iter().map(|w| (w, speakers)));
        speakers += 1;
    }
    if speakers == 0 {
        fail(format!(
            "{meeting}: no words files; run `cargo xtask fetch-fixtures --set ami-sum`"
        ));
    }
    all.sort_by_key(|(w, s)| (w.start.as_millis(), *s));
    let (words, truth): (Vec<Word>, Vec<usize>) = all.into_iter().unzip();
    (words, truth.into_iter().map(Some).collect(), speakers)
}

/// A 16 kHz mono WAV fixture of fixture set `set`.
pub fn read_mono_wav(path: &Path, set: &str) -> Vec<f32> {
    let file = File::open(path).unwrap_or_else(|e| {
        fail(format!(
            "{}: {e}; run `cargo xtask fetch-fixtures --set {set}`",
            path.display()
        ))
    });
    let wav =
        read_wav(BufReader::new(file)).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    if wav.rate() != PIPELINE_RATE || wav.channels() != 1 {
        fail(format!("{}: expected 16 kHz mono", path.display()));
    }
    wav.into_samples()
}

pub fn fixtures_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EARSHOT_FIXTURES").filter(|v| !v.is_empty()) {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| fail("LOCALAPPDATA")))
        .join("earshot-dev")
        .join("fixtures")
}
