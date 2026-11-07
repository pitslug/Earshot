//! Reference preparation for `asr_eval`: pure functions, unit-tested. Also compiled into
//! `earshot-runtime`'s `model_eval` and `diar_eval` examples and `cargo xtask t613` (by path),
//! so all of them score AMI by one rule ([`load_ami_ihm`] / [`load_ami_sdm`],
//! [`AmiHeadset::score`], [`parse_ami_words`]); keep it to std and `earshot-core`. Its
//! tests are in `refs_tests.rs`, a module of `asr_eval` only, so the other crates never
//! compile them.

use std::fs::File;
use std::io::BufReader;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use earshot_core::asr::{Transcription, Word, drop_non_speech};
use earshot_core::audio::{PIPELINE_RATE, read_wav};
use earshot_core::metrics::wer::{WerCounts, word_errors_normalized};

/// AMI meetings of the `ami` fixture set (`accept m2`, `model_eval`).
pub const AMI_MEETINGS: [&str; 2] = ["IS1009a", "ES2004a"];
/// AMI speaker-to-headset table (pinned member of the annotations archive).
pub const AMI_MEETINGS_XML: &str = "ami/annotations/corpusResources/meetings.xml";
/// How to get the AMI fixtures.
pub const FETCH_AMI: &str = "run `cargo xtask fetch-fixtures --set ami`";

/// One speaker of the AMI IHM condition with oracle segmentation (Kaldi/ESPnet; T2.5b,
/// decided by the owner): every annotated segment of the speaker, clipped to the
/// meeting's UEM, is one utterance, decoded by one call on the speaker's own headset and
/// aligned against the speaker's words starting inside it ([`segment_references`]); the
/// speaker's UEM words outside every segment are deletions ([`AmiHeadset::score`]).
#[derive(Debug, Clone, PartialEq)]
pub struct AmiHeadset {
    pub meeting: String,
    /// Headset channel (`Headset-<n>`), from `corpusResources/meetings.xml`.
    pub headset: u8,
    /// The AMI audio channel the segments are decoded from ([`AmiMic::channel`]):
    /// `Headset-<n>` (IHM) or `Array1-01` (SDM).
    pub channel: String,
    /// Speaker letter.
    pub speaker: String,
    /// UEM span `(start, end)` in seconds.
    pub uem: (f64, f64),
    /// The whole recording of [`AmiHeadset::channel`], 16 kHz mono (shared by every speaker
    /// of a meeting for SDM).
    pub audio: Arc<[f32]>,
    /// Every word of the speaker (all times).
    pub words: Vec<AmiWord>,
    /// Segments, their references and the words outside them.
    pub plan: SegmentRefs,
    /// Sample range of each `plan.spans` entry in `audio` (never empty).
    pub ranges: Vec<Range<usize>>,
}

/// Word errors of one headset: per segment (in `plan.spans` order) and the outside words.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AmiScore {
    pub segments: Vec<WerCounts>,
    /// The speaker's UEM words outside every segment, all deletions (zero when none).
    pub outside: WerCounts,
}

impl AmiScore {
    /// Segments and outside words summed.
    pub fn total(&self) -> WerCounts {
        self.segments.iter().fold(self.outside, |acc, &c| acc + c)
    }
}

impl AmiHeadset {
    /// Builds one speaker from its annotation texts and the audio of `mic`'s channel for
    /// this speaker (16 kHz mono). The segments' headset is checked against `meetings.xml`
    /// for every mic: the oracle segments and references never depend on which recording is
    /// decoded.
    ///
    /// # Errors
    /// Unparseable annotations, segments on another headset than `meetings.xml` says, or a
    /// segment past the end of the audio.
    pub fn new(
        meeting: &str,
        (headset, speaker): (u8, &str),
        uem: (f64, f64),
        words_xml: &str,
        segments_xml: &str,
        (mic, audio): (AmiMic, Arc<[f32]>),
    ) -> Result<Self, String> {
        let words = parse_ami_words(words_xml).map_err(|e| format!("words: {e}"))?;
        let segments = parse_ami_segments(segments_xml).map_err(|e| format!("segments: {e}"))?;
        if segments.channel != headset {
            return Err(format!(
                "segments of speaker {speaker} are on headset {}, but {AMI_MEETINGS_XML} puts \
                 the speaker on headset {headset}",
                segments.channel
            ));
        }
        let plan = segment_references(&words, &segments.spans, uem.0, uem.1)?;
        let rate = f64::from(PIPELINE_RATE.hz());
        let ranges = plan
            .spans
            .iter()
            .map(|&span| {
                let r = span_samples(span, rate, audio.len());
                if r.is_empty() {
                    Err(format!(
                        "segment {span:?} lies past the end of the audio ({} samples)",
                        audio.len()
                    ))
                } else {
                    Ok(r)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AmiHeadset {
            meeting: meeting.to_owned(),
            headset,
            channel: mic.channel(headset),
            speaker: speaker.to_owned(),
            uem,
            audio,
            words,
            plan,
            ranges,
        })
    }

    /// The audio of every segment, in `plan.spans` order.
    pub fn segment_audio(&self) -> impl Iterator<Item = &[f32]> {
        self.ranges.iter().map(|r| &self.audio[r.clone()])
    }

    /// Scores one decode per segment (`hypotheses[i]` for `plan.spans[i]`) after Whisper
    /// normalisation, plus the outside words as deletions.
    ///
    /// # Errors
    /// Not exactly one hypothesis per segment.
    pub fn score<S: AsRef<str>>(&self, hypotheses: &[S]) -> Result<AmiScore, String> {
        if hypotheses.len() != self.plan.references.len() {
            return Err(format!(
                "{} hypotheses for {} segments",
                hypotheses.len(),
                self.plan.references.len()
            ));
        }
        let segments = self
            .plan
            .references
            .iter()
            .zip(hypotheses)
            .map(|(r, h)| word_errors_normalized(r, h.as_ref()))
            .collect();
        let outside = if self.plan.outside.is_empty() {
            WerCounts::default()
        } else {
            word_errors_normalized(&self.plan.outside.join(" "), "")
        };
        Ok(AmiScore { segments, outside })
    }

    /// The speaker's UEM words inside a segment, in start order (`model_eval`'s timing and
    /// asr_eval's `word_times`).
    #[allow(dead_code)] // also compiled into model_eval, which shares this file
    pub fn segment_words(&self) -> Vec<&AmiWord> {
        let mut w: Vec<&AmiWord> = self
            .words
            .iter()
            .filter(|w| w.start >= self.uem.0 && w.start < self.uem.1)
            .filter(|w| {
                self.plan
                    .spans
                    .iter()
                    .any(|&(s, e)| w.start >= s && w.start < e)
            })
            .collect();
        w.sort_by(|a, b| a.start.total_cmp(&b.start));
        w
    }
}

/// The words production keeps from a transcription: Whisper's non-speech annotations
/// ("[BLANK_AUDIO]", "(buzzing)") dropped per segment, as `Transcription::into_segments`
/// does before a segment is stored (T6.7). Both `asr.wer_ami_ihm` and `model_eval` score
/// [`hypothesis_text`] of these words.
pub fn kept_words(t: &Transcription) -> Vec<Word> {
    t.segments
        .iter()
        .flat_map(|s| drop_non_speech(s.timed_words()))
        .collect()
}

/// `words` joined by single spaces: the hypothesis text that is scored.
pub fn hypothesis_text(words: &[Word]) -> String {
    words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn read_ami(fixtures: &Path, rel: &str) -> Result<Vec<u8>, String> {
    let p = fixtures.join(rel);
    std::fs::read(&p).map_err(|e| format!("{}: {e}; {FETCH_AMI}", p.display()))
}

/// Which AMI recording the oracle segments are decoded from. The segments, references and
/// scoring are the same for both (Kaldi `egs/ami` and ESPnet `egs2/ami` use one segment
/// list for ihm and sdm).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmiMic {
    /// Individual headset microphones: each speaker's own headset (`asr.wer_ami_ihm`).
    Ihm,
    /// Single distant microphone: the first microphone of the first array, `Array1-01`
    /// (the Kaldi/ESPnet `sdm1` condition), one far-field recording shared by every speaker
    /// (T2.5c, informational).
    Sdm,
}

impl AmiMic {
    /// The AMI audio channel (`ami/audio/<meeting>.<channel>.wav`) decoded for the speaker
    /// on headset `headset`.
    pub fn channel(self, headset: u8) -> String {
        match self {
            AmiMic::Ihm => format!("Headset-{headset}"),
            AmiMic::Sdm => "Array1-01".to_owned(),
        }
    }
}

/// One AMI channel recording: 16 kHz and exactly one channel (a multi-channel file would be
/// mixed by `to_mono`, which is neither a headset nor a single distant microphone).
fn read_ami_audio(fixtures: &Path, meeting: &str, channel: &str) -> Result<Arc<[f32]>, String> {
    let wav_path = fixtures.join(format!("ami/audio/{meeting}.{channel}.wav"));
    let file =
        File::open(&wav_path).map_err(|e| format!("{}: {e}; {FETCH_AMI}", wav_path.display()))?;
    let wav = read_wav(BufReader::new(file)).map_err(|e| format!("{}: {e}", wav_path.display()))?;
    if wav.rate() != PIPELINE_RATE {
        return Err(format!("{}: expected 16 kHz", wav_path.display()));
    }
    if wav.channels() != 1 {
        return Err(format!(
            "{}: expected one channel, got {}",
            wav_path.display(),
            wav.channels()
        ));
    }
    Ok(wav.into_samples().into())
}

/// Loads every headset of `meetings` from the fixtures directory (the `ami` set), in
/// meeting then headset order: the IHM condition of `asr.wer_ami_ihm`.
///
/// # Errors
/// As [`load_ami`].
pub fn load_ami_ihm(fixtures: &Path, meetings: &[&str]) -> Result<Vec<AmiHeadset>, String> {
    load_ami(fixtures, meetings, AmiMic::Ihm)
}

/// The same speakers, segments and references as [`load_ami_ihm`], every segment decoded
/// from the meeting's single distant microphone (`Array1-01`) instead of the speaker's
/// headset; overlapping segments of different speakers are each decoded on their own.
///
/// # Errors
/// As [`load_ami`].
pub fn load_ami_sdm(fixtures: &Path, meetings: &[&str]) -> Result<Vec<AmiHeadset>, String> {
    load_ami(fixtures, meetings, AmiMic::Sdm)
}

/// Loads every speaker of `meetings` with the recording `mic` names, in meeting then
/// headset order. An SDM recording is read once per meeting and shared.
///
/// # Errors
/// A missing fixture (with the fetch instruction), unparseable annotations, audio that is
/// not 16 kHz with one channel, or any [`AmiHeadset::new`] error.
pub fn load_ami(
    fixtures: &Path,
    meetings: &[&str],
    mic: AmiMic,
) -> Result<Vec<AmiHeadset>, String> {
    let channels_xml = latin1(&read_ami(fixtures, AMI_MEETINGS_XML)?);
    let mut out = Vec::new();
    for &meeting in meetings {
        let uem = latin1(&read_ami(fixtures, &format!("ami/uem/{meeting}.uem"))?);
        let uem = parse_uem(&uem, meeting)?;
        let channels = parse_ami_channels(&channels_xml, meeting)
            .map_err(|e| format!("{AMI_MEETINGS_XML}: {e}"))?;
        let shared = match mic {
            AmiMic::Ihm => None,
            AmiMic::Sdm => Some(read_ami_audio(fixtures, meeting, &mic.channel(0))?),
        };
        for (channel, agent) in channels {
            let words = format!("ami/annotations/words/{meeting}.{agent}.words.xml");
            let segments = format!("ami/annotations/segments/{meeting}.{agent}.segments.xml");
            let words_xml = latin1(&read_ami(fixtures, &words)?);
            let segments_xml = latin1(&read_ami(fixtures, &segments)?);
            let audio = match &shared {
                Some(a) => Arc::clone(a),
                None => read_ami_audio(fixtures, meeting, &mic.channel(channel))?,
            };
            let headset = AmiHeadset::new(
                meeting,
                (channel, &agent),
                uem,
                &words_xml,
                &segments_xml,
                (mic, audio),
            )
            .map_err(|e| format!("{meeting} speaker {agent} ({words}, {segments}): {e}"))?;
            out.push(headset);
        }
    }
    Ok(out)
}

/// One word of an AMI manual words file.
#[derive(Debug, Clone, PartialEq)]
pub struct AmiWord {
    /// Start time (s).
    pub start: f64,
    /// Word text as it enters the reference.
    pub text: String,
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let at = tag.find(&key)? + key.len();
    let rest = &tag[at..];
    Some(&rest[..rest.find('"')?])
}

fn unescape(s: &str) -> String {
    s.replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Parses an AMI manual annotations v1.6.2 words file (`words/<meeting>.<spk>.words.xml`).
///
/// Reference rules (documented in `main.rs`):
/// - only `<w>` elements count; `<vocalsound>`, `<disfmarker>`, `<gap>` are not words;
/// - `punc="true"` entries are punctuation, not words;
/// - `trunc="true"` entries are unfinished word fragments, which NIST scoring treats as
///   optionally deletable; they are left out of the reference;
/// - spelled acronyms are written with underscores (`L_C_D_`, `T_V_`): the underscores are
///   removed (`LCD`, `TV`);
/// - XML character references are decoded; the file is ISO-8859-1 and is read as bytes
///   mapped one to one onto chars.
///
/// # Errors
/// A `<w>` without a parseable `starttime` or without a closing `</w>`.
pub fn parse_ami_words(xml: &str) -> Result<Vec<AmiWord>, String> {
    let mut words = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find("<w ") {
        rest = &rest[at..];
        let close = rest.find('>').ok_or("unterminated <w> tag")?;
        let tag = &rest[..close];
        if tag.ends_with('/') {
            rest = &rest[close + 1..];
            continue;
        }
        let body_end = rest.find("</w>").ok_or("<w> without </w>")?;
        let body = &rest[close + 1..body_end];
        rest = &rest[body_end + 4..];
        if attr(tag, "punc") == Some("true") || attr(tag, "trunc") == Some("true") {
            continue;
        }
        let start: f64 = attr(tag, "starttime")
            .ok_or_else(|| format!("word without starttime: {tag}"))?
            .parse()
            .map_err(|e| format!("bad starttime in {tag}: {e}"))?;
        let text = unescape(body.trim()).replace('_', "");
        if !text.is_empty() {
            words.push(AmiWord { start, text });
        }
    }
    Ok(words)
}

/// Decodes ISO-8859-1 bytes.
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// The reference of one meeting: every participant's words that start inside the UEM span
/// `[from, to)`, ordered by start time (ties keep participant order), joined by spaces.
pub fn ami_reference(speakers: &[Vec<AmiWord>], from: f64, to: f64) -> String {
    let mut all: Vec<&AmiWord> = speakers
        .iter()
        .flatten()
        .filter(|w| w.start >= from && w.start < to)
        .collect();
    all.sort_by(|a, b| a.start.total_cmp(&b.start));
    all.iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `(headset channel, speaker letter)` pairs of `meeting` from the AMI corpus resource
/// `corpusResources/meetings.xml` (`<speaker ... channel="0" nxt_agent="A" .../>` inside
/// `<meeting ... observation="<meeting>">`), sorted by channel. Same rule as xtask's
/// `fixtures::ami_headset_speakers` (shared cases in `fixtures/ami/parse_cases.toml`).
///
/// # Errors
/// The meeting is missing, or its speakers are not one per headset 0-3 with the letters
/// [`AMI_SPEAKERS`].
pub fn parse_ami_channels(xml: &str, meeting: &str) -> Result<Vec<(u8, String)>, String> {
    let open = format!(" observation=\"{meeting}\"");
    let at = xml
        .find(&open)
        .ok_or_else(|| format!("meeting {meeting} not in meetings.xml"))?;
    let body = &xml[at..];
    let body = &body[..body.find("</meeting>").ok_or("unterminated <meeting>")?];
    let mut speakers = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("<speaker ") {
        rest = &rest[i..];
        let end = rest.find('>').ok_or("unterminated <speaker>")?;
        let tag = &rest[..end];
        rest = &rest[end..];
        let channel: u8 = attr(tag, "channel")
            .ok_or_else(|| format!("speaker without channel: {tag}"))?
            .parse()
            .map_err(|e| format!("bad channel in {tag}: {e}"))?;
        let agent =
            attr(tag, "nxt_agent").ok_or_else(|| format!("speaker without nxt_agent: {tag}"))?;
        speakers.push((channel, agent.to_owned()));
    }
    speakers.sort();
    let channels: Vec<u8> = speakers.iter().map(|(c, _)| *c).collect();
    if channels != [0, 1, 2, 3] {
        return Err(format!(
            "{meeting}: expected one speaker on each headset 0-3, got channels {channels:?}"
        ));
    }
    let mut letters: Vec<&str> = speakers.iter().map(|(_, a)| a.as_str()).collect();
    letters.sort_unstable();
    if letters != AMI_SPEAKERS {
        return Err(format!(
            "{meeting}: expected speakers {AMI_SPEAKERS:?}, got {letters:?}"
        ));
    }
    Ok(speakers)
}

/// AMI participant letters of every meeting used (four per meeting).
pub const AMI_SPEAKERS: [&str; 4] = ["A", "B", "C", "D"];

/// The annotated speech segments of one AMI participant.
#[derive(Debug, Clone, PartialEq)]
pub struct AmiSegments {
    /// Headset channel the segments were transcribed from (the `channel` attribute).
    pub channel: u8,
    /// `(transcriber_start, transcriber_end)` in seconds, sorted by start.
    pub spans: Vec<(f64, f64)>,
}

/// Parses an AMI manual annotations v1.6.2 segments file
/// (`segments/<meeting>.<spk>.segments.xml`): every `<segment channel="n"
/// transcriber_start="s" transcriber_end="e">`. These are the oracle segments the standard
/// AMI IHM condition decodes (Kaldi `egs/ami`, ESPnet `egs2/ami`).
///
/// # Errors
/// No segment, a segment without a parseable channel or time, an empty span, or segments
/// on more than one channel.
pub fn parse_ami_segments(xml: &str) -> Result<AmiSegments, String> {
    let mut channel = None;
    let mut spans = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find("<segment ") {
        rest = &rest[at..];
        let close = rest.find('>').ok_or("unterminated <segment> tag")?;
        let tag = &rest[..close];
        rest = &rest[close..];
        let num = |name: &str| -> Result<f64, String> {
            attr(tag, name)
                .ok_or_else(|| format!("segment without {name}: {tag}"))?
                .parse()
                .map_err(|e| format!("bad {name} in {tag}: {e}"))
        };
        let c: u8 = attr(tag, "channel")
            .ok_or_else(|| format!("segment without channel: {tag}"))?
            .parse()
            .map_err(|e| format!("bad channel in {tag}: {e}"))?;
        if *channel.get_or_insert(c) != c {
            return Err(format!(
                "segments on channels {} and {c}",
                channel.unwrap_or(c)
            ));
        }
        let (start, end) = (num("transcriber_start")?, num("transcriber_end")?);
        if !(start >= 0.0 && start < end) {
            return Err(format!("empty or negative span in {tag}"));
        }
        spans.push((start, end));
    }
    let channel = channel.ok_or("no <segment> in segments file")?;
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(AmiSegments { channel, spans })
}

/// One speaker's segments as scored utterances (Kaldi/ESPnet AMI IHM convention).
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentRefs {
    /// Segment spans clipped to the UEM, sorted by start; spans the clipping empties are
    /// left out.
    pub spans: Vec<(f64, f64)>,
    /// Reference of each span in `spans`: the speaker's words whose start lies in
    /// `[span start, span end)`, in start order, joined by spaces (may be empty).
    pub references: Vec<String>,
    /// UEM words of the speaker that lie in no segment, in start order. They are scored as
    /// deletions, never dropped.
    pub outside: Vec<String>,
}

/// Assigns every word of one speaker that starts inside the UEM `[from, to)` to the
/// segment (one of `spans`, clipped to the UEM) whose `[start, end)` contains its start.
///
/// # Errors
/// A word lies in two segments (overlapping segments of one speaker).
pub fn segment_references(
    words: &[AmiWord],
    spans: &[(f64, f64)],
    from: f64,
    to: f64,
) -> Result<SegmentRefs, String> {
    let mut clipped: Vec<(f64, f64)> = spans
        .iter()
        .map(|&(s, e)| (s.max(from), e.min(to)))
        .filter(|(s, e)| s < e)
        .collect();
    clipped.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut per: Vec<Vec<&AmiWord>> = vec![Vec::new(); clipped.len()];
    let mut outside: Vec<&AmiWord> = Vec::new();
    for w in words.iter().filter(|w| w.start >= from && w.start < to) {
        let hits: Vec<usize> = clipped
            .iter()
            .enumerate()
            .filter(|(_, (s, e))| w.start >= *s && w.start < *e)
            .map(|(i, _)| i)
            .collect();
        match hits.as_slice() {
            [] => outside.push(w),
            [i] => per[*i].push(w),
            _ => {
                return Err(format!(
                    "word `{}` at {} s lies in {} segments",
                    w.text,
                    w.start,
                    hits.len()
                ));
            }
        }
    }
    let join = |mut v: Vec<&AmiWord>| -> Vec<String> {
        v.sort_by(|a, b| a.start.total_cmp(&b.start));
        v.iter().map(|w| w.text.clone()).collect()
    };
    Ok(SegmentRefs {
        spans: clipped,
        references: per.into_iter().map(|v| join(v).join(" ")).collect(),
        outside: join(outside),
    })
}

/// Sample range of the span `(start, end)` (seconds) at `rate` Hz, rounded to the nearest
/// sample and clamped to `len` samples (may be empty past the end of the audio).
pub fn span_samples(span: (f64, f64), rate: f64, len: usize) -> Range<usize> {
    let sample = |t: f64| ((t * rate).round().max(0.0) as usize).min(len);
    sample(span.0)..sample(span.1).max(sample(span.0))
}

/// Parses a one-line UEM for `meeting`: `(start, end)` in seconds.
///
/// # Errors
/// Not exactly one line for `meeting`, or unparseable times.
pub fn parse_uem(text: &str, meeting: &str) -> Result<(f64, f64), String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let [line] = lines.as_slice() else {
        return Err(format!("UEM for {meeting} must have one line"));
    };
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() != 4 || f[0] != meeting {
        return Err(format!("UEM line `{line}` is not for {meeting}"));
    }
    let a: f64 = f[2].parse().map_err(|e| format!("UEM start: {e}"))?;
    let b: f64 = f[3].parse().map_err(|e| format!("UEM end: {e}"))?;
    if !(a >= 0.0 && a < b) {
        return Err(format!("UEM line `{line}` has a bad span"));
    }
    Ok((a, b))
}

/// Groups consecutive utterances (durations in seconds, in order) into streams: a stream
/// is closed as soon as it reaches `target_s`; a remainder shorter than half the target is
/// appended to the previous stream. Every utterance is in exactly one stream.
pub fn group_streams(durations: &[f64], target_s: f64) -> Vec<Range<usize>> {
    let mut streams: Vec<Range<usize>> = Vec::new();
    let (mut start, mut acc) = (0, 0.0);
    for (i, d) in durations.iter().enumerate() {
        acc += d;
        if acc >= target_s {
            streams.push(start..i + 1);
            start = i + 1;
            acc = 0.0;
        }
    }
    if start < durations.len() {
        match streams.last_mut() {
            Some(last) if acc < target_s / 2.0 => last.end = durations.len(),
            _ => streams.push(start..durations.len()),
        }
    }
    streams
}

/// Scoring windows around near-end utterance spans (sorted, disjoint) in a signal of `n`
/// samples: each window reaches halfway into the gap to its neighbours, and at most `edge`
/// samples before the first span and after the last one. Windows are disjoint and cover
/// all audio between the first and last span, so nothing between utterances escapes
/// scoring.
pub fn windows(spans: &[Range<usize>], n: usize, edge: usize) -> Vec<Range<usize>> {
    let mut out = Vec::with_capacity(spans.len());
    for (i, s) in spans.iter().enumerate() {
        let start = match i.checked_sub(1).map(|p| &spans[p]) {
            Some(prev) => prev.end + (s.start - prev.end) / 2,
            None => s.start.saturating_sub(edge),
        };
        let end = match spans.get(i + 1) {
            Some(next) => s.end + (next.start - s.end) / 2,
            None => (s.end + edge).min(n),
        };
        out.push(start..end);
    }
    out
}

/// The `asr.wer_aec_delta` seed rule (T2.4g): renders with `base`, then `base + k * step`
/// (k = 1, 2, ...) until the renders' word counts sum to at least `min_words`. `render`
/// returns a render and its word count; it must not depend on recognition results.
///
/// # Errors
/// When `max_seeds` renders hold fewer than `min_words` words.
pub fn render_seeds<T>(
    base: u64,
    step: u64,
    min_words: usize,
    max_seeds: usize,
    mut render: impl FnMut(u64) -> (T, usize),
) -> Result<Vec<(u64, T)>, String> {
    let (mut out, mut words) = (Vec::new(), 0);
    while words < min_words {
        if out.len() == max_seeds {
            return Err(format!("{max_seeds} seeds hold only {words} words"));
        }
        let seed = base + out.len() as u64 * step;
        let (r, w) = render(seed);
        words += w;
        out.push((seed, r));
    }
    Ok(out)
}
