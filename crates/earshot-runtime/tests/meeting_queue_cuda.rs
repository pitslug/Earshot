//! T9.1l: a meeting started during a hung CUDA trial records from the start and ends on what the
//! start decides. Its own binary, because the choice is one per process. In a build with the
//! provider the meeting ends on CUDA:
//! `cargo test -p earshot-runtime --features onnx-cuda --test meeting_queue_cuda`
//! Without the provider nothing can choose CUDA, so that build proves everything else: the meeting
//! records from the start and reads its order only after the decision.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use earshot_core::audio::{MonoBuffer, SampleRate};
use earshot_core::capture::{
    AudioSource, CaptureSession, CaptureSink, DeviceTime, Discontinuity, EndOfStream, EndReason,
    ReplayBuilder, ReplaySource, SourceStatus, TrackId,
};
use earshot_core::identity::ManualClock;
use earshot_core::model::{AudioFileRepo, ClockDomain};
use earshot_runtime::Services;
use earshot_runtime::backends::Accelerator;
use earshot_runtime::cuda::{hold_onnx, onnx_held};
use earshot_runtime::host::settled_order;
use earshot_runtime::live::{HoldReason, LiveConfig, LiveHold, LiveTranscriptState, start_live};
use earshot_runtime::meeting_queue::{MeetingHold, MeetingQueue};
use earshot_store::{DataDir, Store, read_flac};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;

const GUARD: Duration = Duration::from_secs(120);

/// A replay whose end waits until it is released: a meeting going on while the trial hangs.
struct UntilReleased {
    inner: ReplaySource,
    released: Arc<AtomicBool>,
    delivered: Arc<AtomicBool>,
    end: Option<EndOfStream>,
}

struct KeepEnd<'a>(&'a mut dyn CaptureSink, &'a mut Option<EndOfStream>);

impl CaptureSink for KeepEnd<'_> {
    fn frames(&mut self, track: TrackId, frames: &[f32], device_ts: DeviceTime) {
        self.0.frames(track, frames, device_ts);
    }
    fn discontinuity(&mut self, track: TrackId, d: Discontinuity) {
        self.0.discontinuity(track, d);
    }
    fn end(&mut self, end: EndOfStream) {
        *self.1 = Some(end);
    }
}

impl AudioSource for UntilReleased {
    type Error = <ReplaySource as AudioSource>::Error;
    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }
    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        if self.end.is_none() {
            let status = self.inner.pump(&mut KeepEnd(sink, &mut self.end))?;
            if self.end.is_none() {
                return Ok(status);
            }
            self.delivered.store(true, Ordering::SeqCst);
        }
        if self.released.load(Ordering::SeqCst)
            && let Some(end) = self.end.take()
        {
            sink.end(end);
            return Ok(SourceStatus::Ended);
        }
        Ok(SourceStatus::Idle)
    }
    fn stop(&mut self) {
        self.released.store(true, Ordering::SeqCst);
        if let Some(end) = &mut self.end {
            end.reason = EndReason::Stopped;
        }
        self.inner.stop();
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("earshot-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for(what: &str, ok: impl Fn() -> bool) {
    let limit = Instant::now() + GUARD;
    while !ok() {
        assert!(Instant::now() < limit, "{what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(all(feature = "onnx-cuda", not(feature = "cuda")))]
mod provider {
    use earshot_runtime::backends::ModelDir;
    use earshot_runtime::catalogue::Catalogue;
    use earshot_runtime::cuda::{CudaPart, folder, use_for_transcription_in};

    fn bytes_of(name: &str) -> Vec<u8> {
        format!("the bytes of {name}").into_bytes()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn test_manifest() -> Catalogue {
        let mut text = "sets = [\"s\"]\nhosts = [\"example.org\"]\n\n".to_owned();
        for (i, (id, part)) in [("a", "summaries"), ("b", "transcription")]
            .into_iter()
            .enumerate()
        {
            text.push_str(&format!(
                "[[artefact]]\nid = \"{id}\"\nsets = [\"s\"]\nlocation = \"cache\"\n\
                 url = \"https://example.org/{id}\"\npath = \"archives/{id}.zip\"\n\
                 sha256 = \"{}\"\nsize = 1\nlicence = \"x\"\nattribution = \"x\"\n\n",
                "0".repeat(64)
            ));
            text.push_str(&format!(
                "[[cuda_archive]]\npart = \"{part}\"\nartefact = \"{id}\"\nformat = \"zip\"\n\n"
            ));
            for name in CudaPart::ALL[i].required_files() {
                let bytes = bytes_of(name);
                text.push_str(&format!(
                    "[[cuda_archive.file]]\nname = \"{name}\"\nmember = \"bin/{name}\"\n\
                     sha256 = \"{}\"\nsize = {}\n\n",
                    sha256_hex(&bytes),
                    bytes.len()
                ));
            }
        }
        Catalogue::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    /// What the start does when its trial ends well: chooses the verified folder.
    pub fn choose_cuda(root: &std::path::Path) {
        let models = ModelDir::new(root);
        let dir = folder(&models);
        std::fs::create_dir_all(&dir).unwrap();
        for part in CudaPart::ALL {
            for name in part.required_files() {
                std::fs::write(dir.join(name), bytes_of(name)).unwrap();
            }
        }
        use_for_transcription_in(&test_manifest(), &models, true).unwrap();
    }
}

#[test]
fn meeting_queue_a_meeting_during_a_hung_cuda_trial_records_from_the_start_and_ends_on_its_choice()
{
    let root = temp_dir("meeting-queue-cuda");
    let data = DataDir::new(root.join("data"));
    data.create().unwrap();
    let svc = Services::new(
        Store::open(&data.db_path()).unwrap(),
        data,
        Arc::new(ManualClock::new(1_760_000_000_000)),
    )
    .unwrap();

    // The start is working, and stays so until the meeting has recorded everything: a hung trial.
    let hold = hold_onnx();
    assert!(onnx_held());

    let audio: Vec<f32> = (0..16_000 * 4)
        .map(|i| 0.2 * (i as f32 * 0.07).sin())
        .collect();
    let (released, delivered) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let source = UntilReleased {
        inner: ReplayBuilder::new(
            DeviceTime::from_ticks(5_000_000),
            MonoBuffer::new(SampleRate::HZ_16000, audio.clone()),
        )
        .build()
        .unwrap(),
        released: Arc::clone(&released),
        delivered: Arc::clone(&delivered),
        end: None,
    };

    // What the meeting's load reads, once its gates open: the settled order. The fake transcribes
    // nothing: what ran is not the point here, what it was told to run on is.
    let order: Arc<std::sync::Mutex<Option<Vec<Accelerator>>>> = Arc::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let factory = {
        let (order, calls) = (Arc::clone(&order), Arc::clone(&calls));
        Box::new(move |_: TrackId, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            *order.lock().unwrap() = Some(settled_order());
            Ok(None)
        })
    };
    let queue = MeetingQueue::new();
    let mut session = start_live(
        source,
        svc.clone(),
        LiveConfig {
            clock_domain: ClockDomain::Replay,
            idle_wait: Duration::from_millis(1),
            hold: Some(LiveHold::new(Arc::new(MeetingHold::new(Arc::new(
                queue.enter(),
            ))))),
            ..LiveConfig::default()
        },
        Some(factory),
        Box::new(ChaCha8Rng::seed_from_u64(1)),
    )
    .unwrap();
    let meeting = session.meeting();
    let events = session.take_events().unwrap();

    // It records from the start, all the way, with the trial still hung.
    wait_for("the audio was delivered", || {
        delivered.load(Ordering::SeqCst)
    });
    assert!(onnx_held(), "the trial is still hung");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no model before the decision"
    );

    // The trial ends and the start decides.
    #[cfg(all(feature = "onnx-cuda", not(feature = "cuda")))]
    provider::choose_cuda(&root.join("models"));
    drop(hold);
    wait_for("the meeting's load ran", || {
        calls.load(Ordering::SeqCst) > 0
    });
    released.store(true, Ordering::SeqCst);

    // It said what it waited for.
    let mut states = Vec::new();
    loop {
        match events.recv_timeout(GUARD) {
            Ok(earshot_runtime::live::RuntimeEvent::LiveTranscript(s)) => states.push(s),
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("the session never ended"),
        }
    }
    let outcome = session.wait().unwrap();
    assert_eq!(
        states.first(),
        Some(&LiveTranscriptState::Held(HoldReason::Starting)),
        "{states:?}"
    );

    // Every frame is in the recording.
    let files = svc.store().audio_files(meeting).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(outcome.audio_files.len(), 1);
    let (_, flac) = read_flac(&svc.data.resolve(&files[0].path).unwrap()).unwrap();
    assert_eq!(flac.len(), audio.len());

    // And it runs on what the start chose.
    let seen = order.lock().unwrap().clone().unwrap();
    #[cfg(all(feature = "onnx-cuda", not(feature = "cuda")))]
    {
        assert_eq!(seen.first(), Some(&Accelerator::Cuda), "{seen:?}");
        assert!(earshot_runtime::cuda::transcription_from_folder());
    }
    #[cfg(not(any(feature = "onnx-cuda", feature = "cuda")))]
    assert!(!seen.contains(&Accelerator::Cuda), "{seen:?}");
    let _ = seen;
    drop(svc);
    let _ = std::fs::remove_dir_all(&root);
}
