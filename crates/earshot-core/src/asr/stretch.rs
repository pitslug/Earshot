//! Speech pieces on a growing stream (T6.13a): [`transcribe_in_pieces`] for audio that
//! arrives while a meeting runs, so each speech stretch is transcribed once, as soon as it
//! is closed, with the same result as the whole-track pass.
//!
//! [`PieceStream`] keeps the VAD's per-frame probabilities of everything pushed so far and,
//! after each push, decodes the pieces that no later audio can change ([`settled_pieces`]);
//! [`PieceStream::finish`] decodes the rest exactly as [`transcribe_in_pieces`] would.
//! [`PieceStream::push_each`] and [`PieceStream::finish_each`] hand out each piece as soon
//! as its decode returns, with the decoded frontier after it (at least its end), so a host
//! keeps every piece decoded before a later decode of the same call fails or panics. The
//! decoding of each piece (prompt, retries, compression-ratio guard) is the same code as the
//! whole-track pass. Fed the same audio, the two give the same pieces, the same calls in the
//! same order and the same segments, whatever the push sizes.
//!
//! When a piece is settled (`known` = samples covered by whole VAD frames so far; `b` = the
//! earliest a later region can begin once padded: the open region's padded start, else
//! `known - speech_pad`):
//! - a padded region is final when it ends before `b` (nothing later can merge into it);
//! - the first region that is not final has a fixed start; a long one is cut at the same
//!   frames as the whole-track plan as soon as its end is known to lie more than
//!   `max_piece` past the cut start and the cut window's probabilities are in, so a speaker
//!   who never pauses still yields a piece every `max_piece` or less;
//! - the last merged piece is final when the next part (the pending region's remainder, or
//!   anything after `b`) can no longer merge into it (`merge_gap`, `max_piece`). A piece
//!   that ends at a cut of the pending region is final at once (T12.31), without waiting
//!   for the window after it, unless that window's cut could still fall close enough to
//!   join it: only when this cut and the next can both land on the first frame they may
//!   and the two parts then fit one window (`settle` has the proof and the exact
//!   condition).
//!
//!
//! # Resuming (T6.13c)
//!
//! What a stream decoded ([`DecodedPieces`]: its pieces and its decoded frontier) is enough
//! to finish the whole-track pass later on the recording without decoding those pieces
//! again: [`transcribe_rest_in_pieces`] plans the recording as [`transcribe_in_pieces`]
//! does, keeps the stored pieces and decodes only the plan's pieces that end after the
//! frontier, with the decoder state (prompt, first-piece rule, language, report) the
//! stream had. Fed the audio the stream heard, the result is the whole-track pass (the
//! stored pieces are a prefix of its plan and later pieces start at or after the
//! frontier). On a recording whose plan differs slightly (the live stream before 16-bit
//! quantisation) the stored pieces are kept and a later piece that starts before the
//! frontier is decoded from the frontier on, so nothing is decoded twice and no speech
//! after the frontier is missed.
//!
//! [`transcribe_in_pieces`]: super::transcribe_in_pieces

use serde::{Deserialize, Serialize};

use super::pieces::{
    PieceConfig, PieceDecoder, PieceReport, RawRegions, Span, Speech, cut_point,
    frame_probabilities, merge_parts, pad_and_merge, plan_pieces, raw_regions, samples,
    speech_regions,
};
use super::transcriber::{
    AsrError, BackendInfo, CancelToken, RawSegment, TranscribeOptions, Transcriber, Transcription,
    check_pipeline_rate,
};
use super::types::MeetingTime;
use super::vad::Vad;
use crate::audio::MonoBuffer;

/// Settled pieces are looked for after at least this many new VAD frames (only when
/// pieces appear is affected, never which).
pub const SETTLE_EVERY_FRAMES: usize = 8;

/// The pieces of the whole-track plan ([`plan_pieces`] over [`speech_regions`]) that no
/// later audio can change, given the probabilities of `probs.len()` whole frames of `frame`
/// samples (module docs). Always a prefix of the plan over any longer stream.
#[must_use]
pub fn settled_pieces(probs: &[f32], frame: usize, config: &PieceConfig) -> Vec<Span> {
    settle(probs, frame, config).0
}

/// [`settled_pieces`] and the frontier: every speech sample before it lies in one of the
/// settled pieces (the start of what is still open, or the earliest a later region can
/// begin).
fn settle(probs: &[f32], frame: usize, config: &PieceConfig) -> (Vec<Span>, usize) {
    let known = probs.len() * frame;
    let max = samples(config.max_piece);
    let merge_gap = samples(config.merge_gap);
    let pad = samples(config.speech_pad);
    let RawRegions { closed, open } = raw_regions(probs, frame, config);
    let mut raw = closed;
    if let Some((start, silence_from)) = open {
        raw.push(Span {
            start,
            end: silence_from.unwrap_or(known),
        });
    }
    // As if the stream ended now; end positions are lower bounds of the final ones.
    let regions = pad_and_merge(raw, known, config);
    let b = match open {
        Some((start, _)) => start.saturating_sub(pad),
        None => known.saturating_sub(pad),
    };
    let finals = regions.iter().take_while(|r| r.end < b).count();
    let mut parts: Vec<Span> = Vec::new();
    for r in &regions[..finals] {
        let mut s = r.start;
        while r.end - s > max {
            let cut = cut_point(s, probs, frame, max);
            parts.push(Span { start: s, end: cut });
            s = cut;
        }
        parts.push(Span {
            start: s,
            end: r.end,
        });
    }
    // The start of whatever comes next: the pending region's remainder after its known
    // cuts, else at the earliest `b`.
    let mut ends_at_cut = false;
    let next_start = match regions.get(finals) {
        Some(pending) => {
            let mut s = pending.start;
            while pending.end - s > max && (s + max) / frame <= probs.len() {
                let cut = cut_point(s, probs, frame, max);
                parts.push(Span { start: s, end: cut });
                s = cut;
                ends_at_cut = true;
            }
            s
        }
        None => b,
    };
    let mut pieces = merge_parts(&parts, merge_gap, max);
    let mut frontier = next_start;
    if let Some(&last) = pieces.last() {
        let can_merge = if ends_at_cut {
            // `last` ends at the pending region's latest cut `c = next_start`, made on the
            // window starting at `s` (`last.start <= s`: equal unless earlier parts were
            // merged in). Every part up to `c` is the whole-track plan's, and the plan
            // merges part by part, so only its next part `p` matters: it starts at `c`
            // (the region runs on past `c`) and joins `last` exactly when
            // `p.end - last.start <= max` (`merge_parts`; the gap is zero). `p` is either
            // - the region's remainder, when the region turns out to end within `max` of
            //   `c`: `p.end` is the region's final end, at least `pending.end` (ends are
            //   lower bounds), and `pending.end - s > max` is why `c` was cut at all, so
            //   `p.end - last.start > max` and it never joins; or
            // - the next cut part: `p.end = cut_point(c, ..)` over probabilities not all
            //   heard yet, never before `earliest_cut(c, ..)`.
            // So nothing can join `last` once `earliest_cut(c) - last.start > max`;
            // otherwise the next cut decides and `last` waits for it.
            earliest_cut(next_start, frame, max) - last.start <= max
        } else {
            // The next part starts at or after `next_start` and is never empty (cuts move
            // forward, regions have length), so it cannot fit within `max` of `last.start`
            // once `next_start - last.start` reaches `max`.
            next_start.saturating_sub(last.end) <= merge_gap
                && next_start.saturating_sub(last.start) < max
        };
        if can_merge {
            pieces.pop();
            frontier = last.start;
        }
    }
    (pieces, frontier)
}

/// The earliest [`cut_point`] can cut a region longer than `max` samples starting at `s`,
/// whatever the probabilities: its first candidate frame (`cut_point` takes a frame of
/// `(s + max / 2).div_ceil(frame) ..< (s + max) / frame`, else `s + max`, then clamps, which
/// keeps the order).
pub(super) fn earliest_cut(s: usize, frame: usize, max: usize) -> usize {
    let lo = (s + max / 2).div_ceil(frame);
    let hi = (s + max) / frame;
    let first = if lo < hi { lo * frame } else { s + max };
    first.clamp(s + 1, s + max)
}

/// A piece decoded by [`PieceStream`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamPiece {
    /// Its span in samples from the start of the stream.
    pub span: Span,
    /// Its segments, in stream time (empty when dropped as a loop).
    pub segments: Vec<RawSegment>,
    /// Whether every decode of it repeated itself and nothing was kept.
    pub dropped: bool,
    /// Whether it was decoded again because its first result repeated itself.
    #[serde(default)]
    pub retried: bool,
    /// Whether speech its words left uncovered was decoded again in parts.
    #[serde(default)]
    pub uncovered: bool,
    /// The stream's language once it was decoded (the first detected one).
    #[serde(default)]
    pub language: Option<String>,
}

impl StreamPiece {
    /// Start in stream time.
    #[must_use]
    pub fn start(&self) -> MeetingTime {
        MeetingTime::from_pipeline_frames(u64::try_from(self.span.start).unwrap_or(u64::MAX))
    }

    /// End in stream time.
    #[must_use]
    pub fn end(&self) -> MeetingTime {
        MeetingTime::from_pipeline_frames(u64::try_from(self.span.end).unwrap_or(u64::MAX))
    }
}

/// What a finished [`PieceStream`] gives back besides its last pieces.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSummary {
    /// Every piece's counts, as [`transcribe_in_pieces`](super::transcribe_in_pieces)
    /// reports them.
    pub report: PieceReport,
    /// The first detected language, else the requested one.
    pub language: Option<String>,
    /// The backend of the last decode.
    pub backend: BackendInfo,
}

/// [`super::transcribe_in_pieces`] on a stream (module docs). Times are from the first
/// sample pushed.
pub struct PieceStream {
    config: PieceConfig,
    frame: usize,
    probs: Vec<f32>,
    /// Samples not yet in a whole VAD frame.
    partial: Vec<f32>,
    /// Audio from `kept_from` on (earlier audio belongs to decoded pieces only).
    audio: Vec<f32>,
    kept_from: usize,
    total: usize,
    decoded: Vec<Span>,
    /// Every speech sample before this is in a decoded piece ([`PieceStream::decoded_through`]).
    frontier: usize,
    since_settle: usize,
    decoder: PieceDecoder,
}

impl std::fmt::Debug for PieceStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PieceStream")
            .field("config", &self.config)
            .field("total", &self.total)
            .field("decoded", &self.decoded.len())
            .finish_non_exhaustive()
    }
}

impl PieceStream {
    /// A stream decoding with `options` and `config`; resets `vad`, which every later call
    /// must be given again (it keeps the VAD's state).
    ///
    /// # Errors
    /// [`AsrError::Config`] for an invalid config or a VAD without frames.
    pub fn new<T, V>(
        transcriber: &T,
        vad: &mut V,
        options: &TranscribeOptions,
        config: &PieceConfig,
    ) -> Result<Self, AsrError>
    where
        T: Transcriber + ?Sized,
        V: Vad + ?Sized,
    {
        config.validate()?;
        let frame = vad.frame_len();
        if frame == 0 {
            return Err(AsrError::Config("VAD frame length is zero".into()));
        }
        vad.reset();
        Ok(Self {
            config: config.clone(),
            frame,
            probs: Vec::new(),
            partial: Vec::with_capacity(frame),
            audio: Vec::new(),
            kept_from: 0,
            total: 0,
            decoded: Vec::new(),
            frontier: 0,
            since_settle: 0,
            decoder: PieceDecoder::new(options, config, transcriber.backend_info()),
        })
    }

    /// Samples pushed so far.
    #[must_use]
    pub fn pushed(&self) -> usize {
        self.total
    }

    /// Samples from the start of the stream before which every speech piece has been
    /// decoded: what a consumer may treat as transcribed (the stream holds the rest).
    #[must_use]
    pub fn decoded_through(&self) -> usize {
        self.frontier
    }

    /// The counts so far.
    #[must_use]
    pub fn report(&self) -> &PieceReport {
        self.decoder.report()
    }

    /// Adds `samples` (16 kHz mono) and decodes every piece that is now settled.
    ///
    /// # Errors
    /// VAD and transcriber errors; cancellation.
    pub fn push<T, V>(
        &mut self,
        transcriber: &mut T,
        vad: &mut V,
        samples: &[f32],
        cancel: &CancelToken,
    ) -> Result<Vec<StreamPiece>, AsrError>
    where
        T: Transcriber + ?Sized,
        V: Vad + ?Sized,
    {
        let mut out = Vec::new();
        self.push_each(transcriber, vad, samples, cancel, &mut |p, _| out.push(p))?;
        Ok(out)
    }

    /// [`PieceStream::push`], handing each piece to `on_piece` as soon as it is decoded,
    /// with the decoded frontier after it ([`PieceStream::decoded_through`] at that moment).
    /// On an error (or a panic in the transcriber) every piece decoded before it has been
    /// handed out, and the frontier given with the last of them is valid for
    /// [`DecodedPieces`].
    ///
    /// # Errors
    /// VAD and transcriber errors; cancellation.
    pub fn push_each<T, V>(
        &mut self,
        transcriber: &mut T,
        vad: &mut V,
        samples: &[f32],
        cancel: &CancelToken,
        on_piece: &mut dyn FnMut(StreamPiece, usize),
    ) -> Result<(), AsrError>
    where
        T: Transcriber + ?Sized,
        V: Vad + ?Sized,
    {
        self.audio.extend_from_slice(samples);
        self.total += samples.len();
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (self.frame - self.partial.len()).min(rest.len());
            self.partial.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.partial.len() == self.frame {
                if cancel.is_cancelled() {
                    return Err(AsrError::Cancelled);
                }
                self.probs.push(vad.speech_probability(&self.partial)?);
                self.partial.clear();
                self.since_settle += 1;
            }
        }
        if self.since_settle < SETTLE_EVERY_FRAMES {
            return Ok(());
        }
        self.since_settle = 0;
        let (settled, frontier) = settle(&self.probs, self.frame, &self.config);
        self.decode_new(transcriber, &settled, cancel, on_piece)?;
        self.frontier = self.frontier.max(frontier);
        Ok(())
    }

    /// Ends the stream: decodes the pieces left, exactly as the whole-track pass plans them.
    ///
    /// # Errors
    /// VAD and transcriber errors; cancellation; [`AsrError::Config`] if the pieces already
    /// decoded are not a prefix of the whole plan (a bug, never expected).
    pub fn finish<T, V>(
        self,
        transcriber: &mut T,
        vad: &mut V,
        cancel: &CancelToken,
    ) -> Result<(Vec<StreamPiece>, StreamSummary), AsrError>
    where
        T: Transcriber + ?Sized,
        V: Vad + ?Sized,
    {
        let mut out = Vec::new();
        let summary = self.finish_each(transcriber, vad, cancel, &mut |p, _| out.push(p))?;
        Ok((out, summary))
    }

    /// [`PieceStream::finish`], handing each piece to `on_piece` as soon as it is decoded,
    /// as [`PieceStream::push_each`] does. After it returns the whole stream is decoded
    /// (the frontier is [`PieceStream::pushed`]).
    ///
    /// # Errors
    /// As [`PieceStream::finish`].
    pub fn finish_each<T, V>(
        mut self,
        transcriber: &mut T,
        vad: &mut V,
        cancel: &CancelToken,
        on_piece: &mut dyn FnMut(StreamPiece, usize),
    ) -> Result<StreamSummary, AsrError>
    where
        T: Transcriber + ?Sized,
        V: Vad + ?Sized,
    {
        if !self.partial.is_empty() {
            let mut padded = std::mem::take(&mut self.partial);
            padded.resize(self.frame, 0.0);
            self.probs.push(vad.speech_probability(&padded)?);
        }
        let regions = speech_regions(&self.probs, self.frame, self.total, &self.config);
        let plan = plan_pieces(&regions, &self.probs, self.frame, &self.config);
        self.decode_new(transcriber, &plan, cancel, on_piece)?;
        self.frontier = self.total;
        let (t, report) = self.decoder.finish(Vec::new());
        Ok(StreamSummary {
            report,
            language: t.language,
            backend: t.backend,
        })
    }

    /// Decodes the pieces of `plan` after the ones already decoded, handing each to
    /// `on_piece` with the frontier after it: the plan's pieces before its end are all
    /// decoded (they are decoded in plan order), so every speech sample before it is in a
    /// decoded piece.
    fn decode_new<T: Transcriber + ?Sized>(
        &mut self,
        transcriber: &mut T,
        plan: &[Span],
        cancel: &CancelToken,
        on_piece: &mut dyn FnMut(StreamPiece, usize),
    ) -> Result<(), AsrError> {
        if plan.len() < self.decoded.len() || plan[..self.decoded.len()] != self.decoded[..] {
            return Err(AsrError::Config(
                "piece stream: decoded pieces are not a prefix of the plan".into(),
            ));
        }
        for &piece in &plan[self.decoded.len()..] {
            let from = piece.start - self.kept_from;
            let to = piece.end - self.kept_from;
            let before = self.decoder.report().clone();
            let speech = Speech {
                probs: &self.probs,
                frame: self.frame,
            };
            let segments =
                self.decoder
                    .decode(transcriber, &self.audio[from..to], piece, speech, cancel)?;
            let report = self.decoder.report();
            let (dropped, retried, uncovered) = (
                report.dropped.len() > before.dropped.len(),
                report.retried > before.retried,
                report.uncovered > before.uncovered,
            );
            self.decoded.push(piece);
            // Later pieces start at or after this one's end.
            self.audio.drain(..to);
            self.kept_from = piece.end;
            self.frontier = self.frontier.max(piece.end);
            on_piece(
                StreamPiece {
                    span: piece,
                    segments,
                    dropped,
                    retried,
                    uncovered,
                    language: self.decoder.language().cloned(),
                },
                self.frontier,
            );
        }
        Ok(())
    }
}

/// What a [`PieceStream`] decoded before it finished or stopped: every piece it gave back,
/// in order, and its decoded frontier ([`PieceStream::decoded_through`]) after the last of
/// them. Kept so the whole-track pass can finish from there
/// ([`transcribe_rest_in_pieces`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecodedPieces {
    /// The decoded pieces, in stream order.
    pub pieces: Vec<StreamPiece>,
    /// Samples from the start of the stream before which every speech piece is in
    /// `pieces`.
    pub through: usize,
}

impl DecodedPieces {
    /// Checks the record against a recording of `len` samples: pieces in order, not empty,
    /// not overlapping, and none past the frontier, which is within the recording.
    ///
    /// # Errors
    /// [`AsrError::Config`] naming the first inconsistency.
    pub fn check(&self, len: usize) -> Result<(), AsrError> {
        let fail = |m: &str| Err(AsrError::Config(format!("decoded pieces: {m}")));
        if self.through > len {
            return fail("the frontier lies past the end of the recording");
        }
        let mut end = 0;
        for p in &self.pieces {
            if p.span.is_empty() {
                return fail("a piece is empty");
            }
            if p.span.start < end {
                return fail("pieces overlap or are out of order");
            }
            if p.span.end > self.through {
                return fail("a piece lies past the frontier");
            }
            end = p.span.end;
        }
        Ok(())
    }
}

/// [`super::transcribe_in_pieces`] over `audio` for everything `done` does not cover
/// (module docs, Resuming): the stored pieces' segments, then the plan's pieces ending
/// after `done.through` (each started no earlier than it), decoded with the state the
/// stream had. The report counts the stored pieces too. Segment times are relative to the
/// start of `audio`.
///
/// # Errors
/// [`AsrError::Config`] for an invalid config or a record inconsistent with `audio`
/// ([`DecodedPieces::check`]); VAD and transcriber errors; cancellation.
pub fn transcribe_rest_in_pieces<T, V>(
    transcriber: &mut T,
    vad: &mut V,
    audio: &MonoBuffer,
    options: &TranscribeOptions,
    config: &PieceConfig,
    done: &DecodedPieces,
    cancel: &CancelToken,
) -> Result<(Transcription, PieceReport), AsrError>
where
    T: Transcriber + ?Sized,
    V: Vad + ?Sized,
{
    config.validate()?;
    check_pipeline_rate(audio)?;
    done.check(audio.samples.len())?;
    let frame = vad.frame_len();
    let probs = frame_probabilities(vad, &audio.samples, cancel)?;
    let regions = speech_regions(&probs, frame, audio.samples.len(), config);
    let rest: Vec<Span> = plan_pieces(&regions, &probs, frame, config)
        .into_iter()
        .filter(|p| p.end > done.through)
        .map(|p| Span {
            start: p.start.max(done.through),
            end: p.end,
        })
        .collect();
    let mut decoder =
        PieceDecoder::resume(options, config, transcriber.backend_info(), &done.pieces);
    let mut segments: Vec<RawSegment> = done
        .pieces
        .iter()
        .flat_map(|p| p.segments.iter().cloned())
        .collect();
    let speech = Speech {
        probs: &probs,
        frame,
    };
    for piece in rest {
        segments.extend(decoder.decode(
            transcriber,
            &audio.samples[piece.start..piece.end],
            piece,
            speech,
            cancel,
        )?);
    }
    Ok(decoder.finish(segments))
}
