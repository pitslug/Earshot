//! Per-track sample timeline built from timestamped capture packets.
//!
//! WASAPI loopback delivers no packets while nothing renders, and any stream can
//! drop packets under load. The timeline places every packet by its QPC position
//! and fills holes with synthesized silence, so the written track stays aligned
//! with wall time. Platform independent.

use serde::Serialize;

/// QPC positions from `IAudioCaptureClient::GetBuffer` are in 100 ns units.
pub const QPC_UNITS_PER_SEC: u64 = 10_000_000;

/// Gaps shorter than this are treated as timestamp jitter and not filled.
pub const DEFAULT_GAP_TOLERANCE_MS: u32 = 5;

/// Metadata of one captured packet, as returned by `GetBuffer`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketInfo {
    /// QPC position of the first frame, in 100 ns units.
    pub qpc: u64,
    /// Device position of the first frame, in frames.
    pub device_position: u64,
    /// Frames in the packet.
    pub frames: u32,
    /// `AUDCLNT_BUFFERFLAGS_SILENT`: treat the packet as silence regardless of its data.
    pub silent: bool,
    /// `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`.
    pub discontinuity: bool,
    /// `AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR`: `qpc` is not reliable.
    pub timestamp_error: bool,
}

/// One packet as recorded in `timestamps.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PacketRecord {
    /// QPC position of the first frame (100 ns units).
    pub qpc: u64,
    /// Device position of the first frame.
    pub device_position: u64,
    /// Frames in the packet.
    pub frames: u32,
    /// Frame index of the packet's first frame in the written track.
    pub track_frame: u64,
    /// Silence frames synthesized immediately before this packet.
    pub silence_before: u64,
    /// Leading frames of this packet dropped because they predate the track origin.
    pub trimmed_frames: u64,
    /// Packet was flagged silent.
    pub silent: bool,
    /// Packet was flagged as a data discontinuity.
    pub discontinuity: bool,
    /// Packet was flagged with a timestamp error.
    pub timestamp_error: bool,
}

/// f32 track assembled from packets, with gap filling. Mono unless built
/// [`with_channels`](TrackTimeline::with_channels): samples are then interleaved, and
/// every position, count and limit is in frames.
#[derive(Clone, Debug)]
pub struct TrackTimeline {
    sample_rate: u32,
    /// Samples per frame.
    channels: u16,
    origin_qpc: u64,
    tolerance_frames: u64,
    expected_next_qpc: Option<u64>,
    samples: Vec<f32>,
    packets: Vec<PacketRecord>,
    synthesized_frames: u64,
    silence_gaps: u32,
    limit_qpc: u64,
    reanchor: bool,
}

fn frames_to_qpc(frames: u64, sample_rate: u32) -> u64 {
    let units = u128::from(frames) * u128::from(QPC_UNITS_PER_SEC);
    let sr = u128::from(sample_rate);
    ((units + sr / 2) / sr) as u64
}

fn qpc_to_frames(units: u64, sample_rate: u32) -> u64 {
    let f = u128::from(units) * u128::from(sample_rate);
    let q = u128::from(QPC_UNITS_PER_SEC);
    ((f + q / 2) / q) as u64
}

impl TrackTimeline {
    /// New empty track at `sample_rate` whose frame 0 is at `origin_qpc`. Use the same
    /// origin for every track of a recording so their frame 0s coincide.
    pub fn new(sample_rate: u32, origin_qpc: u64) -> Self {
        Self::with_tolerance_ms(sample_rate, origin_qpc, DEFAULT_GAP_TOLERANCE_MS)
    }

    /// Like [`TrackTimeline::new`] with an explicit gap tolerance.
    pub fn with_tolerance_ms(sample_rate: u32, origin_qpc: u64, tolerance_ms: u32) -> Self {
        Self {
            sample_rate,
            channels: 1,
            origin_qpc,
            tolerance_frames: u64::from(sample_rate) * u64::from(tolerance_ms) / 1000,
            expected_next_qpc: None,
            samples: Vec::new(),
            packets: Vec::new(),
            synthesized_frames: 0,
            silence_gaps: 0,
            limit_qpc: u64::MAX,
            reanchor: false,
        }
    }

    /// Makes the track interleaved with `channels` samples per frame (before any
    /// packet is pushed).
    ///
    /// # Panics
    /// If `channels` is 0 or packets were already pushed.
    pub fn with_channels(mut self, channels: u16) -> Self {
        assert!(channels > 0, "a track has at least one channel");
        assert!(
            self.samples.is_empty(),
            "channels are set before any packet"
        );
        self.channels = channels;
        self
    }

    /// Frames in the track so far.
    pub fn frames(&self) -> u64 {
        (self.samples.len() / usize::from(self.channels)) as u64
    }

    /// Samples per frame.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Upper bound for any QPC used to place packets or pad the track (normally the
    /// recording deadline plus a margin). Synthesized silence never extends the track
    /// past this position, so a bogus timestamp cannot cause unbounded growth.
    pub fn set_qpc_limit(&mut self, limit_qpc: u64) {
        self.limit_qpc = limit_qpc;
    }

    /// Marks a stream discontinuity (device re-open) at the current end of the track
    /// and returns that track frame. The next packet is placed exactly at its QPC
    /// relative to the end of the previous one: the gap is filled with silence and an
    /// overlap is trimmed, with no jitter tolerance.
    pub fn mark_discontinuity(&mut self) -> u64 {
        self.reanchor = self.expected_next_qpc.is_some();
        self.frames()
    }

    /// Frames of silence needed to bring the track from its expected next position up
    /// to `qpc`, or 0 when the hole is within tolerance (or negative).
    fn gap_frames_to(&self, qpc: u64) -> u64 {
        let expected = self.expected_next_qpc.unwrap_or(self.origin_qpc);
        let qpc = qpc.min(self.limit_qpc);
        let gap = qpc_to_frames(qpc.saturating_sub(expected), self.sample_rate);
        if gap > self.tolerance_frames { gap } else { 0 }
    }

    /// Appends up to `frames` of silence, never extending the track past the QPC
    /// limit, and returns the frames actually inserted.
    fn insert_silence(&mut self, frames: u64) -> u64 {
        let cap = if self.limit_qpc == u64::MAX {
            u64::MAX
        } else {
            qpc_to_frames(
                self.limit_qpc.saturating_sub(self.origin_qpc),
                self.sample_rate,
            )
        };
        let frames = frames.min(cap.saturating_sub(self.frames()));
        if frames == 0 {
            return 0;
        }
        let ch = usize::from(self.channels);
        self.samples
            .resize(self.samples.len() + frames as usize * ch, 0.0);
        self.synthesized_frames += frames;
        self.silence_gaps += 1;
        frames
    }

    /// Appends one packet. `samples` holds its converted frames (interleaved,
    /// [`channels`](Self::channels) per frame) and is ignored when the packet is
    /// flagged silent (zeros are written instead).
    ///
    /// The first packet that keeps any frames anchors the track at the origin exactly
    /// (no jitter tolerance): frames stamped before the origin are dropped (counted in
    /// `trimmed_frames`), and a later start is preceded by silence. After that,
    /// packets are placed relative to the previous packet (not the origin), so slow
    /// drift between the device clock and QPC never turns into synthesized gaps.
    /// The first packet after [`TrackTimeline::mark_discontinuity`] is placed exactly
    /// like the first one, relative to the end of the previous packet. Every QPC is
    /// clamped to the limit set by [`TrackTimeline::set_qpc_limit`].
    /// A packet flagged `timestamp_error` is appended directly after the previous one.
    ///
    /// # Panics
    /// If the packet is not silent and `samples.len() != info.frames * channels`.
    pub fn push(&mut self, info: PacketInfo, samples: &[f32]) {
        let ch = usize::from(self.channels);
        assert!(
            info.silent || samples.len() == info.frames as usize * ch,
            "packet has {} frames of {ch} channels but {} samples were given",
            info.frames,
            samples.len()
        );
        // Exact placement (no jitter tolerance) for the first packet, relative to the
        // origin, and for the first packet after a discontinuity, relative to the end
        // of the previous packet.
        let exact_from = match self.expected_next_qpc {
            None => Some(self.origin_qpc),
            Some(expected) if self.reanchor => Some(expected),
            Some(_) => None,
        };
        let (silence, trim) = if info.timestamp_error {
            (0, 0)
        } else if let Some(reference) = exact_from {
            let qpc = info.qpc.min(self.limit_qpc.max(reference));
            if qpc >= reference {
                (qpc_to_frames(qpc - reference, self.sample_rate), 0)
            } else {
                let early = qpc_to_frames(reference - qpc, self.sample_rate);
                (0, early.min(u64::from(info.frames)))
            }
        } else {
            (self.gap_frames_to(info.qpc), 0)
        };
        let silence = self.insert_silence(silence);
        let track_frame = self.frames();
        let kept = info.frames as usize - trim as usize;
        if info.silent {
            self.samples.resize(self.samples.len() + kept * ch, 0.0);
        } else {
            self.samples
                .extend_from_slice(&samples[trim as usize * ch..]);
        }
        let duration = frames_to_qpc(u64::from(info.frames), self.sample_rate);
        if kept > 0 {
            self.reanchor = false;
            self.expected_next_qpc = Some(if info.timestamp_error {
                self.expected_next_qpc.unwrap_or(self.origin_qpc) + duration
            } else {
                info.qpc.min(self.limit_qpc).saturating_add(duration)
            });
        }
        self.packets.push(PacketRecord {
            qpc: info.qpc,
            device_position: info.device_position,
            frames: info.frames,
            track_frame,
            silence_before: silence,
            trimmed_frames: trim,
            silent: info.silent,
            discontinuity: info.discontinuity,
            timestamp_error: info.timestamp_error,
        });
    }

    /// Pads the track with silence up to `end_qpc` (the moment capture stopped).
    pub fn finish(&mut self, end_qpc: u64) {
        let silence = self.gap_frames_to(end_qpc);
        let silence = self.insert_silence(silence);
        if silence > 0 {
            self.expected_next_qpc = Some(end_qpc);
        }
    }

    /// Native sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// QPC position of frame 0 of the track: always the origin. The first placed
    /// packet is trimmed (stamped before the origin) or preceded by silence (after it).
    pub fn start_qpc(&self) -> u64 {
        self.origin_qpc
    }

    /// Track samples so far (interleaved when the track has several channels).
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Packet log.
    pub fn packets(&self) -> &[PacketRecord] {
        &self.packets
    }

    /// Total synthesized silence frames.
    pub fn synthesized_frames(&self) -> u64 {
        self.synthesized_frames
    }

    /// Number of silence insertions.
    pub fn silence_gaps(&self) -> u32 {
        self.silence_gaps
    }

    /// Consumes the timeline, returning samples and packet log.
    pub fn into_parts(self) -> (Vec<f32>, Vec<PacketRecord>) {
        (self.samples, self.packets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;
    /// 10 ms packet at 48 kHz.
    const PF: u32 = 480;
    const PQ: u64 = 100_000;
    const T0: u64 = 1_000_000_000;

    fn pkt(qpc: u64, frames: u32) -> PacketInfo {
        PacketInfo {
            qpc,
            device_position: 0,
            frames,
            ..Default::default()
        }
    }

    fn ones(n: u32) -> Vec<f32> {
        vec![1.0; n as usize]
    }

    #[test]
    fn contiguous_packets_need_no_silence() {
        let mut t = TrackTimeline::new(SR, T0);
        for i in 0..5 {
            t.push(pkt(T0 + i * PQ, PF), &ones(PF));
        }
        t.finish(T0 + 5 * PQ);
        assert_eq!(t.samples().len(), 5 * PF as usize);
        assert!(t.samples().iter().all(|&s| s == 1.0));
        assert_eq!(t.synthesized_frames(), 0);
        assert_eq!(t.silence_gaps(), 0);
        assert_eq!(t.start_qpc(), T0);
        assert_eq!(t.packets()[3].track_frame, 3 * u64::from(PF));
    }

    #[test]
    fn no_packets_at_all_becomes_full_silence() {
        // Loopback with nothing rendering: zero packets for 5 s.
        let mut t = TrackTimeline::new(SR, T0);
        t.finish(T0 + 5 * QPC_UNITS_PER_SEC);
        assert_eq!(t.samples().len(), 5 * SR as usize);
        assert!(t.samples().iter().all(|&s| s == 0.0));
        assert_eq!(t.synthesized_frames(), 5 * u64::from(SR));
        assert_eq!(t.silence_gaps(), 1);
        assert!(t.packets().is_empty());
    }

    #[test]
    fn mid_stream_gap_is_filled_by_timestamp() {
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0, PF), &ones(PF));
        // 250 ms with no packets, then audio resumes.
        let resume = T0 + PQ + 2_500_000;
        t.push(pkt(resume, PF), &ones(PF));
        t.finish(resume + PQ);
        let gap = 12_000usize; // 250 ms at 48 kHz
        assert_eq!(t.samples().len(), 2 * PF as usize + gap);
        assert!(
            t.samples()[PF as usize..PF as usize + gap]
                .iter()
                .all(|&s| s == 0.0)
        );
        assert_eq!(t.samples()[PF as usize + gap], 1.0);
        assert_eq!(t.packets()[1].silence_before, gap as u64);
        assert_eq!(t.packets()[1].track_frame, u64::from(PF) + gap as u64);
        assert_eq!(t.silence_gaps(), 1);
    }

    #[test]
    fn jitter_below_tolerance_is_not_filled() {
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0, PF), &ones(PF));
        t.push(pkt(T0 + PQ + 30_000, PF), &ones(PF)); // 3 ms late
        t.finish(T0 + 2 * PQ + 30_000);
        assert_eq!(t.samples().len(), 2 * PF as usize);
        assert_eq!(t.synthesized_frames(), 0);
    }

    #[test]
    fn device_clock_drift_does_not_accumulate_into_false_gaps() {
        // Device clock runs 200 ppm slow relative to QPC: each 10 ms packet is
        // stamped 2 us later than its frame count implies. Over 60 s that is 12 ms,
        // which must not be filled because each step is anchored on the previous packet.
        let mut t = TrackTimeline::new(SR, T0);
        for i in 0..6_000u64 {
            t.push(pkt(T0 + i * (PQ + 20), PF), &ones(PF));
        }
        assert_eq!(t.synthesized_frames(), 0);
        assert_eq!(t.samples().len(), 6_000 * PF as usize);
    }

    #[test]
    fn silent_flag_writes_zeros() {
        let mut t = TrackTimeline::new(SR, T0);
        let mut p = pkt(T0, PF);
        p.silent = true;
        t.push(p, &[]);
        assert_eq!(t.samples().len(), PF as usize);
        assert!(t.samples().iter().all(|&s| s == 0.0));
        assert!(t.packets()[0].silent);
        assert_eq!(t.synthesized_frames(), 0, "flagged silence is device data");
    }

    #[test]
    fn late_first_packet_gets_leading_silence() {
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0 + 1_000_000, PF), &ones(PF)); // first packet 100 ms after start
        assert_eq!(t.start_qpc(), T0);
        assert_eq!(t.packets()[0].silence_before, 4_800);
        assert_eq!(t.samples().len(), 4_800 + PF as usize);
    }

    #[test]
    fn early_first_packet_is_trimmed_to_origin() {
        // First packet stamped 5 ms (240 frames) before the origin: its first 240
        // frames predate frame 0 and are dropped.
        let mut t = TrackTimeline::new(SR, T0);
        let ramp: Vec<f32> = (0..PF).map(|k| k as f32).collect();
        t.push(pkt(T0 - 50_000, PF), &ramp);
        assert_eq!(t.start_qpc(), T0);
        assert_eq!(t.samples().len(), 240);
        assert_eq!(t.samples()[0], 240.0);
        assert_eq!(t.synthesized_frames(), 0);
        let p = &t.packets()[0];
        assert_eq!(
            (p.trimmed_frames, p.track_frame, p.silence_before),
            (240, 0, 0)
        );
        // The next packet continues seamlessly.
        t.push(pkt(T0 - 50_000 + PQ, PF), &ones(PF));
        assert_eq!(t.synthesized_frames(), 0);
        assert_eq!(t.samples().len(), 240 + PF as usize);
        assert_eq!(t.packets()[1].trimmed_frames, 0);
    }

    #[test]
    fn packet_entirely_before_origin_is_dropped_and_next_one_anchors() {
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0 - 200_000, PF), &ones(PF)); // 20 ms before start, 10 ms long
        assert!(t.samples().is_empty());
        assert_eq!(t.packets()[0].trimmed_frames, u64::from(PF));
        // Next packet straddles the origin by 2 ms (96 frames).
        let ramp: Vec<f32> = (0..PF).map(|k| k as f32).collect();
        t.push(pkt(T0 - 20_000, PF), &ramp);
        assert_eq!(t.samples().len(), (PF - 96) as usize);
        assert_eq!(t.samples()[0], 96.0);
        assert_eq!(t.packets()[1].trimmed_frames, 96);
        assert_eq!(t.synthesized_frames(), 0);
        assert_eq!(t.start_qpc(), T0);
    }

    #[test]
    fn first_packet_slightly_late_is_padded_exactly() {
        // 3 ms is below the mid-stream jitter tolerance, but frame 0 must still sit
        // at the origin, so the lead-in is padded (144 frames).
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0 + 30_000, PF), &ones(PF));
        assert_eq!(t.start_qpc(), T0);
        assert_eq!(t.packets()[0].silence_before, 144);
        assert_eq!(t.samples().len(), 144 + PF as usize);
        assert_eq!(t.samples()[143], 0.0);
        assert_eq!(t.samples()[144], 1.0);
    }

    #[test]
    fn silent_early_packet_is_trimmed_too() {
        let mut t = TrackTimeline::new(SR, T0);
        let mut p = pkt(T0 - 50_000, PF);
        p.silent = true;
        t.push(p, &[]);
        assert_eq!(t.samples().len(), 240);
        assert_eq!(t.packets()[0].trimmed_frames, 240);
    }

    #[test]
    fn timestamp_error_packets_are_not_gap_filled() {
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0, PF), &ones(PF));
        let mut bad = pkt(T0 + 50 * PQ, PF);
        bad.timestamp_error = true;
        t.push(bad, &ones(PF));
        // Next good packet continues from where the bad one was assumed to end.
        t.push(pkt(T0 + 2 * PQ, PF), &ones(PF));
        assert_eq!(t.synthesized_frames(), 0);
        assert_eq!(t.samples().len(), 3 * PF as usize);
        assert!(t.packets()[1].timestamp_error);
    }

    #[test]
    fn finish_pads_tail_and_ignores_small_remainder() {
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0, PF), &ones(PF));
        t.finish(T0 + PQ + 10_000); // 1 ms tail: below tolerance
        assert_eq!(t.samples().len(), PF as usize);
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0, PF), &ones(PF));
        t.finish(T0 + PQ + QPC_UNITS_PER_SEC); // loopback stopped 1 s before the end
        assert_eq!(t.samples().len(), PF as usize + SR as usize);
    }

    #[test]
    #[should_panic(expected = "frames")]
    fn frame_count_mismatch_panics() {
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0, PF), &ones(PF - 1));
    }

    #[test]
    fn bogus_high_qpc_mid_stream_is_clamped_to_limit() {
        let mut t = TrackTimeline::new(SR, T0);
        t.set_qpc_limit(T0 + QPC_UNITS_PER_SEC);
        t.push(pkt(T0, PF), &ones(PF));
        // A garbage timestamp years in the future must not synthesize years of silence.
        t.push(pkt(u64::MAX / 4, PF), &ones(PF));
        let max = SR as usize + 2 * PF as usize;
        assert!(t.samples().len() <= max, "{} > {max}", t.samples().len());
        // Normal packets after the bogus one do not grow the track unboundedly either.
        t.push(pkt(T0 + 2 * PQ, PF), &ones(PF));
        t.finish(T0 + QPC_UNITS_PER_SEC);
        assert!(t.samples().len() <= max + PF as usize);
    }

    #[test]
    fn bogus_high_qpc_on_first_packet_is_clamped_to_limit() {
        let mut t = TrackTimeline::new(SR, T0);
        t.set_qpc_limit(T0 + QPC_UNITS_PER_SEC);
        t.push(pkt(u64::MAX - 1, PF), &ones(PF));
        t.finish(u64::MAX);
        assert!(t.samples().len() <= SR as usize + PF as usize);
    }

    #[test]
    fn limit_does_not_change_normal_placement() {
        let mut t = TrackTimeline::new(SR, T0);
        t.set_qpc_limit(T0 + QPC_UNITS_PER_SEC);
        t.push(pkt(T0 + 1_000_000, PF), &ones(PF));
        t.finish(T0 + QPC_UNITS_PER_SEC);
        assert_eq!(t.packets()[0].silence_before, 4_800);
        assert_eq!(t.samples().len(), SR as usize);
    }

    #[test]
    fn discontinuity_reanchors_next_packet_exactly() {
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0, PF), &ones(PF));
        // Device switch: the old stream is closed at the end of packet 0.
        let frame = t.mark_discontinuity();
        assert_eq!(frame, u64::from(PF));
        // New device starts 3 ms later: below the jitter tolerance, but after a
        // discontinuity the gap is filled exactly (144 frames).
        t.push(pkt(T0 + PQ + 30_000, PF), &ones(PF));
        assert_eq!(t.packets()[1].silence_before, 144);
        assert_eq!(t.packets()[1].track_frame, u64::from(PF) + 144);
        // Afterwards the normal tolerance applies again.
        t.push(pkt(T0 + 2 * PQ + 30_000 + 20_000, PF), &ones(PF));
        assert_eq!(t.packets()[2].silence_before, 0);
    }

    #[test]
    fn discontinuity_trims_overlap_of_new_device() {
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0, PF), &ones(PF));
        t.mark_discontinuity();
        // New device's first packet starts 5 ms before the old one ended: 240 frames
        // of overlap are dropped so the track keeps wall-clock alignment.
        let ramp: Vec<f32> = (0..PF).map(|k| k as f32).collect();
        t.push(pkt(T0 + PQ - 50_000, PF), &ramp);
        assert_eq!(t.packets()[1].trimmed_frames, 240);
        assert_eq!(t.packets()[1].silence_before, 0);
        assert_eq!(t.samples().len(), 2 * PF as usize - 240);
        assert_eq!(t.samples()[PF as usize], 240.0);
    }

    #[test]
    fn discontinuity_before_any_packet_keeps_origin_anchor() {
        let mut t = TrackTimeline::new(SR, T0);
        assert_eq!(t.mark_discontinuity(), 0);
        t.push(pkt(T0 + 1_000_000, PF), &ones(PF));
        assert_eq!(t.packets()[0].silence_before, 4_800);
    }

    #[test]
    fn long_device_loss_is_filled_up_to_the_new_device() {
        let mut t = TrackTimeline::new(SR, T0);
        t.push(pkt(T0, PF), &ones(PF));
        t.mark_discontinuity();
        // No device for 2 s.
        t.push(pkt(T0 + PQ + 2 * QPC_UNITS_PER_SEC, PF), &ones(PF));
        assert_eq!(t.packets()[1].silence_before, 2 * u64::from(SR));
        assert_eq!(t.synthesized_frames(), 2 * u64::from(SR));
    }

    /// `frames` stereo frames: left `offset + k`, right `-(offset + k)`.
    fn stereo(frames: u32, offset: f32) -> Vec<f32> {
        (0..frames)
            .flat_map(|k| [offset + k as f32, -(offset + k as f32)])
            .collect()
    }

    #[test]
    fn stereo_track_is_interleaved_and_placed_in_frames() {
        let mut t = TrackTimeline::new(SR, T0).with_channels(2);
        assert_eq!((t.channels(), t.frames()), (2, 0));
        t.push(pkt(T0, PF), &stereo(PF, 0.0));
        // 250 ms (12000 frames) without packets, then audio resumes.
        let resume = T0 + PQ + 2_500_000;
        t.push(pkt(resume, PF), &stereo(PF, 1_000.0));
        t.finish(resume + PQ + QPC_UNITS_PER_SEC);
        let gap = 12_000u64;
        let pf = u64::from(PF);
        assert_eq!(t.packets()[1].silence_before, gap, "silence in frames");
        assert_eq!(t.packets()[1].track_frame, pf + gap, "positions in frames");
        assert_eq!(t.frames(), 2 * pf + gap + u64::from(SR));
        assert_eq!(t.samples().len() as u64, 2 * t.frames());
        assert_eq!(t.synthesized_frames(), gap + u64::from(SR));
        assert_eq!(&t.samples()[..2 * PF as usize], &stereo(PF, 0.0)[..]);
        let at = 2 * (pf + gap) as usize;
        assert!(t.samples()[2 * PF as usize..at].iter().all(|&s| s == 0.0));
        assert_eq!(
            &t.samples()[at..at + 2 * PF as usize],
            &stereo(PF, 1_000.0)[..]
        );
    }

    #[test]
    fn stereo_first_packet_is_trimmed_by_whole_frames() {
        let mut t = TrackTimeline::new(SR, T0).with_channels(2);
        // 5 ms (240 frames) before the origin.
        t.push(pkt(T0 - 50_000, PF), &stereo(PF, 0.0));
        assert_eq!(t.packets()[0].trimmed_frames, 240);
        assert_eq!(t.frames(), 240);
        assert_eq!(t.samples()[..2], [240.0, -240.0]);
        let mut p = pkt(T0 - 50_000 + PQ, PF);
        p.silent = true;
        t.push(p, &[]);
        assert_eq!(t.frames(), 240 + u64::from(PF));
        assert!(t.samples()[480..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn stereo_padding_respects_the_qpc_limit_in_frames() {
        let mut t = TrackTimeline::new(SR, T0).with_channels(2);
        t.set_qpc_limit(T0 + QPC_UNITS_PER_SEC);
        t.push(pkt(u64::MAX / 4, PF), &stereo(PF, 0.0));
        t.finish(u64::MAX);
        assert!(t.frames() <= u64::from(SR + PF), "{}", t.frames());
        assert_eq!(t.samples().len() as u64, 2 * t.frames());
    }

    #[test]
    #[should_panic(expected = "samples")]
    fn stereo_packet_with_mono_samples_panics() {
        let mut t = TrackTimeline::new(SR, T0).with_channels(2);
        t.push(pkt(T0, PF), &ones(PF));
    }

    #[test]
    fn conversions_round_trip() {
        assert_eq!(frames_to_qpc(480, 48_000), 100_000);
        assert_eq!(qpc_to_frames(100_000, 48_000), 480);
        assert_eq!(frames_to_qpc(441, 44_100), 100_000);
    }
}
