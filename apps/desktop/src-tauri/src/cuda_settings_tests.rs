//! The CUDA section over fake hardware, fake files and a temp store. Nothing here touches a real
//! download, a real DLL or llama.cpp.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;

use earshot_runtime::cuda_switch::{NOT_AVAILABLE_YET, PartFiles};
use earshot_runtime::earshot_hardware::{ComputeCapability, Gpu, NVIDIA_VENDOR_ID};

use super::*;
use crate::model_jobs::{CheckDone, CheckStep, DownloadEnd, ModelSink, ModelWork};
use crate::testutil::{TempDir, services};

const VULKAN: &[Accelerator] = &[Accelerator::Vulkan, Accelerator::Cpu];

fn rtx(cap: (u32, u32)) -> Gpu {
    Gpu {
        name: "NVIDIA GeForce RTX 5080".into(),
        vendor_id: NVIDIA_VENDOR_ID,
        dedicated_memory: Some(16 << 30),
        driver_version: Some("32.0.16.1714".into()),
        integrated: Some(false),
        compute_capability: Some(ComputeCapability {
            major: cap.0,
            minor: cap.1,
        }),
    }
}

fn desktop() -> Hardware {
    Hardware {
        gpus: vec![rtx((12, 0))],
        ..Hardware::default()
    }
}

fn no_nvidia() -> Hardware {
    Hardware {
        gpus: vec![Gpu {
            name: "Qualcomm(R) Adreno(TM) X2-90 GPU".into(),
            vendor_id: 0x4d4f_4351,
            dedicated_memory: Some(512 << 20),
            driver_version: Some("31.0.112.0".into()),
            integrated: Some(true),
            compute_capability: None,
        }],
        ..Hardware::default()
    }
}

/// The fake of files that cannot be had (the state of a manifest that does not pin a part).
#[derive(Debug, Clone, Copy, Default)]
struct NoCudaFilesYet;

impl CudaFiles for NoCudaFilesYet {
    fn part_files(&self, _: &ModelDir, _: CudaPart) -> PartFiles {
        PartFiles {
            not_available: Some(NOT_AVAILABLE_YET.to_owned()),
            download_size: 100,
            disk_size: 200,
            held: 0,
            state: PartState::Missing,
        }
    }

    fn download(
        &self,
        _: &ModelDir,
        _: CudaPart,
        _: &CancelToken,
        _: &mut dyn FnMut(u64, u64),
    ) -> Result<(), CudaFilesError> {
        Err(CudaFilesError::Failed {
            message: NOT_AVAILABLE_YET.to_owned(),
            retryable: false,
        })
    }

    fn remove(&self, _: &ModelDir, _: CudaPart) -> Result<PartRemoved, CudaFilesError> {
        Ok(PartRemoved::default())
    }

    fn verify(&self, _: &ModelDir, _: CudaPart) -> Result<(), CudaFilesError> {
        Err(CudaFilesError::Failed {
            message: NOT_AVAILABLE_YET.to_owned(),
            retryable: false,
        })
    }
}

/// Files with fixed states that count every call.
struct FakeFiles {
    summaries: PartState,
    transcription: PartState,
    not_available: Option<String>,
    verify_fails: bool,
    download_ends: Mutex<Result<(), CudaFilesError>>,
    download_wait: Mutex<Option<mpsc::Receiver<()>>>,
    downloads: AtomicU32,
    removes: AtomicU32,
    verifies: AtomicU32,
}

impl FakeFiles {
    fn new(summaries: PartState, transcription: PartState) -> Self {
        Self {
            summaries,
            transcription,
            not_available: None,
            verify_fails: false,
            download_ends: Mutex::new(Ok(())),
            download_wait: Mutex::new(None),
            downloads: AtomicU32::new(0),
            removes: AtomicU32::new(0),
            verifies: AtomicU32::new(0),
        }
    }
}

impl CudaFiles for FakeFiles {
    fn part_files(&self, _: &ModelDir, part: CudaPart) -> PartFiles {
        PartFiles {
            not_available: self.not_available.clone(),
            download_size: 100,
            disk_size: 200,
            held: 0,
            state: match part {
                CudaPart::Summaries => self.summaries,
                CudaPart::Transcription => self.transcription,
            },
        }
    }

    fn download(
        &self,
        _: &ModelDir,
        _: CudaPart,
        _: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), CudaFilesError> {
        self.downloads.fetch_add(1, Ordering::SeqCst);
        progress(50, 100);
        if let Some(wait) = self.download_wait.lock().unwrap().take() {
            let _ = wait.recv();
        }
        self.download_ends.lock().unwrap().clone()
    }

    fn remove(&self, _: &ModelDir, _: CudaPart) -> Result<PartRemoved, CudaFilesError> {
        self.removes.fetch_add(1, Ordering::SeqCst);
        Ok(PartRemoved {
            bytes_freed: 200,
            left_in_folder: vec!["the-users-own.txt".into()],
        })
    }

    fn verify(&self, _: &ModelDir, _: CudaPart) -> Result<(), CudaFilesError> {
        self.verifies.fetch_add(1, Ordering::SeqCst);
        if self.verify_fails {
            Err(CudaFilesError::Failed {
                message: "checksum".into(),
                retryable: false,
            })
        } else {
            Ok(())
        }
    }
}

/// A model check that says it ran, and waits for a message when it is given a receiver.
struct CountingCheck(Arc<AtomicU32>, Mutex<Option<mpsc::Receiver<()>>>);

impl ModelWork for CountingCheck {
    fn download(
        &self,
        _: &Services,
        _: &str,
        _: &CancelToken,
        _: &mut dyn FnMut(u64, u64),
    ) -> Result<(), DownloadEnd> {
        Ok(())
    }

    fn check(
        &self,
        _: &Services,
        _: &CancelToken,
        _: &mut dyn FnMut(CheckStep),
        _: &mut dyn FnMut(String),
    ) -> Result<CheckDone, IpcError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if let Some(wait) = self.1.lock().unwrap().take() {
            let _ = wait.recv();
        }
        Ok(CheckDone {
            answer: None,
            trials: 0,
        })
    }
}

struct Rig {
    jobs: CudaJobs,
    checks: ModelJobs,
    checked: Arc<AtomicU32>,
    events: mpsc::Receiver<CudaEvent>,
    gate: WorkGate,
}

fn rig(files: Arc<FakeFiles>) -> Rig {
    rig_with_check(files, None)
}

fn rig_with_check(files: Arc<FakeFiles>, wait: Option<mpsc::Receiver<()>>) -> Rig {
    let checked = Arc::new(AtomicU32::new(0));
    let gate = WorkGate::new();
    let sink: ModelSink = Arc::new(|_| {});
    let checks = ModelJobs::new(
        Arc::new(CountingCheck(Arc::clone(&checked), Mutex::new(wait))),
        sink,
        gate.clone(),
    );
    let (tx, events) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: CudaSink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    Rig {
        jobs: CudaJobs::new(files, sink, checks.clone()),
        checks,
        checked,
        events,
        gate,
    }
}

fn drain(events: &mpsc::Receiver<CudaEvent>) -> Vec<CudaEvent> {
    events.try_iter().collect()
}

fn view_on(hw: &Hardware, files: &dyn CudaFiles, start: &CudaStart) -> (CudaView, TempDir) {
    let dir = TempDir::new("cuda-view");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let view = cuda_view(
        &svc,
        CudaHost {
            models: &models,
            hardware: hw,
            order: VULKAN,
            files,
        },
        start,
        &|_| false,
        None,
    )
    .unwrap();
    (view, dir)
}

#[test]
fn cuda_settings_the_section_is_offered_only_with_a_suitable_gpu() {
    let files = FakeFiles::new(PartState::Missing, PartState::Missing);
    let off = CudaStart::off();
    let (view, _d) = view_on(&desktop(), &files, &off);
    assert!(view.offered);
    assert_eq!(view.card.as_deref(), Some("NVIDIA GeForce RTX 5080"));
    let (view, _d) = view_on(&no_nvidia(), &files, &off);
    assert!(!view.offered);
    assert_eq!(view.card, None);
    let gtx = Hardware {
        gpus: vec![rtx((6, 1))],
        ..Hardware::default()
    };
    assert!(!view_on(&gtx, &files, &off).0.offered);
}

#[test]
fn cuda_settings_each_part_has_its_own_size_terms_and_off_by_default() {
    let files = FakeFiles::new(PartState::Missing, PartState::Missing);
    let (view, _d) = view_on(&desktop(), &files, &CudaStart::off());
    assert!(!view.switch.summaries);
    assert!(!view.switch.transcription);
    assert!(!view.restart_needed);
    assert_eq!(view.summaries.part, CudaPartChoice::Summaries);
    assert_eq!(view.transcription.part, CudaPartChoice::Transcription);
    let titles =
        |p: &CudaPartView| -> Vec<String> { p.terms.iter().map(|t| t.url.clone()).collect() };
    assert_eq!(
        titles(&view.summaries),
        ["https://docs.nvidia.com/cuda/eula/index.html"]
    );
    assert_eq!(
        titles(&view.transcription),
        [
            "https://docs.nvidia.com/cuda/eula/index.html",
            "https://docs.nvidia.com/deeplearning/cudnn/sla/index.html"
        ]
    );
    // No measured number until there is one in the slot.
    assert_eq!(view.transcription_cpu_saved.as_deref(), MEASURED_CPU_SAVED);
}

#[test]
fn cuda_settings_a_download_not_available_yet_says_so_and_cannot_start() {
    let dir = TempDir::new("cuda-none");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = desktop();
    let start = CudaStart::off();
    let view = cuda_view(
        &svc,
        CudaHost {
            models: &models,
            hardware: &hw,
            order: VULKAN,
            files: &NoCudaFilesYet,
        },
        &start,
        &|_| false,
        None,
    )
    .unwrap();
    assert!(view.offered);
    // This test build has llama.cpp on the processor, so that is what they stay on.
    assert_eq!(view.summaries_stay_on, "the processor");
    assert_eq!(view.restart_blocked, None);
    assert_eq!(
        view.summaries.not_available.as_deref(),
        Some(NOT_AVAILABLE_YET)
    );

    let mut files = FakeFiles::new(PartState::Missing, PartState::Missing);
    files.not_available = Some(NOT_AVAILABLE_YET.to_owned());
    let files = Arc::new(files);
    let r = rig(Arc::clone(&files));
    let e = r
        .jobs
        .start_download(&svc, &hw, CudaPartChoice::Summaries, true)
        .unwrap_err();
    assert_eq!(e.kind, "not_available");
    assert_eq!(files.downloads.load(Ordering::SeqCst), 0);
}

#[test]
fn cuda_settings_a_download_needs_the_terms_the_offer_and_for_transcription_the_summaries_files() {
    let dir = TempDir::new("cuda-refuse");
    let svc = services(&dir);
    let files = Arc::new(FakeFiles::new(PartState::Missing, PartState::Missing));
    let r = rig(Arc::clone(&files));
    let start = |hw: &Hardware, part, accepted| {
        r.jobs
            .start_download(&svc, hw, part, accepted)
            .unwrap_err()
            .kind
    };
    use CudaPartChoice::{Summaries, Transcription};
    assert_eq!(start(&desktop(), Summaries, false), "terms_not_accepted");
    assert_eq!(start(&no_nvidia(), Summaries, true), "not_offered");
    assert_eq!(start(&desktop(), Transcription, true), "needs_summaries");
    r.jobs.join();
    assert_eq!(files.downloads.load(Ordering::SeqCst), 0);
    assert!(drain(&r.events).is_empty());
}

#[test]
fn cuda_settings_the_summaries_download_sends_progress_and_one_end_and_starts_no_check() {
    let dir = TempDir::new("cuda-summaries");
    let svc = services(&dir);
    let files = Arc::new(FakeFiles::new(PartState::Missing, PartState::Missing));
    let r = rig(Arc::clone(&files));
    let part = CudaPartChoice::Summaries;
    r.jobs.start_download(&svc, &desktop(), part, true).unwrap();
    r.jobs.join();
    assert_eq!(
        drain(&r.events),
        [
            CudaEvent::Progress {
                part,
                done: 50,
                total: 100
            },
            CudaEvent::Downloaded { part },
        ]
    );
    r.checks.join();
    assert_eq!(r.checked.load(Ordering::SeqCst), 0);
}

/// The owner: the check runs at the first start that chooses CUDA, not on an arrival.
#[test]
fn cuda_settings_a_finished_download_starts_no_check() {
    let dir = TempDir::new("cuda-transcription");
    let svc = services(&dir);
    for part in [CudaPartChoice::Summaries, CudaPartChoice::Transcription] {
        let files = Arc::new(FakeFiles::new(PartState::Installed, PartState::Missing));
        let r = rig(files);
        r.jobs.start_download(&svc, &desktop(), part, true).unwrap();
        r.jobs.join();
        r.checks.join();
        assert_eq!(r.checked.load(Ordering::SeqCst), 0, "{part:?}");
        let events = drain(&r.events);
        assert!(
            events.contains(&CudaEvent::Downloaded { part }),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CudaEvent::Warning { .. })),
            "{events:?}"
        );
    }
}

#[test]
fn cuda_settings_a_failed_download_shows_no_developer_command() {
    let dir = TempDir::new("cuda-plain");
    let svc = services(&dir);
    let files = Arc::new(FakeFiles::new(PartState::Missing, PartState::Missing));
    *files.download_ends.lock().unwrap() = Err(CudaFilesError::Failed {
        message: "run `cargo xtask fetch-models --set cuda`".into(),
        retryable: true,
    });
    let r = rig(files);
    let part = CudaPartChoice::Summaries;
    r.jobs.start_download(&svc, &desktop(), part, true).unwrap();
    r.jobs.join();
    let events = drain(&r.events);
    let Some(CudaEvent::Failed {
        message, retryable, ..
    }) = events.last()
    else {
        panic!("{events:?}");
    };
    assert_eq!(message, crate::plain_text::MODELS_NEEDED);
    assert!(*retryable);
}

#[test]
fn cuda_settings_switching_on_needs_the_files_and_off_never_touches_them() {
    let dir = TempDir::new("cuda-set");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = desktop();
    let on = CudaSwitchValue {
        summaries: true,
        transcription: false,
    };
    let off = CudaSwitchValue {
        summaries: false,
        transcription: false,
    };
    let missing = FakeFiles::new(PartState::Missing, PartState::Missing);
    let e = set_switch(&svc, &models, &hw, &missing, on).unwrap_err();
    assert_eq!(e.kind, "invalid_setting");
    let e = set_switch(
        &svc,
        &models,
        &no_nvidia(),
        &FakeFiles::new(PartState::Installed, PartState::Missing),
        on,
    )
    .unwrap_err();
    assert_eq!(e.kind, "invalid_setting");
    assert_eq!(
        cuda_switch::cuda_switch(&svc.store()).unwrap().0,
        CudaSwitch::default(),
        "nothing stored"
    );

    let files = FakeFiles::new(PartState::Installed, PartState::Installed);
    assert_eq!(set_switch(&svc, &models, &hw, &files, on).unwrap(), on);
    // Transcription cannot be on without summaries, and nothing changes when it is refused.
    let bad = CudaSwitchValue {
        summaries: false,
        transcription: true,
    };
    let e = set_switch(&svc, &models, &hw, &files, bad).unwrap_err();
    assert_eq!(e.kind, "invalid_setting");
    assert_eq!(cuda_switch::cuda_switch(&svc.store()).unwrap().0, on.into());
    // Switching back is one setting and never deletes files, even after the files have gone or
    // the card has.
    assert_eq!(
        set_switch(&svc, &models, &no_nvidia(), &missing, off).unwrap(),
        off
    );
    assert_eq!(files.removes.load(Ordering::SeqCst), 0);
    assert_eq!(missing.removes.load(Ordering::SeqCst), 0);
}

#[test]
fn cuda_settings_a_switch_applies_at_the_next_start() {
    let dir = TempDir::new("cuda-restart");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = desktop();
    let files = FakeFiles::new(PartState::Installed, PartState::Missing);
    let start = CudaStart::off();
    let view = |start: &CudaStart| {
        let host = CudaHost {
            models: &models,
            hardware: &hw,
            order: VULKAN,
            files: &files,
        };
        cuda_view(&svc, host, start, &|_| false, None).unwrap()
    };
    assert!(!view(&start).restart_needed);
    let on = CudaSwitchValue {
        summaries: true,
        transcription: false,
    };
    set_switch(&svc, &models, &hw, &files, on).unwrap();
    let after = view(&start);
    assert!(after.switch.summaries);
    assert!(after.restart_needed);
    assert!(
        !after.this_run.summaries_on_cuda,
        "the setting says nothing about what runs"
    );
    // A run that started with it needs no restart, whether the folder loaded or was refused.
    for summaries in [
        SummariesStart::Loaded {
            backend: "cuda".into(),
        },
        SummariesStart::Stayed { note: "n".into() },
    ] {
        let started = CudaStart {
            switch: on.into(),
            summaries,
            transcription: TranscriptionStart::Off,
        };
        assert!(!view(&started).restart_needed);
    }
}

#[test]
fn cuda_settings_remove_refuses_what_is_on_or_loaded_and_keeps_the_settings() {
    let dir = TempDir::new("cuda-remove");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = desktop();
    let files = Arc::new(FakeFiles::new(PartState::Installed, PartState::Missing));
    let r = rig(Arc::clone(&files));
    let part = CudaPartChoice::Summaries;
    let on = CudaSwitchValue {
        summaries: true,
        transcription: false,
    };
    set_switch(&svc, &models, &hw, &*files, on).unwrap();
    let e = r.jobs.remove(&svc, &CudaStart::off(), part).unwrap_err();
    assert_eq!(e.kind, "part_in_use");
    let off = CudaSwitchValue {
        summaries: false,
        transcription: false,
    };
    set_switch(&svc, &models, &hw, &*files, off).unwrap();
    // Off now, but this run loaded it.
    let loaded = CudaStart {
        switch: on.into(),
        summaries: SummariesStart::Loaded {
            backend: "cuda".into(),
        },
        transcription: TranscriptionStart::Off,
    };
    let e = r.jobs.remove(&svc, &loaded, part).unwrap_err();
    assert_eq!(e.kind, "part_in_use");
    assert!(e.message.contains("Restart"), "{}", e.message);
    assert_eq!(files.removes.load(Ordering::SeqCst), 0);
    assert_eq!(
        r.jobs.remove(&svc, &CudaStart::off(), part).unwrap(),
        CudaRemoved {
            bytes_freed: 200,
            left_in_folder: vec!["the-users-own.txt".into()]
        }
    );
    assert_eq!(files.removes.load(Ordering::SeqCst), 1);
}

#[test]
fn cuda_settings_while_the_start_checks_the_page_says_so_and_nothing_is_removed() {
    let dir = TempDir::new("cuda-checking");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let hw = desktop();
    let files = Arc::new(FakeFiles::new(PartState::Installed, PartState::Missing));
    let r = rig(Arc::clone(&files));
    let checking = CudaStart {
        switch: SWITCHED_ON,
        summaries: SummariesStart::Checking,
        transcription: TranscriptionStart::Off,
    };
    let host = CudaHost {
        models: &models,
        hardware: &hw,
        order: VULKAN,
        files: &*files,
    };
    let view = cuda_view(&svc, host, &checking, &|_| false, None).unwrap();
    assert!(view.this_run.checking);
    assert!(!view.this_run.summaries_on_cuda);
    assert_eq!(
        view.summaries.removal_refused.as_deref(),
        Some(CUDA_CHECKING)
    );
    assert_eq!(
        view.transcription.removal_refused.as_deref(),
        Some(CUDA_CHECKING)
    );
    for part in [CudaPartChoice::Summaries, CudaPartChoice::Transcription] {
        let e = r.jobs.remove(&svc, &checking, part).unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str()),
            ("busy", CUDA_CHECKING)
        );
    }
    assert_eq!(files.removes.load(Ordering::SeqCst), 0);
    // `models()` meanwhile shows what runs without the folder, and no note yet.
    let (mut before, _d) = models_view_on(VULKAN);
    let after = before.clone();
    apply_start(&mut before, &checking);
    assert_eq!(before, after);
    // Transcription still being tried holds the removal too.
    let transcribing = CudaStart {
        switch: BOTH_ON,
        summaries: SummariesStart::Off,
        transcription: TranscriptionStart::Checking,
    };
    assert!(transcribing.checking());
    let e = r
        .jobs
        .remove(&svc, &transcribing, CudaPartChoice::Transcription)
        .unwrap_err();
    assert_eq!(e.message, CUDA_CHECKING);
    assert_eq!(files.removes.load(Ordering::SeqCst), 0);
}

#[test]
fn cuda_settings_transcription_counts_as_loaded_only_when_the_start_chose_it() {
    let dir = TempDir::new("cuda-loaded");
    let svc = services(&dir);
    let r = rig(Arc::new(FakeFiles::new(
        PartState::Installed,
        PartState::Installed,
    )));
    let part = CudaPartChoice::Transcription;
    // The stored setting is off by now, so only what the start found stops the removal.
    let start = |transcription| CudaStart {
        switch: BOTH_ON,
        summaries: SummariesStart::Off,
        transcription,
    };
    let chosen = start(TranscriptionStart::Chosen {
        backend: "cuda".into(),
    });
    assert_eq!(chosen.loaded(), [CudaPart::Transcription]);
    let e = r.jobs.remove(&svc, &chosen, part).unwrap_err();
    assert_eq!(e.kind, "part_in_use");
    let stayed = start(TranscriptionStart::Stayed { why: "w".into() });
    assert_eq!(stayed.loaded(), []);
    assert_eq!(r.jobs.remove(&svc, &stayed, part).unwrap().bytes_freed, 200);
}

/// Moved from `ModelJobs` with the rule (the stronger of the two paths'): a meeting or a job may
/// have the files mapped, and either part's download stops either part's removal.
#[test]
fn cuda_settings_removal_is_refused_while_either_part_downloads_or_a_meeting_or_job_runs() {
    let dir = TempDir::new("cuda-remove-busy");
    let svc = services(&dir);
    let hw = desktop();
    use CudaPartChoice::{Summaries, Transcription};

    let files = Arc::new(FakeFiles::new(PartState::Installed, PartState::Missing));
    let r = rig(Arc::clone(&files));
    let meeting = r.gate.enter_meeting().unwrap();
    let e = r
        .jobs
        .remove(&svc, &CudaStart::off(), Summaries)
        .unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str(), e.retryable),
        ("busy", crate::activity::WORK_RUNNING_FOR_CUDA_REMOVAL, true)
    );
    drop(meeting);
    let job = r.gate.enter_work().unwrap();
    let e = r
        .jobs
        .remove(&svc, &CudaStart::off(), Summaries)
        .unwrap_err();
    assert_eq!(e.message, crate::activity::WORK_RUNNING_FOR_CUDA_REMOVAL);
    drop(job);
    assert_eq!(files.removes.load(Ordering::SeqCst), 0);

    for (downloading, removed) in [(Transcription, Summaries), (Summaries, Transcription)] {
        let files = Arc::new(FakeFiles::new(PartState::Installed, PartState::Missing));
        let (release, wait) = mpsc::channel();
        *files.download_wait.lock().unwrap() = Some(wait);
        let r = rig(Arc::clone(&files));
        r.jobs.start_download(&svc, &hw, downloading, true).unwrap();
        let e = r.jobs.remove(&svc, &CudaStart::off(), removed).unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str(), e.retryable),
            ("busy", CUDA_DOWNLOADING_MESSAGE, true)
        );
        release.send(()).unwrap();
        r.jobs.join();
        r.checks.join();
        assert_eq!(files.removes.load(Ordering::SeqCst), 0);
        // Once it ended the removal goes ahead, and the pass is released afterwards.
        r.jobs.remove(&svc, &CudaStart::off(), removed).unwrap();
        assert_eq!(files.removes.load(Ordering::SeqCst), 1);
        drop(r.gate.enter_meeting().unwrap());
    }
}

/// One way to a download, and it asks for NVIDIA's terms: the model jobs do not know the parts.
#[test]
fn cuda_settings_a_part_is_not_a_model_job() {
    for name in ["cuda-summaries", "cuda-transcription"] {
        assert_eq!(
            crate::model_jobs::require_catalogue_model(name)
                .unwrap_err()
                .kind,
            "unknown_model"
        );
    }
}

/// T8.4af's decision: the folder is in nothing the check reads unless this run chose it, and a
/// child that did choose it checks every byte first, so a check may start beside a part's download.
#[test]
fn cuda_settings_a_check_may_start_while_a_part_downloads() {
    let dir = TempDir::new("cuda-beside");
    let svc = services(&dir);
    let files = Arc::new(FakeFiles::new(PartState::Missing, PartState::Missing));
    let (release, wait) = mpsc::channel();
    *files.download_wait.lock().unwrap() = Some(wait);
    let r = rig(Arc::clone(&files));
    r.jobs
        .start_download(&svc, &desktop(), CudaPartChoice::Summaries, true)
        .unwrap();
    r.checks.start_check(&svc).unwrap();
    r.checks.join();
    assert_eq!(r.checked.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    r.jobs.join();
}

#[test]
fn cuda_settings_the_restart_bar_names_what_a_restart_would_cut() {
    assert_eq!(restart_blocked(false, false), None);
    assert_eq!(restart_blocked(true, false), Some(RESTART_MEETING));
    assert_eq!(restart_blocked(false, true), Some(RESTART_DOWNLOADING));
    assert_eq!(restart_blocked(true, true), Some(RESTART_MEETING));
    let dir = TempDir::new("cuda-bar");
    let svc = services(&dir);
    let files = Arc::new(FakeFiles::new(PartState::Missing, PartState::Missing));
    let (release, wait) = mpsc::channel();
    *files.download_wait.lock().unwrap() = Some(wait);
    let r = rig(Arc::clone(&files));
    assert!(!r.jobs.any_downloading());
    r.jobs
        .start_download(&svc, &desktop(), CudaPartChoice::Summaries, true)
        .unwrap();
    assert!(r.jobs.any_downloading());
    release.send(()).unwrap();
    r.jobs.join();
    assert!(!r.jobs.any_downloading());
}

#[test]
fn cuda_settings_a_running_model_check_keeps_downloads_and_removals_out() {
    let dir = TempDir::new("cuda-check");
    let svc = services(&dir);
    let files = Arc::new(FakeFiles::new(PartState::Missing, PartState::Missing));
    let (release, wait) = mpsc::channel();
    let r = rig_with_check(Arc::clone(&files), Some(wait));
    r.checks.start_check(&svc).unwrap();
    let part = CudaPartChoice::Summaries;
    let download = r.jobs.start_download(&svc, &desktop(), part, true);
    let remove = r.jobs.remove(&svc, &CudaStart::off(), part);
    release.send(()).unwrap();
    r.checks.join();
    let message = crate::model_jobs::CHECK_RUNNING_MESSAGE;
    assert_eq!(download.unwrap_err().message, message);
    assert_eq!(remove.unwrap_err().message, message);
    assert_eq!(files.downloads.load(Ordering::SeqCst), 0);
    assert_eq!(files.removes.load(Ordering::SeqCst), 0);
    // Afterwards both go ahead.
    assert_eq!(
        r.jobs
            .remove(&svc, &CudaStart::off(), part)
            .unwrap()
            .bytes_freed,
        200
    );
}

/// Every step of the start, counted, with what each answers.
struct Start {
    offered: bool,
    model: bool,
    verify_fails: bool,
    verdict: Verdict,
    load: Result<(), String>,
}

impl Default for Start {
    fn default() -> Self {
        Self {
            offered: true,
            model: true,
            verify_fails: false,
            verdict: Verdict::Usable {
                backend: "cuda".into(),
            },
            load: Ok(()),
        }
    }
}

/// `(what the start decided, verifies, trials, loads)`.
fn run_start(s: Start) -> (SummariesStart, u32, u32, u32) {
    let mut files = FakeFiles::new(PartState::Installed, PartState::Missing);
    files.verify_fails = s.verify_fails;
    let (trials, loads) = (AtomicU32::new(0), AtomicU32::new(0));
    let models = ModelDir::new("C:/models");
    let outcome = decide_summaries(
        s.offered,
        &models,
        &files,
        s.model.then(|| PathBuf::from("C:/m.gguf")),
        "Vulkan",
        &|folder, _| {
            assert_eq!(folder, cuda::folder(&models));
            trials.fetch_add(1, Ordering::SeqCst);
            s.verdict.clone()
        },
        &|_| {
            loads.fetch_add(1, Ordering::SeqCst);
            s.load.clone()
        },
    );
    (
        outcome,
        files.verifies.load(Ordering::SeqCst),
        trials.load(Ordering::SeqCst),
        loads.load(Ordering::SeqCst),
    )
}

fn note_of(s: &SummariesStart) -> &str {
    match s {
        SummariesStart::Stayed { note } => note,
        other => panic!("expected a refusal: {other:?}"),
    }
}

#[test]
fn cuda_settings_start_off_does_nothing() {
    let dir = TempDir::new("cuda-start-off");
    let svc = services(&dir);
    let files = FakeFiles::new(PartState::Installed, PartState::Installed);
    let files: Arc<FakeFiles> = Arc::new(files);
    // Answered at once, on no thread.
    let told = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&told);
    let latch = begin(&svc, Arc::clone(&files) as Arc<dyn CudaFiles>, move || {
        flag.store(true, Ordering::SeqCst);
    });
    assert_eq!(*latch.current(), CudaStart::off());
    assert_eq!(files.verifies.load(Ordering::SeqCst), 0);
    // Stored off is the same: `begin`'s branch for summaries off reaches no step of the trial.
    set_cuda_switch(
        &mut svc.store(),
        CudaSwitch {
            summaries: false,
            transcription: false,
        },
    )
    .unwrap();
    let latch = begin(&svc, Arc::clone(&files) as Arc<dyn CudaFiles>, || {});
    assert_eq!(latch.current().summaries, SummariesStart::Off);
    assert_eq!(files.verifies.load(Ordering::SeqCst), 0);
    // Nothing worked, so there is nothing to tell the page.
    assert!(!told.load(Ordering::SeqCst));
}

/// Sets a flag when dropped, standing for the hold on llama.cpp's gate.
struct FakeHold(Arc<AtomicBool>);

impl Drop for FakeHold {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

const SWITCHED_ON: CudaSwitch = CudaSwitch {
    summaries: true,
    transcription: false,
};

const BOTH_ON: CudaSwitch = CudaSwitch {
    summaries: true,
    transcription: true,
};

#[test]
fn cuda_settings_start_returns_at_once_and_holds_llama_until_the_work_ended() {
    let released = Arc::new(AtomicBool::new(false));
    let (go, wait) = mpsc::channel::<()>();
    let told = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&told);
    let latch = latched(
        SWITCHED_ON,
        FakeHold(Arc::clone(&released)),
        "Vulkan".into(),
        move || flag.store(true, Ordering::SeqCst),
        move || {
            wait.recv().unwrap();
            (
                SummariesStart::Loaded {
                    backend: "cuda".into(),
                },
                TranscriptionStart::Off,
            )
        },
    );
    // We are here with the work unfinished: the caller (the window's setup) was not held.
    // Anyone may ask meanwhile, and is told at once.
    assert_eq!(
        *latch.current(),
        CudaStart {
            switch: SWITCHED_ON,
            summaries: SummariesStart::Checking,
            transcription: TranscriptionStart::Off,
        }
    );
    assert!(!told.load(Ordering::SeqCst));
    assert!(
        !released.load(Ordering::SeqCst),
        "llama stays shut meanwhile"
    );
    // Whoever waits for the answer waits for the work.
    let (answer_tx, answer) = mpsc::channel();
    let waiter = Arc::clone(&latch);
    let thread = std::thread::spawn(move || answer_tx.send(waiter.wait()).unwrap());
    assert!(answer.recv_timeout(Duration::from_millis(200)).is_err());
    go.send(()).unwrap();
    let start = answer.recv().unwrap();
    thread.join().unwrap();
    assert_eq!(
        start.summaries,
        SummariesStart::Loaded {
            backend: "cuda".into()
        }
    );
    assert!(
        released.load(Ordering::SeqCst),
        "the answer comes after the hold ended"
    );
    // The page is told after the answer is there: it runs last, on the start's thread.
    let limit = std::time::Instant::now() + Duration::from_secs(60);
    while !told.load(Ordering::SeqCst) {
        assert!(std::time::Instant::now() < limit, "the page was never told");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(matches!(
        latch.current().summaries,
        SummariesStart::Loaded { .. }
    ));
}

#[test]
fn cuda_settings_start_a_panic_releases_llama_and_stays_with_a_note() {
    let released = Arc::new(AtomicBool::new(false));
    let latch = latched(
        SWITCHED_ON,
        FakeHold(Arc::clone(&released)),
        "Vulkan".into(),
        || {},
        || panic!("the trial's own failure"),
    );
    let start = latch.wait();
    assert!(released.load(Ordering::SeqCst));
    let note = note_of(&start.summaries);
    assert!(note.contains("They stay on Vulkan"), "{note}");
    assert!(!note.contains("the trial's own failure"), "{note}");
    assert_eq!(start.transcription, TranscriptionStart::Off);

    // With transcription on, both parts say so and every hold ends.
    let released = Arc::new(AtomicBool::new(false));
    let latch = latched(
        BOTH_ON,
        FakeHold(Arc::clone(&released)),
        "Vulkan".into(),
        || {},
        || panic!("the trial's own failure"),
    );
    let start = latch.wait();
    assert!(released.load(Ordering::SeqCst));
    let TranscriptionStart::Stayed { why } = &start.transcription else {
        panic!("{start:?}");
    };
    assert!(!why.contains("the trial's own failure"), "{why}");
}

/// The gates are process-wide: tests that close them and look at them take turns.
static GATE_TESTS: Mutex<()> = Mutex::new(());

#[test]
fn cuda_settings_begin_with_the_setting_on_closes_the_gate_and_opens_it_again() {
    let _turn = GATE_TESTS.lock().unwrap();
    let dir = TempDir::new("cuda-begin");
    let svc = services(&dir);
    set_cuda_switch(&mut svc.store(), SWITCHED_ON).unwrap();
    let mut files = FakeFiles::new(PartState::Installed, PartState::Missing);
    // Refused at the check at the latest, so this test never starts a trial or calls llama.
    files.verify_fails = true;
    let latch = begin(&svc, Arc::new(files), || {});
    let start = latch.wait();
    assert_eq!(start.switch, SWITCHED_ON);
    assert!(note_of(&start.summaries).contains("They stay on"));
    assert_eq!(start.transcription, TranscriptionStart::Off);
    assert!(!earshot_runtime::host::llama_held());
}

#[test]
fn cuda_settings_begin_with_transcription_on_refuses_with_a_note_and_opens_onnx_again() {
    let _turn = GATE_TESTS.lock().unwrap();
    let dir = TempDir::new("cuda-begin-both");
    let svc = services(&dir);
    set_cuda_switch(&mut svc.store(), BOTH_ON).unwrap();
    let mut files = FakeFiles::new(PartState::Installed, PartState::Installed);
    // Keeps summaries from their trial. Transcription stops at the empty models folder (no model to
    // try it with) whatever this host has installed, so this test exe never runs a trial.
    files.verify_fails = true;
    let models = ModelDir::new(dir.0.join("no-models"));
    let latch = begin_in(&svc, Arc::new(files), models, || {});
    let start = latch.wait();
    let TranscriptionStart::Stayed { why } = &start.transcription else {
        panic!("{start:?}");
    };
    let hardware = earshot_runtime::hardware();
    assert_eq!(
        Some(why.as_str()),
        transcription_refusal(
            cuda_switch::offered(&hardware),
            cuda_switch::transcription_fits_card(&hardware),
            cuda::provider_built(),
            false,
        ),
        "the refusal comes from this start's inputs, not a trial or the host's models"
    );
    assert!(start.loaded().is_empty());
    assert!(!cuda::onnx_held(), "the hold ended before the answer");
    assert!(!earshot_runtime::host::llama_held());
    assert!(!cuda::transcription_from_folder());
}

/// Every step of the start for transcription, counted: `(what it decided, trials, chosen)`.
fn run_transcription_start(
    refusal: Option<&str>,
    verdict: Verdict,
    choose: Result<(), String>,
) -> (TranscriptionStart, u32, u32) {
    let (trials, chosen) = (AtomicU32::new(0), AtomicU32::new(0));
    let outcome = decide_transcription(
        refusal,
        &|| {
            trials.fetch_add(1, Ordering::SeqCst);
            verdict.clone()
        },
        &|| {
            chosen.fetch_add(1, Ordering::SeqCst);
            choose.clone()
        },
    );
    (
        outcome,
        trials.load(Ordering::SeqCst),
        chosen.load(Ordering::SeqCst),
    )
}

#[test]
fn cuda_settings_transcription_tries_then_chooses_and_reports_the_trials_backend() {
    let usable = || Verdict::Usable {
        backend: "cuda".into(),
    };
    let (s, t, c) = run_transcription_start(None, usable(), Ok(()));
    assert_eq!(
        s,
        TranscriptionStart::Chosen {
            backend: "cuda".into()
        }
    );
    assert_eq!((t, c), (1, 1));

    type Case<'a> = (
        Option<&'a str>,
        Verdict,
        Result<(), String>,
        &'a str,
        (u32, u32),
    );
    let cases: Vec<Case<'_>> = vec![
        // No Parakeet model: no switch this run, and nothing was tried.
        (
            transcription_refusal(true, true, true, false),
            usable(),
            Ok(()),
            "no Parakeet model",
            (0, 0),
        ),
        (
            transcription_refusal(false, true, true, true),
            usable(),
            Ok(()),
            "graphics card",
            (0, 0),
        ),
        (
            transcription_refusal(true, true, false, true),
            usable(),
            Ok(()),
            "version of Earshot",
            (0, 0),
        ),
        (
            None,
            Verdict::Refused {
                why: "the trial ended with exit code 0xc0000409".into(),
            },
            Ok(()),
            "0xc0000409",
            (1, 0),
        ),
        // The files changed between the trial and the choice, which reads them again.
        (
            None,
            usable(),
            Err(cuda::NOT_READY.to_owned()),
            cuda::NOT_READY,
            (1, 1),
        ),
    ];
    for (refusal, verdict, choose, word, counts) in cases {
        let (s, t, c) = run_transcription_start(refusal, verdict, choose);
        let TranscriptionStart::Stayed { why } = s else {
            panic!("{word}: {s:?}");
        };
        assert!(why.contains(word), "{why}");
        assert_eq!((t, c), counts, "{word}");
    }
    assert_eq!(transcription_refusal(true, true, true, true), None);
    assert_eq!(
        transcription_refusal(true, false, true, true),
        Some(CARD_TOO_SMALL)
    );
}

/// A start whose thread cannot start tells the page like any other.
#[test]
fn cuda_settings_start_that_cannot_spawn_still_resolves_and_tells() {
    let released = Arc::new(AtomicBool::new(false));
    let told = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&told);
    let latch = latched_on(
        |_job| Err(std::io::Error::other("no threads left")),
        BOTH_ON,
        FakeHold(Arc::clone(&released)),
        "Vulkan".into(),
        move || flag.store(true, Ordering::SeqCst),
        || unreachable!("never ran"),
    );
    assert!(
        released.load(Ordering::SeqCst),
        "the hold went with the thread"
    );
    assert!(told.load(Ordering::SeqCst));
    let start = latch.current();
    assert!(note_of(&start.summaries).contains("could not run"));
    assert!(matches!(
        start.transcription,
        TranscriptionStart::Stayed { .. }
    ));
}

#[test]
fn cuda_settings_the_check_follows_only_a_start_that_chose_cuda() {
    let start = |transcription| CudaStart {
        switch: BOTH_ON,
        summaries: SummariesStart::Off,
        transcription,
    };
    let chosen = start(TranscriptionStart::Chosen {
        backend: "cuda".into(),
    });
    assert!(check_follows_start(&chosen, true, false));
    assert!(!check_follows_start(&chosen, true, true), "already current");
    assert!(!check_follows_start(&chosen, false, false), "a named model");
    for other in [
        TranscriptionStart::Off,
        TranscriptionStart::Checking,
        TranscriptionStart::Stayed { why: "w".into() },
    ] {
        assert!(!check_follows_start(&start(other), true, false));
    }
}

#[test]
fn cuda_settings_check_after_start_runs_the_check_or_warns() {
    let chosen = || {
        CudaLatch::resolved_with(CudaStart {
            switch: BOTH_ON,
            summaries: SummariesStart::Off,
            transcription: TranscriptionStart::Chosen {
                backend: "cuda".into(),
            },
        })
    };
    let dir = TempDir::new("cuda-after");
    let svc = services(&dir);

    // No stored check, so the choice leaves it out of date.
    let r = rig(Arc::new(FakeFiles::new(
        PartState::Installed,
        PartState::Installed,
    )));
    let (tx, warned) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: CudaSink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    check_after_start(chosen(), svc.clone(), r.checks.clone(), Arc::clone(&sink));
    let limit = std::time::Instant::now() + Duration::from_secs(120);
    while r.checked.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < limit, "the check never ran");
        std::thread::sleep(Duration::from_millis(5));
    }
    r.checks.join();
    assert_eq!(r.checked.load(Ordering::SeqCst), 1);
    assert!(warned.try_recv().is_err());

    // Refused by a meeting: one warning, and no check.
    let r = rig(Arc::new(FakeFiles::new(
        PartState::Installed,
        PartState::Installed,
    )));
    let meeting = r.gate.enter_meeting().unwrap();
    check_after_start(chosen(), svc.clone(), r.checks.clone(), sink);
    let e = warned.recv_timeout(Duration::from_secs(120)).unwrap();
    assert!(matches!(e, CudaEvent::Warning { .. }), "{e:?}");
    assert_eq!(r.checked.load(Ordering::SeqCst), 0);
    drop(meeting);
}

/// Not a test of its own: [`cuda_settings_a_meeting_starts_while_a_trial_is_hung`] runs it alone in
/// a process, because the choice and its gates are one per process and a meeting ends the choice.
#[test]
#[ignore = "run by cuda_settings_a_meeting_starts_while_a_trial_is_hung in its own process"]
fn cuda_settings_meeting_preflight_beside_a_hung_trial() {
    use crate::meeting::{HostPrepare, MeetingSource, Prepare, StartRequest};
    use crate::meeting_events::CaptureMode;
    use earshot_runtime::model_settings::{ModelPick, ModelSettings, set_model_settings};
    use earshot_runtime::summary_settings::{
        EndpointProvider, SummarySettings, SummaryWriter, set_summary_settings,
    };

    let dir = TempDir::new("cuda-hung");
    let svc = services(&dir);
    {
        let mut db = svc.store();
        set_model_settings(
            &mut db,
            &ModelSettings {
                transcription: ModelPick::Named("base.en".into()),
                ..ModelSettings::default()
            },
        )
        .unwrap();
        // Nothing connects before a summary; only the trial's gate is being looked at here.
        set_summary_settings(
            &mut db,
            &SummarySettings {
                writer: SummaryWriter::OwnEndpoint,
                provider: EndpointProvider::OpenAiCompatible,
                url: "http://127.0.0.1:9/v1".into(),
                model: "m".into(),
                ..SummarySettings::default()
            },
        )
        .unwrap();
    }
    // The start's own holds, and a trial that never ends.
    let (never, hung) = mpsc::channel::<()>();
    let latch = latched(
        BOTH_ON,
        (earshot_runtime::host::hold_llama(), Some(cuda::hold_onnx())),
        "Vulkan".into(),
        || {},
        move || {
            let _ = hung.recv();
            (SummariesStart::Off, TranscriptionStart::Off)
        },
    );
    assert!(cuda::onnx_held());

    let source_asked = Arc::new(AtomicBool::new(false));
    let asked = Arc::clone(&source_asked);
    let prepare = HostPrepare::with_source(Box::new(move |_, _| {
        asked.store(true, Ordering::SeqCst);
        Ok(MeetingSource {
            starter: Box::new(|_, _, _, _| unreachable!("this test starts no capture")),
            clock_domain: earshot_core::model::ClockDomain::Replay,
            notices: Vec::new(),
            call_signal: None,
        })
    }));
    let request = StartRequest {
        mode: CaptureMode::Call,
        title: String::new(),
    };
    let ticket = Arc::new(earshot_runtime::meeting_queue::MeetingQueue::new().enter());
    drop(
        prepare
            .prepare(&svc, &request, &ticket)
            .unwrap_or_else(|e| panic!("{e:?}: cargo xtask fetch-models --set ci")),
    );

    // Capture is next, with the trial still hung: the whole pre-flight ran beside it.
    assert!(source_asked.load(Ordering::SeqCst));
    assert!(latch.current().checking(), "the trial is still hung");
    // Nothing was given up: the start goes on and decides for this meeting too, which holds its
    // models until then (T9.1l; the meeting itself is `meeting_queue_cuda`).
    assert!(cuda::onnx_held());
    assert!(latch.current().checking());
    assert!(
        !cuda::transcription_from_folder(),
        "nothing has decided yet"
    );
    // And a second meeting's pre-flight does not wait either.
    drop(prepare.prepare(&svc, &request, &ticket).unwrap());
    drop(never);
}

/// A trial that never ends must not cost a recording: the real pre-flight, up to the capture
/// source, in a process with the start's real gates closed. A wait here would hang until the
/// deadline. Needs `cargo xtask fetch-models --set ci`.
#[test]
fn cuda_settings_a_meeting_starts_while_a_trial_is_hung() {
    let out = earshot_proc::run(
        &earshot_proc::ChildSpec {
            program: std::env::current_exe().unwrap(),
            args: [
                "--exact",
                "cuda_settings::tests::cuda_settings_meeting_preflight_beside_a_hung_trial",
                "--ignored",
                "--nocapture",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
            current_dir: std::env::temp_dir(),
            env_remove: Vec::new(),
            env_set: Vec::new(),
            stdin: Vec::new(),
            deadline: Duration::from_secs(120),
            max_output: 1024 * 1024,
        },
        &|| false,
    )
    .expect("the pre-flight beside a hung trial ended in time");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && text.contains("1 passed"),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The check that follows a start gives way: the meeting is not refused, and the check stops.
#[test]
fn cuda_settings_the_check_after_a_start_gives_way_to_a_meeting() {
    struct UntilCancelled(mpsc::Sender<()>);
    impl ModelWork for UntilCancelled {
        fn download(
            &self,
            _: &Services,
            _: &str,
            _: &CancelToken,
            _: &mut dyn FnMut(u64, u64),
        ) -> Result<(), DownloadEnd> {
            Ok(())
        }
        fn check(
            &self,
            _: &Services,
            cancel: &CancelToken,
            _: &mut dyn FnMut(CheckStep),
            _: &mut dyn FnMut(String),
        ) -> Result<CheckDone, IpcError> {
            self.0.send(()).unwrap();
            let limit = std::time::Instant::now() + Duration::from_secs(120);
            while !cancel.is_cancelled() {
                assert!(std::time::Instant::now() < limit, "never cancelled");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(crate::check_child::cancelled())
        }
    }
    let dir = TempDir::new("cuda-way");
    let svc = services(&dir);
    let (running, is_running) = mpsc::channel();
    let gate = WorkGate::new();
    let (tx, heard) = mpsc::channel();
    let tx = Mutex::new(tx);
    let sink: ModelSink = Arc::new(move |e| {
        let _ = tx.lock().unwrap().send(e);
    });
    let checks = ModelJobs::new(Arc::new(UntilCancelled(running)), sink, gate.clone());
    let cuda_sink: CudaSink = Arc::new(|e| panic!("no warning: {e:?}"));
    let latch = CudaLatch::resolved_with(CudaStart {
        switch: BOTH_ON,
        summaries: SummariesStart::Off,
        transcription: TranscriptionStart::Chosen {
            backend: "cuda".into(),
        },
    });
    check_after_start(latch, svc.clone(), checks.clone(), cuda_sink);
    is_running.recv_timeout(Duration::from_secs(120)).unwrap();
    // The meeting goes in at once with the check running, and the check ends.
    let meeting = gate.enter_meeting().unwrap();
    checks.join();
    drop(meeting);
    // Quietly: nothing the user is told as the recording begins.
    assert_eq!(
        heard.try_iter().collect::<Vec<_>>(),
        [crate::model_jobs::ModelJobEvent::CheckGaveWay]
    );
    // A check the user asked for still keeps a meeting out, as it always has.
    let asked = gate.enter_check().unwrap();
    assert_eq!(gate.enter_meeting().unwrap_err().kind, "busy");
    drop(asked);
    drop(gate.enter_meeting().unwrap());
}

/// Everything that loads ONNX Runtime in the app reads the order after the choice.
#[test]
fn cuda_settings_every_app_path_to_onnx_runtime_waits_for_the_choice() {
    // A job waits at the gate. A meeting's capture never reaches it: everything of a meeting that
    // loads ONNX Runtime goes through `MeetingModels`, on the session's hold thread or in its turn.
    let jobs = include_str!("jobs.rs");
    let load = jobs.find("host::init_onnx_for(&");
    assert!(load.is_some(), "jobs.rs loads it");
    let at = jobs.find("host::settled_order()");
    assert!(at.is_some() && at < load, "jobs.rs");
    let meeting = include_str!("meeting.rs");
    for direct in [
        concat!("init_onnx", "_for("),
        concat!("host::settled", "_order"),
        concat!("host::live", "_factory("),
    ] {
        assert!(!meeting.contains(direct), "{direct}");
    }
    assert!(meeting.contains("MeetingModels::new("));
    let host = include_str!("../../../../crates/earshot-runtime/src/host.rs");
    let resolve = host.find("pub fn resolve(&self)").unwrap();
    let settled = host[resolve..].find("settled_order()").unwrap();
    let init = host[resolve..].find("init_onnx_for(").unwrap();
    assert!(settled < init, "the order is read after the choice");
    // Nothing holding a pass that refuses meetings waits (`activity` refuses it first).
    let child = include_str!("check_child.rs");
    assert!(!child.contains(concat!("wait_for", "_onnx")));
    assert!(include_str!("main.rs").contains("WorkGate::for_app()"));
}

/// The app runs on the real files; the fake is this test file's.
#[test]
fn cuda_settings_main_uses_the_real_files() {
    let main = include_str!("main.rs");
    assert!(main.contains("ManifestCudaFiles::new("));
    assert!(!main.contains(concat!("NoCuda", "FilesYet")));
}

/// Nothing in `main` may reach llama.cpp before the gate closes.
#[test]
fn cuda_settings_begin_comes_first_in_the_apps_setup() {
    let main = include_str!("main.rs");
    let begin = main
        .find("cuda_settings::begin(")
        .expect("main begins the switch");
    for later in [
        "spawn_retention(",
        "MeetingDriver::new(",
        "JobRunner::new(",
        "ModelJobs::new(",
        "BenchmarkJobs::new(",
        "build_tray()",
    ] {
        let at = main
            .find(later)
            .unwrap_or_else(|| panic!("{later} in main"));
        assert!(begin < at, "{later} comes before the gate closes");
    }
}

#[test]
fn cuda_settings_start_verifies_tries_then_loads_and_reports_the_trials_backend() {
    let (s, v, t, l) = run_start(Start::default());
    assert_eq!(
        s,
        SummariesStart::Loaded {
            backend: "cuda".into()
        }
    );
    assert_eq!((v, t, l), (1, 1, 1));
}

#[test]
fn cuda_settings_start_every_refusal_stays_with_a_note_and_goes_no_further() {
    // (the case, the words the note holds, verifies, trials, loads)
    let cases: Vec<(Start, &str, u32, u32, u32)> = vec![
        (
            Start {
                offered: false,
                ..Start::default()
            },
            "graphics card",
            0,
            0,
            0,
        ),
        (
            Start {
                model: false,
                ..Start::default()
            },
            "no summary model",
            0,
            0,
            0,
        ),
        (
            Start {
                verify_fails: true,
                ..Start::default()
            },
            "did not pass the check",
            1,
            0,
            0,
        ),
        (
            Start {
                verdict: Verdict::Refused {
                    why: "the trial ended with exit code 0xc0000409".into(),
                },
                ..Start::default()
            },
            "0xc0000409",
            1,
            1,
            0,
        ),
        // A folder that ran on something else is not CUDA, whatever it loads.
        (
            Start {
                verdict: Verdict::Usable {
                    backend: "vulkan".into(),
                },
                ..Start::default()
            },
            "not on CUDA",
            1,
            1,
            0,
        ),
        (
            Start {
                load: Err("a library of the set failed to load".into()),
                ..Start::default()
            },
            "failed to load",
            1,
            1,
            1,
        ),
    ];
    for (case, word, verifies, trials, loads) in cases {
        let (s, v, t, l) = run_start(case);
        let note = note_of(&s);
        assert!(note.contains(word), "{note}");
        assert!(note.contains("They stay on Vulkan"), "{note}");
        assert_eq!((v, t, l), (verifies, trials, loads), "{note}");
    }
}

#[test]
fn cuda_settings_stays_on_names_what_the_build_has() {
    assert_eq!(stays_on(VULKAN, true), "Vulkan");
    assert_eq!(stays_on(&[Accelerator::Cpu], true), "the processor");
    assert_eq!(stays_on(&[], true), "the processor");
    assert_ne!(stays_on(&[Accelerator::Cuda], true), "Vulkan");
    // A card too small for the model, or the processor chosen in Settings: not Vulkan, whatever
    // the build has.
    assert_eq!(stays_on(VULKAN, false), "the processor");
}

fn models_view_on(order: &[Accelerator]) -> (ModelsView, TempDir) {
    let dir = TempDir::new("cuda-models");
    let svc = services(&dir);
    let models = ModelDir::new(dir.0.join("models"));
    let view = crate::models::models_view(&svc, &models, &desktop(), order, &|_| false).unwrap();
    (view, dir)
}

#[test]
fn cuda_settings_the_hardware_rows_follow_the_evidence_not_the_setting() {
    let order = [Accelerator::Vulkan, Accelerator::Cpu];
    let on = CudaSwitch {
        summaries: true,
        transcription: false,
    };
    // The build's own label first: a Vulkan build's summaries say Vulkan, never CUDA.
    assert_eq!(crate::models::summary_gpu_part(&order), RunsOn::Vulkan);
    assert_eq!(
        crate::models::summary_gpu_part(&[Accelerator::Cuda, Accelerator::Cpu]),
        RunsOn::Cuda
    );
    assert_eq!(
        crate::models::summary_gpu_part(&[Accelerator::Cpu]),
        RunsOn::Processor
    );
    // What `models_view` gives in a build with llama.cpp on Vulkan (this test build has it on the
    // processor, whatever the order says).
    let on_the_card = || {
        let (mut view, dir) = models_view_on(&order);
        view.in_use.summary_runs_on = crate::models::summary_gpu_part(&order);
        (view, dir)
    };
    let (mut view, _d) = on_the_card();
    let notes = view.in_use.notes.len();

    // On, and refused: Vulkan with one note.
    let stayed_start = CudaStart {
        switch: on,
        summaries: SummariesStart::Stayed {
            note: "stayed".into(),
        },
        transcription: TranscriptionStart::Off,
    };
    apply_start(&mut view, &stayed_start);
    assert_eq!(view.in_use.summary_runs_on, RunsOn::Vulkan);
    assert_eq!(view.in_use.notes.len(), notes + 1);
    assert!(view.in_use.notes.iter().any(|n| n == "stayed"));

    // On, and it ran there.
    let (mut view, _d) = on_the_card();
    let loaded = CudaStart {
        switch: on,
        summaries: SummariesStart::Loaded {
            backend: "cuda".into(),
        },
        transcription: TranscriptionStart::Off,
    };
    apply_start(&mut view, &loaded);
    assert_eq!(view.in_use.summary_runs_on, RunsOn::Cuda);
    assert_eq!(view.in_use.notes.len(), notes);

    // Off in this run, whatever is stored since.
    let (mut view, _d) = on_the_card();
    apply_start(&mut view, &CudaStart::off());
    assert_eq!(view.in_use.summary_runs_on, RunsOn::Vulkan);
    assert_eq!(view.in_use.notes.len(), notes);

    // A summary on the processor stays there even with the folder loaded.
    let (mut view, _d) = models_view_on(&[Accelerator::Cpu]);
    assert_eq!(view.in_use.summary_runs_on, RunsOn::Processor);
    apply_start(&mut view, &loaded);
    assert_eq!(view.in_use.summary_runs_on, RunsOn::Processor);
}

#[test]
fn cuda_settings_transcription_that_stayed_says_so_once_and_one_that_was_chosen_says_nothing() {
    let order = [Accelerator::Vulkan, Accelerator::Cpu];
    let start = |transcription| CudaStart {
        switch: BOTH_ON,
        summaries: SummariesStart::Loaded {
            backend: "cuda".into(),
        },
        transcription,
    };
    let (mut view, _d) = models_view_on(&order);
    let notes = view.in_use.notes.len();
    apply_start(
        &mut view,
        &start(TranscriptionStart::Stayed {
            why: "its files did not pass the check".into(),
        }),
    );
    assert_eq!(view.in_use.notes.len(), notes + 1);
    let note = &view.in_use.notes[notes];
    assert!(note.contains("its files did not pass the check"), "{note}");
    assert!(note.contains("It stays on"), "{note}");

    // Chosen, or still being tried: no note, and the row is the order's, never this code's.
    for quiet in [
        TranscriptionStart::Chosen {
            backend: "cuda".into(),
        },
        TranscriptionStart::Checking,
        TranscriptionStart::Off,
    ] {
        let (mut view, _d) = models_view_on(&order);
        let before = view.in_use.transcription_runs_on;
        apply_start(&mut view, &start(quiet));
        assert_eq!(view.in_use.notes.len(), notes);
        assert_eq!(view.in_use.transcription_runs_on, before);
    }
}

#[test]
fn cuda_settings_restart_is_refused_while_anything_runs_and_then_holds_the_gate() {
    let gate = WorkGate::new();
    let meeting = gate.enter_meeting().unwrap();
    let e = claim_for_restart(&gate, false).unwrap_err();
    assert_eq!(e.kind, "busy");
    assert_eq!(e.message, RESTART_BUSY);
    drop(meeting);
    let job = gate.enter_work().unwrap();
    assert_eq!(claim_for_restart(&gate, false).unwrap_err().kind, "busy");
    drop(job);
    let check = gate.enter_check().unwrap();
    assert_eq!(claim_for_restart(&gate, false).unwrap_err().kind, "busy");
    drop(check);
    // A download would be cut, and that is refused whatever else is idle.
    let e = claim_for_restart(&gate, true).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), e.message.as_str()),
        ("busy", RESTART_DOWNLOADING)
    );
    assert_eq!(
        gate.enter_work().map(drop),
        Ok(()),
        "a refusal takes no pass"
    );
    claim_for_restart(&gate, false).unwrap();
    // Nothing long starts between the claim and the end of the process.
    assert_eq!(gate.enter_meeting().unwrap_err().kind, "busy");
}

#[test]
fn cuda_settings_terms_open_only_fixed_nvidia_pages() {
    for terms in [CudaTerms::Cuda, CudaTerms::Cudnn] {
        let link = terms.link();
        assert!(link.url.starts_with("https://docs.nvidia.com/"), "{link:?}");
        let (program, args) = open_page_command(&link.url);
        assert!(program.ends_with("explorer.exe"), "{program:?}");
        assert_eq!(args, [OsString::from(&link.url)]);
    }
}

#[test]
fn cuda_settings_nothing_the_page_shows_holds_a_developer_command() {
    let start = CudaStart::off();
    let (view, _d) = view_on(&desktop(), &NoCudaFilesYet, &start);
    let json = serde_json::to_string(&view).unwrap();
    assert!(!crate::plain_text::names_developer_tool(&json), "{json}");
    assert_eq!(earshot_runtime::catalogue::manifest_id_in(&json), None);
}
