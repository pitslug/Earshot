//! The AMI dev reference and mixes of `docs/far-speakers/ami-dev-calibration.md` (T12.9), one
//! copy shared by `cluster_sweep` and `vp_meetings` (T12.67). Times only: the words of the
//! annotation files are never read for their text.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use earshot_core::audio::{PIPELINE_RATE, quantise_pcm16, read_wav};
use earshot_core::metrics::der::Turn;

/// Letters a speaker's words file can have.
pub const AMI_LETTERS: [&str; 5] = ["A", "B", "C", "D", "E"];
/// One speaker's reference intervals closer than this are joined (s).
pub const JOIN_GAP_S: f64 = 0.5;

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let rest = &tag[at..];
    Some(&rest[..rest.find('"')?])
}

/// One speaker's word intervals (s): every `<w>` that is not punctuation and has times with
/// end > start, in file order.
pub fn word_intervals(xml: &str) -> Result<Vec<(f64, f64)>, String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find("<w ") {
        rest = &rest[at..];
        let close = rest.find('>').ok_or("unterminated <w>")?;
        let tag = &rest[..close];
        rest = &rest[close + 1..];
        if attr(tag, "punc") == Some("true") {
            continue;
        }
        let time = |n: &str| attr(tag, n).and_then(|v| v.parse::<f64>().ok());
        if let (Some(s), Some(e)) = (time("starttime"), time("endtime"))
            && e > s
        {
            out.push((s, e));
        }
    }
    Ok(out)
}

/// Joins intervals closer than [`JOIN_GAP_S`] (sorted first).
pub fn join(mut v: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (s, e) in v {
        match out.last_mut() {
            Some(last) if s - last.1 < JOIN_GAP_S => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

/// A meeting's reference.
pub struct Reference {
    /// Speech labelled by `nxt_agent` letter.
    pub turns: Vec<Turn>,
    /// Speakers with any speech.
    pub speakers: usize,
    /// Scored region (first to last reference word, s).
    pub uem: (f64, f64),
}

/// A meeting's reference speech ([`Reference`]).
pub fn reference(fixtures: &Path, meeting: &str) -> Result<Reference, String> {
    let mut reference = Vec::new();
    let mut true_k = 0;
    for letter in AMI_LETTERS {
        let p = fixtures
            .join("ami/annotations/words")
            .join(format!("{meeting}.{letter}.words.xml"));
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        let xml: String = bytes.iter().map(|&b| char::from(b)).collect();
        let speech = join(word_intervals(&xml)?);
        if speech.is_empty() {
            continue;
        }
        true_k += 1;
        reference.extend(speech.into_iter().map(|(s, e)| Turn::new(s, e, letter)));
    }
    if true_k == 0 {
        return Err(format!(
            "{meeting}: no words files; run `cargo xtask fetch-fixtures --set ami-sum`"
        ));
    }
    let uem = (
        reference
            .iter()
            .map(|t| t.start)
            .fold(f64::INFINITY, f64::min),
        reference.iter().map(|t| t.end).fold(0.0, f64::max),
    );
    Ok(Reference {
        turns: reference,
        speakers: true_k,
        uem,
    })
}

/// A meeting's `Mix-Headset` recording at 16 kHz mono, quantised as the FLAC writer stores it.
pub fn mix(fixtures: &Path, meeting: &str) -> Result<Vec<f32>, String> {
    let path = fixtures.join(format!("ami/audio/{meeting}.Mix-Headset.wav"));
    let file = File::open(&path).map_err(|e| {
        format!(
            "{}: {e}; run `cargo xtask fetch-fixtures --set ami-dev`",
            path.display()
        )
    })?;
    let wav = read_wav(BufReader::new(file)).map_err(|e| format!("{}: {e}", path.display()))?;
    if wav.rate() != PIPELINE_RATE || wav.channels() != 1 {
        return Err(format!("{}: expected 16 kHz mono", path.display()));
    }
    Ok(quantise_pcm16(&wav.into_samples()))
}
