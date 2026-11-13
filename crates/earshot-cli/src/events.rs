//! `earshot-cli replay --events-json <path>`: the session's events as timestamped JSON lines,
//! for timing harnesses (`cargo xtask t613`, T6.13).
//!
//! Each line is one JSON object with `t_ms`, the wall time in milliseconds since the log
//! was created (monotonic clock), and `kind`. Lines are written and flushed as the events
//! arrive, so a run that dies keeps everything before it. With `--realtime` the line
//! `audio_origin` marks the wall time at which the replay started delivering audio: the
//! audio at meeting time `t` was delivered at `audio_origin + t`.
//!
//! Kinds: `started`, `audio_origin`, `live_backend` (`track`, `backend`, `device`,
//! `model`), `final` (`track`, `start_ms`, `end_ms`, `words`: a live final as it was stored
//! and shown), `stats` (`end_ms`, `mic_backlog_s`, `reference_backlog_s`, at the printed
//! cadence), `live_partials`, `discontinuity`, `warning` (`text`), `ended` (`end_ms`),
//! `post_start`, `post_stage` (`stage`: `transcribed`, `diarised`, `echo_checked` or
//! `stored`, with `track` and `elapsed_ms`), `post_done` (`final_segments`, `speakers`,
//! `backend`), `transcript_lag` (`track`, `behind_ms`: written only when a track's live
//! transcript lag reaches a new largest value, so the last line per track is the run's
//! largest), `live_summaries_off` (`reason`), `live_summary_mapped` (`window`, `took_ms`,
//! `error`). The log never holds transcript text: finals carry their times and word
//! count only (the GATE-12 recordings are private, AGENTS.md).

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use earshot_core::asr::TranscriptEvent;
use earshot_core::audio::PIPELINE_RATE;
use earshot_core::capture::TrackId;
use earshot_core::pipeline::PipelineStats;
use earshot_runtime::live::RuntimeEvent;
use earshot_runtime::post::PostStage;
use serde_json::{Map, Value, json};

/// The JSON-lines event log.
pub struct EventLog {
    created: Instant,
    out: Mutex<Box<dyn Write + Send>>,
    /// The largest transcript lag logged so far per track (mic, reference), in ms.
    lag_max: Mutex<[u64; 2]>,
}

fn track(t: TrackId) -> &'static str {
    match t {
        TrackId::Mic => "mic",
        TrackId::Reference => "reference",
    }
}

impl EventLog {
    /// A log written to `path` (created or truncated).
    ///
    /// # Errors
    /// The file cannot be created.
    pub fn create(path: &Path) -> Result<Self> {
        let file =
            File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
        Ok(Self::to_writer(Box::new(BufWriter::new(file))))
    }

    /// A log written to `out`.
    #[must_use]
    pub fn to_writer(out: Box<dyn Write + Send>) -> Self {
        Self {
            created: Instant::now(),
            out: Mutex::new(out),
            lag_max: Mutex::new([0, 0]),
        }
    }

    /// Writes one line of `kind` with `fields`; a write error is ignored (the log is a
    /// diagnostic and never stops the meeting).
    fn line(&self, kind: &str, fields: Value) {
        // The time is taken under the lock, so lines from several threads (the session's
        // `audio_origin`, the host's events) are written in time order.
        let mut out = self
            .out
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut obj = Map::new();
        let t = self.created.elapsed().as_secs_f64() * 1_000.0;
        obj.insert("t_ms".into(), json!((t * 1_000.0).round() / 1_000.0));
        obj.insert("kind".into(), json!(kind));
        if let Value::Object(f) = fields {
            obj.extend(f);
        }
        let _ = writeln!(out, "{}", Value::Object(obj));
        let _ = out.flush();
    }

    /// The replay started delivering audio (meeting time zero) now.
    pub fn audio_origin(&self) {
        self.line("audio_origin", json!({}));
    }

    /// A runtime event (level meters and partials are not logged; stats go through
    /// [`EventLog::stats`]).
    pub fn event(&self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::Started { .. } => self.line("started", json!({})),
            RuntimeEvent::LiveBackend {
                track: t, backend, ..
            } => self.line(
                "live_backend",
                json!({
                    "track": track(*t),
                    "backend": format!("{:?}", backend.kind),
                    "device": backend.device,
                    "model": backend.model,
                }),
            ),
            RuntimeEvent::Transcript {
                track: t,
                event: TranscriptEvent::Final(seg),
            } => self.line(
                "final",
                json!({
                    "track": track(*t),
                    "start_ms": seg.start.as_millis(),
                    "end_ms": seg.end.as_millis(),
                    "words": seg.words.len(),
                }),
            ),
            RuntimeEvent::LivePartials {
                track: t,
                enabled,
                behind,
            } => self.line(
                "live_partials",
                json!({
                    "track": track(*t),
                    "enabled": enabled,
                    "behind_s": behind.as_secs_f64(),
                }),
            ),
            RuntimeEvent::Discontinuity { track: t, at, kind } => self.line(
                "discontinuity",
                json!({
                    "track": track(*t),
                    "at_ms": at.as_millis(),
                    "what": format!("{kind:?}"),
                }),
            ),
            RuntimeEvent::Warning(w) => self.line(
                "warning",
                json!({ "text": crate::session::format_warning(w) }),
            ),
            RuntimeEvent::LiveSpeakersOff { reason } => {
                self.line("live_speakers_off", json!({ "reason": reason }));
            }
            RuntimeEvent::LiveSummariesOff { reason } => {
                self.line("live_summaries_off", json!({ "reason": reason }));
            }
            RuntimeEvent::LiveTranscriptFinished { .. } => {
                self.line("live_transcript_finished", json!({}));
            }
            RuntimeEvent::RecordingSaved { .. } => self.line("recording_saved", json!({})),
            RuntimeEvent::LiveTranscript(state) => {
                self.line("live_transcript", json!({ "state": format!("{state:?}") }));
            }
            RuntimeEvent::LiveSummaryMapped {
                window,
                took,
                error,
            } => self.line(
                "live_summary_mapped",
                json!({
                    "window": window,
                    "took_ms": u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
                    "error": error,
                }),
            ),
            RuntimeEvent::TranscriptLag { track: t, behind } => {
                let ms = u64::try_from(behind.as_millis()).unwrap_or(u64::MAX);
                let i = usize::from(*t == TrackId::Reference);
                let new_max = {
                    let mut max = self
                        .lag_max
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let new = ms > max[i];
                    max[i] = max[i].max(ms);
                    new
                };
                if new_max {
                    self.line(
                        "transcript_lag",
                        json!({ "track": track(*t), "behind_ms": ms }),
                    );
                }
            }
            RuntimeEvent::Ended(o) => self.line("ended", json!({ "end_ms": o.end.as_millis() })),
            // Provisional text is never logged, here or anywhere; the CLI runs no previews.
            RuntimeEvent::LivePreview { .. }
            | RuntimeEvent::LivePreviewState { .. }
            | RuntimeEvent::LivePreviewReport(_) => {}
            RuntimeEvent::Transcript { .. }
            | RuntimeEvent::Levels(_)
            | RuntimeEvent::VoiceActivity { .. }
            | RuntimeEvent::Stats(_)
            | RuntimeEvent::Resynced { .. } => {}
        }
    }

    /// A stats snapshot (logged at the printed cadence).
    pub fn stats(&self, stats: &PipelineStats) {
        let secs = |samples: u64| samples as f64 / f64::from(PIPELINE_RATE.hz());
        self.line(
            "stats",
            json!({
                "end_ms": stats.end.as_millis(),
                "mic_backlog_s": secs(stats.mic.asr_backlog),
                "reference_backlog_s": stats.reference.as_ref().map(|r| secs(r.asr_backlog)),
            }),
        );
    }

    /// The cleanup started.
    pub fn post_start(&self) {
        self.line("post_start", json!({}));
    }

    /// A cleanup step finished.
    pub fn stage(&self, stage: &PostStage) {
        let ms = |d: &std::time::Duration| d.as_secs_f64() * 1_000.0;
        let fields = match stage {
            PostStage::Transcribed { track: t, elapsed } => {
                json!({ "stage": "transcribed", "track": track(*t), "elapsed_ms": ms(elapsed) })
            }
            PostStage::Diarised {
                track: t,
                speakers,
                elapsed,
            } => json!({
                "stage": "diarised",
                "track": track(*t),
                "speakers": speakers,
                "elapsed_ms": ms(elapsed),
            }),
            PostStage::EchoChecked { elapsed } => {
                json!({ "stage": "echo_checked", "elapsed_ms": ms(elapsed) })
            }
            PostStage::Stored { elapsed } => {
                json!({ "stage": "stored", "elapsed_ms": ms(elapsed) })
            }
        };
        self.line("post_stage", fields);
    }

    /// The cleanup finished: what it stored and the backend that ran it.
    pub fn post_done(&self, final_segments: usize, speakers: usize, backend: &str) {
        self.line(
            "post_done",
            json!({
                "final_segments": final_segments,
                "speakers": speakers,
                "backend": backend,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use earshot_core::asr::{MeetingTime, Segment, Word};
    use earshot_core::identity::EntityId;
    use std::sync::Arc;

    /// A writer whose bytes the test can read.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn lines(s: &Shared) -> Vec<Value> {
        String::from_utf8(s.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn word(text: &str, start: u64, end: u64) -> Word {
        Word {
            text: text.into(),
            start: MeetingTime::from_millis(start),
            end: MeetingTime::from_millis(end),
            probability: 0.9,
        }
    }

    #[test]
    fn events_log_each_new_largest_transcript_lag_per_track() {
        let buf = Shared::default();
        let log = EventLog::to_writer(Box::new(buf.clone()));
        let lag = |t, ms| RuntimeEvent::TranscriptLag {
            track: t,
            behind: std::time::Duration::from_millis(ms),
        };
        for e in [
            lag(TrackId::Mic, 0),
            lag(TrackId::Mic, 400),
            lag(TrackId::Reference, 300),
            lag(TrackId::Mic, 200),
            lag(TrackId::Mic, 900),
            lag(TrackId::Reference, 300),
        ] {
            log.event(&e);
        }
        let got: Vec<(String, u64)> = lines(&buf)
            .iter()
            .map(|v| {
                assert_eq!(v["kind"], "transcript_lag");
                (
                    v["track"].as_str().unwrap().to_owned(),
                    v["behind_ms"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("mic".to_owned(), 400),
                ("reference".to_owned(), 300),
                ("mic".to_owned(), 900),
            ]
        );
    }

    #[test]
    fn events_lines_from_several_threads_stay_in_time_order() {
        let buf = Shared::default();
        let log = Arc::new(EventLog::to_writer(Box::new(buf.clone())));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        log.audio_origin();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let t: Vec<f64> = lines(&buf)
            .iter()
            .map(|v| v["t_ms"].as_f64().unwrap())
            .collect();
        assert_eq!(t.len(), 2_000);
        assert!(
            t.windows(2).all(|w| w[0] <= w[1]),
            "lines out of time order"
        );
    }

    #[test]
    fn events_log_finals_with_times_and_counts_but_never_text() {
        let buf = Shared::default();
        let log = EventLog::to_writer(Box::new(buf.clone()));
        log.audio_origin();
        let seg = Segment::from_words(
            EntityId::from_bytes([1; 16]),
            EntityId::from_bytes([2; 16]),
            vec![
                word("secret", 1_200, 1_700),
                word("words", 1_800, 2_500),
                word("here", 2_600, 3_400),
            ],
            MeetingTime::ZERO,
        );
        log.event(&RuntimeEvent::Transcript {
            track: TrackId::Mic,
            event: TranscriptEvent::Final(seg),
        });
        log.stage(&PostStage::Stored {
            elapsed: std::time::Duration::from_millis(250),
        });
        log.post_done(7, 3, "Cuda");
        let raw = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(!raw.contains("secret"), "no transcript text: {raw}");
        let l = lines(&buf);
        assert_eq!(l.len(), 4);
        assert_eq!(l[0]["kind"], "audio_origin");
        assert_eq!(l[1]["kind"], "final");
        assert_eq!(l[1]["track"], "mic");
        assert_eq!(l[1]["start_ms"], 1_200);
        assert_eq!(l[1]["end_ms"], 3_400);
        assert_eq!(l[1]["words"], 3);
        assert_eq!(l[2]["kind"], "post_stage");
        assert_eq!(l[2]["stage"], "stored");
        assert_eq!(l[2]["elapsed_ms"], 250.0);
        assert_eq!(l[3]["final_segments"], 7);
        // Wall times never go backwards.
        let t: Vec<f64> = l.iter().map(|v| v["t_ms"].as_f64().unwrap()).collect();
        assert!(t.windows(2).all(|w| w[0] <= w[1]), "{t:?}");
    }
}
