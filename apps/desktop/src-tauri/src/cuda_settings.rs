//! The CUDA section of Settings, Models (T8.4af) and what the switch does at start.
//!
//! The rules are `earshot_runtime::cuda_switch`'s; this module shapes them for the page, runs the
//! parts' downloads and removals ([`CudaJobs`], the only path to either) and applies both settings
//! once, at start ([`begin`]): a DLL cannot be unloaded, so a change waits for the next start and
//! the page offers a restart.
//!
//! **At start**: for summaries, verify the part, try the folder in a throwaway child
//! (`llama_probe`), load it. For transcription, try the part in a child (`transcription_probe`,
//! which checks every file first), then choose it (`cuda::use_for_transcription`, which checks them
//! again), and ONNX Runtime then loads from it. That can take minutes, so it never holds the window or the tray: it runs on its own
//! thread behind the two gates, `host::hold_llama` (every llama call passes it) and
//! `cuda::hold_onnx` (every ONNX Runtime load passes it, and anything about to read the order for
//! one: `host::settled_order`). So nothing loads either library before the choice is made.
//! **A recording never waits**, because a setting never costs a recording: a
//! meeting that starts meanwhile records at once and holds its models until the choice lands
//! (T9.1l, `earshot_runtime::meeting_queue`), so it keeps CUDA. A trial that hangs is ended by
//! [`START_TRIAL_LIMIT`]; until then the meeting's audio queues. A summary can wait at llama.cpp's
//! gate too, which costs no audio.
//! [`CudaLatch`] carries the answer: until then `models()` and this section answer at once with
//! [`SummariesStart::Checking`] and [`TranscriptionStart::Checking`], and
//! [`CudaEvent::StartChecked`] tells the page to read again. Anything but a load stays on what the
//! app has beside its exe and says so in one note per part, never silently and never a CPU run
//! labelled CUDA. What the trials found stays in memory for this run; nothing about it is stored.
//!
//! What runs is read from that evidence ([`apply_start`] on `ModelsView.in_use`, and
//! `accelerator_order`, which holds CUDA only after the choice), not from the setting.
//!
//! **The model check** tests what this run can use, so it is CUDA's only in a run that chose it:
//! its child repeats the choice (`check_child`). The choice changes the check's fingerprint, so
//! the first start that chooses CUDA finds the stored answer out of date and runs the check
//! ([`check_after_start`]), which gives way to a meeting: the meeting starts and the check is
//! cancelled. That is the only check CUDA starts: a finished download starts
//! none, because its switch can only be turned on once the files are there and it applies at the
//! next start.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use earshot_core::asr::CancelToken;
use earshot_runtime::Services;
use earshot_runtime::backends::{Accelerator, ModelDir, accelerator_order};
use earshot_runtime::cuda::{self, CudaPart};
use earshot_runtime::cuda_switch::{
    self, CudaFiles, CudaFilesError, CudaSwitch, CudaSwitchError, MEASURED_CPU_SAVED, PartRemoved,
    PartState, removal_refused, set_cuda_switch,
};
use earshot_runtime::earshot_hardware::{self, Hardware};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, State};

use crate::activity::WorkGate;
use crate::ipc::{AppState, IpcError, blocking};
use crate::llama_probe::{self, Verdict};
use crate::meeting::{DriverPhase, MeetingDriver};
use crate::model_jobs::ModelJobs;
use crate::models::{Installed, ModelsView, RunsOn};
use crate::plain_text::plain;

#[cfg(test)]
#[path = "cuda_settings_tests.rs"]
mod tests;

/// One part, as the page names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Type)]
pub enum CudaPartChoice {
    /// Summaries on CUDA.
    Summaries,
    /// Transcription on CUDA.
    Transcription,
}

impl CudaPartChoice {
    fn part(self) -> CudaPart {
        match self {
            Self::Summaries => CudaPart::Summaries,
            Self::Transcription => CudaPart::Transcription,
        }
    }
}

/// Whose terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum CudaTerms {
    /// The CUDA EULA (cuBLAS is in its Attachment A).
    Cuda,
    /// The cuDNN licence.
    Cudnn,
}

impl CudaTerms {
    /// The title and the address docs/cuda-libraries.md names. The only pages this app opens.
    #[must_use]
    pub fn link(self) -> TermsLink {
        let (title, url) = match self {
            Self::Cuda => (
                "NVIDIA CUDA EULA",
                "https://docs.nvidia.com/cuda/eula/index.html",
            ),
            Self::Cudnn => (
                "NVIDIA cuDNN licence",
                "https://docs.nvidia.com/deeplearning/cudnn/sla/index.html",
            ),
        };
        TermsLink {
            terms: self,
            title: title.to_owned(),
            url: url.to_owned(),
        }
    }
}

/// NVIDIA's terms for a part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct TermsLink {
    /// Which ([`open_cuda_terms`] takes this, never an address).
    pub terms: CudaTerms,
    /// What the page calls them.
    pub title: String,
    /// The address, shown in full.
    pub url: String,
}

/// The two settings as the UI sends and reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct CudaSwitchValue {
    /// Summaries run on CUDA.
    pub summaries: bool,
    /// Transcription runs on CUDA (needs `summaries`).
    pub transcription: bool,
}

impl From<CudaSwitch> for CudaSwitchValue {
    fn from(s: CudaSwitch) -> Self {
        Self {
            summaries: s.summaries,
            transcription: s.transcription,
        }
    }
}

impl From<CudaSwitchValue> for CudaSwitch {
    fn from(s: CudaSwitchValue) -> Self {
        Self {
            summaries: s.summaries,
            transcription: s.transcription,
        }
    }
}

/// One part on the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct CudaPartView {
    /// Which.
    pub part: CudaPartChoice,
    /// Why it cannot be downloaded yet; `null` when it can.
    pub not_available: Option<String>,
    /// Bytes to download.
    pub download_size: u64,
    /// Bytes on disk once installed.
    pub disk_size: u64,
    /// Bytes held.
    pub held: u64,
    /// Install state.
    pub installed: Installed,
    /// It is downloading now.
    pub downloading: bool,
    /// Its setting.
    pub on: bool,
    /// Why Remove is refused now; `null` when it is not.
    pub removal_refused: Option<String>,
    /// The terms accepted before its download.
    pub terms: Vec<TermsLink>,
}

/// What this run of the app does, from what the start found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct CudaRunView {
    /// The trial at start ran on CUDA and the folder is loaded.
    pub summaries_on_cuda: bool,
    /// The start is still verifying and trying the files; `CudaEvent::StartChecked` follows.
    pub checking: bool,
}

/// The CUDA section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct CudaView {
    /// Whether the section is shown at all.
    pub offered: bool,
    /// The card that earned the offer.
    pub card: Option<String>,
    /// Summaries, then transcription.
    pub summaries: CudaPartView,
    /// Transcription.
    pub transcription: CudaPartView,
    /// The stored settings.
    pub switch: CudaSwitchValue,
    /// One line per stored value this version does not know.
    pub unknown: Vec<String>,
    /// What this run does.
    pub this_run: CudaRunView,
    /// The stored settings differ from what this run started with.
    pub restart_needed: bool,
    /// Why no restart now (a meeting or a download is on); `null` when one can happen.
    pub restart_blocked: Option<String>,
    /// What summaries run on without this part, in a sentence.
    pub summaries_stay_on: String,
    /// The measured CPU saving, once there is one (`cuda_switch::MEASURED_CPU_SAVED`).
    pub transcription_cpu_saved: Option<String>,
}

/// What the start did about summaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummariesStart {
    /// The setting is off.
    Off,
    /// The start has not finished: the files are being verified and tried.
    Checking,
    /// The folder is loaded; the trial ran on this backend.
    Loaded {
        /// By the trial's own load evidence.
        backend: String,
    },
    /// The setting is on and the app stayed where it was.
    Stayed {
        /// The one note the user sees.
        note: String,
    },
}

/// What the start did about transcription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionStart {
    /// The setting is off.
    Off,
    /// The start has not finished.
    Checking,
    /// The trial ran on CUDA and this run chose it: ONNX Runtime loads from the folder.
    Chosen {
        /// By the trial's own placement evidence.
        backend: String,
    },
    /// The setting is on and this run stays off CUDA.
    Stayed {
        /// Why, for the one note the user sees (`transcription_note`).
        why: String,
    },
}

/// What the start found, for this run only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CudaStart {
    /// The settings it read.
    pub switch: CudaSwitch,
    /// What it did about summaries.
    pub summaries: SummariesStart,
    /// What it did about transcription.
    pub transcription: TranscriptionStart,
}

impl CudaStart {
    /// No store, or nothing switched on.
    #[must_use]
    pub fn off() -> Self {
        Self {
            switch: CudaSwitch::default(),
            summaries: SummariesStart::Off,
            transcription: TranscriptionStart::Off,
        }
    }

    /// Whether the start is still working.
    #[must_use]
    pub fn checking(&self) -> bool {
        self.summaries == SummariesStart::Checking
            || self.transcription == TranscriptionStart::Checking
    }

    /// The parts whose libraries this run holds or may hold, from what the start found: ONNX
    /// Runtime loads its library when a meeting needs it.
    fn loaded(&self) -> Vec<CudaPart> {
        let mut out = Vec::new();
        if matches!(self.summaries, SummariesStart::Loaded { .. }) {
            out.push(CudaPart::Summaries);
        }
        if matches!(self.transcription, TranscriptionStart::Chosen { .. }) {
            out.push(CudaPart::Transcription);
        }
        out
    }
}

/// The answer of the start, once there is one.
#[derive(Debug)]
pub struct CudaLatch {
    /// The settings the start read, known at once.
    switch: CudaSwitch,
    start: Mutex<Option<Arc<CudaStart>>>,
    resolved: Condvar,
}

impl CudaLatch {
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Arc<CudaStart>>> {
        self.start.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pending(switch: CudaSwitch) -> Self {
        Self {
            switch,
            start: Mutex::new(None),
            resolved: Condvar::new(),
        }
    }

    fn resolved_with(start: CudaStart) -> Arc<Self> {
        let latch = Self::pending(start.switch);
        *latch.slot() = Some(Arc::new(start));
        Arc::new(latch)
    }

    /// No store: nothing was switched on.
    #[must_use]
    pub fn resolved_off() -> Arc<Self> {
        Self::resolved_with(CudaStart::off())
    }

    fn resolve(&self, start: CudaStart) {
        *self.slot() = Some(Arc::new(start));
        self.resolved.notify_all();
    }

    /// What the start has found so far, at once: `Checking` until it finished.
    #[must_use]
    pub fn current(&self) -> Arc<CudaStart> {
        self.slot().clone().unwrap_or_else(|| {
            Arc::new(CudaStart {
                switch: self.switch,
                summaries: SummariesStart::Checking,
                transcription: if self.switch.transcription {
                    TranscriptionStart::Checking
                } else {
                    TranscriptionStart::Off
                },
            })
        })
    }

    /// What the start found, waiting for it.
    pub fn wait(&self) -> Arc<CudaStart> {
        let mut slot = self.slot();
        loop {
            if let Some(start) = slot.as_ref() {
                return Arc::clone(start);
            }
            slot = self
                .resolved
                .wait(slot)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Runs `work` on its own thread with `hold` alive for all of it, a panic included; the hold ends
/// before the latch resolves, so whoever hears the answer finds both libraries open. `resolved`
/// runs last, also when the thread cannot start.
fn latched<H: Send + 'static>(
    switch: CudaSwitch,
    hold: H,
    stays: String,
    resolved: impl FnOnce() + Send + 'static,
    work: impl FnOnce() -> (SummariesStart, TranscriptionStart) + Send + 'static,
) -> Arc<CudaLatch> {
    latched_on(
        |job| {
            std::thread::Builder::new()
                .name("earshot-cuda-start".into())
                .spawn(job)
                .map(drop)
        },
        switch,
        hold,
        stays,
        resolved,
        work,
    )
}

/// [`latched`] with what starts its thread.
fn latched_on<H: Send + 'static>(
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    switch: CudaSwitch,
    hold: H,
    stays: String,
    resolved: impl FnOnce() + Send + 'static,
    work: impl FnOnce() -> (SummariesStart, TranscriptionStart) + Send + 'static,
) -> Arc<CudaLatch> {
    let refused = move |why: &str, stays: &str| {
        let transcription = if switch.transcription {
            TranscriptionStart::Stayed {
                why: why.to_owned(),
            }
        } else {
            TranscriptionStart::Off
        };
        (stayed(why, stays), transcription)
    };
    let latch = Arc::new(CudaLatch::pending(switch));
    // With whichever of the thread and this function ends up telling.
    let resolved: Box<dyn FnOnce() + Send> = Box::new(resolved);
    let resolved = Arc::new(Mutex::new(Some(resolved)));
    let tell = |slot: &Mutex<Option<Box<dyn FnOnce() + Send>>>| {
        let told = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(told) = told {
            told();
        }
    };
    let (answer, why, told) = (Arc::clone(&latch), stays.clone(), Arc::clone(&resolved));
    let spawned = spawn(Box::new(move || {
        let (summaries, transcription) = std::panic::catch_unwind(AssertUnwindSafe(work))
            .unwrap_or_else(|_| refused("the start stopped unexpectedly", &why));
        drop(hold);
        answer.resolve(CudaStart {
            switch,
            summaries,
            transcription,
        });
        tell(&told);
    }));
    // A thread that cannot start drops the closure, and with it the hold.
    if spawned.is_err() {
        let (summaries, transcription) = refused("the start could not run", &stays);
        latch.resolve(CudaStart {
            switch,
            summaries,
            transcription,
        });
        tell(&resolved);
    }
    latch
}

/// What summaries run on without the folder, for this machine's settings. A failed read says the
/// processor, the one thing that always works.
fn summaries_stay_on(
    services: &Services,
    order: &[Accelerator],
    hardware: &Hardware,
) -> &'static str {
    let placement =
        earshot_runtime::host::local_summary_placement(&services.store(), order, hardware);
    stays_on(order, placement.is_ok_and(|p| p.gpu))
}

/// Starts applying both settings for this run and returns at once. Call it once, before anything
/// that may call llama.cpp or load ONNX Runtime: when a setting is on, its gate closes here. Off,
/// it costs one store read and no thread. A failed store read is off: the page reads the same store
/// and reports it. `resolved` runs when a start that had to work has finished.
#[must_use]
pub fn begin(
    services: &Services,
    files: Arc<dyn CudaFiles>,
    resolved: impl FnOnce() + Send + 'static,
) -> Arc<CudaLatch> {
    let models = earshot_runtime::host::model_dir(&services.data);
    begin_in(services, files, models, resolved)
}

/// [`begin`] over the models folder `models`.
fn begin_in(
    services: &Services,
    files: Arc<dyn CudaFiles>,
    models: ModelDir,
    resolved: impl FnOnce() + Send + 'static,
) -> Arc<CudaLatch> {
    let read = cuda_switch::cuda_switch(&services.store());
    let Ok((switch, _)) = read else {
        return CudaLatch::resolved_with(CudaStart::off());
    };
    if !switch.summaries {
        return CudaLatch::resolved_with(CudaStart {
            switch,
            summaries: SummariesStart::Off,
            transcription: TranscriptionStart::Off,
        });
    }
    // Transcription cannot be on without summaries (`CudaSwitch::validate`).
    let hold = (
        earshot_runtime::host::hold_llama(),
        switch.transcription.then(cuda::hold_onnx),
    );
    let hardware = earshot_runtime::hardware();
    let stays = summaries_stay_on(services, &accelerator_order(), &hardware);
    latched(switch, hold, stays.to_owned(), resolved, move || {
        let offered = cuda_switch::offered(&hardware);
        let summaries = decide_summaries(
            offered,
            &models,
            &*files,
            earshot_runtime::host::smallest_installed_summary_model(&models),
            stays,
            &|folder, model| llama_probe::try_folder_within(folder, model, START_TRIAL_LIMIT),
            &|folder| earshot_runtime::host::load_llama_folder(folder).map_err(|e| e.to_string()),
        );
        let transcription = if switch.transcription {
            decide_transcription(
                transcription_refusal(
                    offered,
                    cuda_switch::transcription_fits_card(&hardware),
                    cuda::provider_built(),
                    cuda::has_trial_model(&models),
                ),
                &|| crate::transcription_probe::trial_within(&models, START_TRIAL_LIMIT),
                &|| cuda::use_for_transcription(&models, true).map_err(|e| unavailable_text(&e)),
            )
        } else {
            TranscriptionStart::Off
        };
        (summaries, transcription)
    })
}

/// A refused choice's reason, without the error type's own words around it.
fn unavailable_text(e: &earshot_runtime::RuntimeError) -> String {
    match e {
        earshot_runtime::RuntimeError::Unavailable(why) => (*why).to_owned(),
        other => crate::plain_text::runtime_text(other),
    }
}

/// How long the trial may take before the start gives up on it, while a summary waits at the gate.
/// A judgement: 1.8 s measured with the model in the disk cache, a cold disk not.
pub const START_TRIAL_LIMIT: Duration = Duration::from_secs(120);

/// What summaries stay on without the folder, in a sentence: `on_card` is the Models page's
/// placement, so a card too small for the model or "the processor" chosen in Settings says so.
#[must_use]
pub fn stays_on(order: &[Accelerator], on_card: bool) -> &'static str {
    if !on_card {
        return "the processor";
    }
    match order.first() {
        Some(Accelerator::Vulkan) => "Vulkan",
        Some(Accelerator::Cuda) => "the libraries installed with Earshot",
        _ => "the processor",
    }
}

fn stayed(why: &str, stays: &str) -> SummariesStart {
    SummariesStart::Stayed {
        note: plain(format!(
            "Summaries on CUDA did not start this time: {why}. They stay on {stays}."
        )),
    }
}

/// The start's decision over its steps once the setting is on (production: [`begin`]). `model`
/// is what a trial loads, `try_folder` and `load` the two steps that touch llama.cpp.
pub fn decide_summaries(
    offered: bool,
    models: &ModelDir,
    files: &dyn CudaFiles,
    model: Option<PathBuf>,
    stays: &str,
    try_folder: &dyn Fn(&Path, &Path) -> Verdict,
    load: &dyn Fn(&Path) -> Result<(), String>,
) -> SummariesStart {
    if !offered {
        return stayed("this computer's graphics card cannot use them", stays);
    }
    let Some(model) = model else {
        return stayed("no summary model is installed to try them with", stays);
    };
    if files.verify(models, CudaPart::Summaries).is_err() {
        return stayed("their files did not pass the check", stays);
    }
    let folder = cuda::folder(models);
    match try_folder(&folder, &model) {
        Verdict::Refused { why } => stayed(&why, stays),
        Verdict::Usable { backend } if backend != "cuda" => {
            stayed(&format!("the trial ran on {backend}, not on CUDA"), stays)
        }
        Verdict::Usable { backend } => match load(&folder) {
            Ok(()) => SummariesStart::Loaded { backend },
            Err(why) => stayed(&why, stays),
        },
    }
}

/// Why a card is not offered transcription (`cuda_switch::TRANSCRIPTION_MIN_CARD_MEMORY`).
pub const CARD_TOO_SMALL: &str =
    "your graphics card has less than 12 GB of memory, so transcription runs on the processor";

/// Why a developer's `cuda` build never takes the folder for transcription: it runs on its own
/// toolkit (`earshot_runtime::cuda::use_for_transcription` refuses it with the same words).
pub const DEVELOPER_BUILD: &str = "this developer build runs on its own CUDA toolkit";

/// Why transcription cannot even be tried on this machine, this build or with these models.
#[must_use]
pub fn transcription_refusal(
    offered: bool,
    card_fits: bool,
    built: bool,
    has_model: bool,
) -> Option<&'static str> {
    transcription_refusal_in(cfg!(feature = "cuda"), offered, card_fits, built, has_model)
}

/// [`transcription_refusal`] for a build that is (`developer`) or is not a developer's `cuda`
/// build.
#[must_use]
pub fn transcription_refusal_in(
    developer: bool,
    offered: bool,
    card_fits: bool,
    built: bool,
    has_model: bool,
) -> Option<&'static str> {
    if !offered {
        Some("this computer's graphics card cannot use it")
    } else if !card_fits {
        Some(CARD_TOO_SMALL)
    } else if developer {
        Some(DEVELOPER_BUILD)
    } else if !built {
        Some("this version of Earshot cannot use it")
    } else if !has_model {
        Some("no Parakeet model is installed to try it with")
    } else {
        None
    }
}

/// The start's decision about transcription once the setting is on (production: [`begin`]):
/// `refusal` from [`transcription_refusal`], `try_folder` and `choose` the steps that touch the
/// folder's libraries, each of which checks every file itself: `try_folder` names only a CUDA run
/// usable (`transcription_probe::verdict`). Nothing here loads ONNX Runtime in this process.
pub fn decide_transcription(
    refusal: Option<&str>,
    try_folder: &dyn Fn() -> Verdict,
    choose: &dyn Fn() -> Result<(), String>,
) -> TranscriptionStart {
    let stayed = |why: &str| TranscriptionStart::Stayed {
        why: plain(why.to_owned()),
    };
    if let Some(why) = refusal {
        return stayed(why);
    }
    match try_folder() {
        Verdict::Refused { why } => stayed(&why),
        Verdict::Usable { backend } => match choose() {
            Ok(()) => TranscriptionStart::Chosen { backend },
            Err(why) => stayed(&why),
        },
    }
}

/// The one note of a run that stayed off CUDA for transcription, naming what really runs.
fn transcription_note(view: &ModelsView, why: &str) -> String {
    let part = match view.in_use.transcription_runs_on {
        RunsOn::Processor => "the processor",
        RunsOn::Npu => "the NPU",
        RunsOn::Vulkan => "the graphics card through Vulkan",
        // Only a developer `cuda` build, whose own toolkit keeps it on CUDA.
        RunsOn::Cuda => "the graphics card through this build's own CUDA",
    };
    format!("Transcription on CUDA did not start this time: {why}. It stays on {part}.")
}

/// `view` with what this run really does: summaries on the graphics card say CUDA only when the
/// trial ran there and the folder is loaded; transcription says it through the order, which holds
/// CUDA only after the choice. A refusal is one note per part. Never from the setting.
pub fn apply_start(view: &mut ModelsView, start: &CudaStart) {
    match &start.summaries {
        // Until the start has finished, what runs is what the app has beside its exe.
        SummariesStart::Off | SummariesStart::Checking => {}
        SummariesStart::Stayed { note } => view.in_use.notes.push(note.clone()),
        SummariesStart::Loaded { .. } => {
            let cuda_if_on_card = |on: &mut RunsOn| {
                if matches!(on, RunsOn::Vulkan | RunsOn::Cuda) {
                    *on = RunsOn::Cuda;
                }
            };
            cuda_if_on_card(&mut view.in_use.summary_runs_on);
            cuda_if_on_card(&mut view.summary_recommended.runs_on);
            for item in &mut view.summary {
                cuda_if_on_card(&mut item.runs_on);
            }
        }
    }
    if let TranscriptionStart::Stayed { why } = &start.transcription {
        let note = transcription_note(view, why);
        view.in_use.notes.push(plain(note));
    }
}

/// Whether this run chose CUDA for transcription.
#[must_use]
pub fn chose_cuda(start: &CudaStart) -> bool {
    matches!(start.transcription, TranscriptionStart::Chosen { .. })
}

/// Whether the model check should run for this start: this run chose CUDA, the transcription model
/// is the check's to pick, and the stored answer is not for what this run can use.
#[must_use]
pub fn check_follows_start(start: &CudaStart, automatic: bool, check_current: bool) -> bool {
    chose_cuda(start) && automatic && !check_current
}

/// Whether the transcription model is the check's to pick and whether the stored check holds for
/// this run. Reads the hardware with the store lock free.
pub(crate) fn check_state(
    services: &Services,
    models: &ModelDir,
) -> Result<(bool, bool), IpcError> {
    use earshot_runtime::model_bench::{check_fingerprint, hardware_facts, stored_check};
    use earshot_runtime::model_settings::{ModelPick, model_settings};
    let (settings, _) = model_settings(&services.store())?;
    let fingerprint = check_fingerprint(models, &hardware_facts(), &settings.language_refs());
    let current = stored_check(&services.store())?.is_some_and(|c| c.current(&fingerprint));
    Ok((settings.transcription == ModelPick::Automatic, current))
}

/// Once the start has resolved, runs the model check if [`check_follows_start`] says so (module
/// docs); a refusal is one `Warning`, as after a download. Off, it costs no thread.
pub fn check_after_start(
    latch: Arc<CudaLatch>,
    services: Services,
    checks: ModelJobs,
    sink: CudaSink,
) {
    if !latch.current().switch.transcription {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("earshot-cuda-check".into())
        .spawn(move || {
            let start = latch.wait();
            let models = earshot_runtime::host::model_dir(&services.data);
            let follows = check_state(&services, &models)
                .is_ok_and(|(automatic, current)| check_follows_start(&start, automatic, current));
            if follows && let Err(e) = checks.start_check_giving_way(&services) {
                sink(CudaEvent::Warning {
                    part: CudaPartChoice::Transcription,
                    message: crate::model_jobs::check_not_started_message(&e.message),
                });
            }
        });
    // Nothing waits for it: the Models page says the check is out of date and offers it.
    drop(spawned);
}

fn installed(state: PartState) -> Installed {
    match state {
        PartState::Installed => Installed::Yes,
        PartState::Partial => Installed::Partly,
        PartState::Missing => Installed::No,
    }
}

fn terms_of(part: CudaPart) -> Vec<TermsLink> {
    match part {
        CudaPart::Summaries => vec![CudaTerms::Cuda.link()],
        CudaPart::Transcription => vec![CudaTerms::Cuda.link(), CudaTerms::Cudnn.link()],
    }
}

/// What the section reads about this machine.
#[derive(Clone, Copy)]
pub struct CudaHost<'a> {
    /// The models folder.
    pub models: &'a ModelDir,
    /// This machine's hardware.
    pub hardware: &'a Hardware,
    /// This build's accelerator order.
    pub order: &'a [Accelerator],
    /// The parts' files.
    pub files: &'a dyn CudaFiles,
}

/// Why no restart now.
#[must_use]
pub fn restart_blocked(meeting_on: bool, downloading: bool) -> Option<&'static str> {
    if meeting_on {
        Some(RESTART_MEETING)
    } else if downloading {
        Some(RESTART_DOWNLOADING)
    } else {
        None
    }
}

/// Everything the section shows.
///
/// # Errors
/// Store errors.
pub fn cuda_view(
    services: &Services,
    host: CudaHost<'_>,
    start: &CudaStart,
    downloading: &dyn Fn(CudaPart) -> bool,
    blocked: Option<&str>,
) -> Result<CudaView, IpcError> {
    let CudaHost {
        models,
        hardware,
        order,
        files,
    } = host;
    let (stored, unknown) = cuda_switch::cuda_switch(&services.store())?;
    let transcription_state = files.part_files(models, CudaPart::Transcription).state;
    let loaded = start.loaded();
    let checking = start.checking();
    let part = |choice: CudaPartChoice| {
        let p = choice.part();
        let f = files.part_files(models, p);
        CudaPartView {
            part: choice,
            not_available: if p == CudaPart::Transcription
                && !cuda_switch::transcription_fits_card(hardware)
            {
                Some(plain(CARD_TOO_SMALL.to_owned()))
            } else {
                f.not_available.map(plain)
            },
            download_size: f.download_size,
            disk_size: f.disk_size,
            held: f.held,
            installed: installed(f.state),
            downloading: downloading(p),
            on: stored.is_on(p),
            removal_refused: if checking {
                Some(CUDA_CHECKING.to_owned())
            } else {
                removal_refused(p, stored, &loaded, transcription_state)
                    .map(|r| r.copy().to_owned())
            },
            terms: terms_of(p),
        }
    };
    Ok(CudaView {
        offered: cuda_switch::offered(hardware),
        card: earshot_hardware::cuda_card(hardware).map(|g| g.name.clone()),
        summaries: part(CudaPartChoice::Summaries),
        transcription: part(CudaPartChoice::Transcription),
        switch: stored.into(),
        unknown: unknown.iter().map(ToString::to_string).collect(),
        this_run: CudaRunView {
            summaries_on_cuda: matches!(start.summaries, SummariesStart::Loaded { .. }),
            checking,
        },
        restart_needed: stored != start.switch,
        restart_blocked: blocked.map(str::to_owned),
        summaries_stay_on: summaries_stay_on(services, order, hardware).to_owned(),
        transcription_cpu_saved: MEASURED_CPU_SAVED.map(str::to_owned),
    })
}

fn refused(kind: &str, message: impl Into<String>) -> IpcError {
    IpcError {
        kind: kind.into(),
        message: message.into(),
        retryable: false,
    }
}

fn busy(message: &str) -> IpcError {
    IpcError {
        kind: "busy".into(),
        message: message.into(),
        retryable: true,
    }
}

/// Checks and stores the settings. Off is always allowed and touches no file; on needs the offer
/// and the part's files. A failed change stores nothing.
///
/// # Errors
/// `invalid_setting`; store errors.
pub fn set_switch(
    services: &Services,
    models: &ModelDir,
    hardware: &Hardware,
    files: &dyn CudaFiles,
    value: CudaSwitchValue,
) -> Result<CudaSwitchValue, IpcError> {
    let wanted: CudaSwitch = value.into();
    let (stored, _) = cuda_switch::cuda_switch(&services.store())?;
    for part in CudaPart::ALL {
        if wanted.is_on(part) && !stored.is_on(part) {
            if !cuda_switch::offered(hardware) {
                return Err(refused(
                    "invalid_setting",
                    "This computer's graphics card cannot use the CUDA libraries.",
                ));
            }
            if files.part_files(models, part).state != PartState::Installed {
                return Err(refused(
                    "invalid_setting",
                    "Download this part first, then switch it on.",
                ));
            }
        }
    }
    let stored = set_cuda_switch(&mut services.store(), wanted).map_err(|e| match e {
        CudaSwitchError::Store(e) => IpcError::from(e),
        other => refused("invalid_setting", plain(format!("{other}."))),
    })?;
    Ok(stored.into())
}

/// One event of the CUDA section. For a download: `Progress` steps, then exactly one of
/// `Downloaded`, `Cancelled` or `Failed`, and no model check follows. `StartChecked` and `Warning`
/// (the model check after a start did not run, and why) belong to no download.
#[derive(Debug, Clone, PartialEq, Serialize, Type, tauri_specta::Event)]
#[serde(tag = "type")]
pub enum CudaEvent {
    /// The start has finished with the files: read the page again.
    StartChecked,
    /// Bytes held.
    Progress {
        /// Which.
        part: CudaPartChoice,
        /// Bytes held.
        done: u64,
        /// Bytes in all.
        total: u64,
    },
    /// Every file is downloaded and verified.
    Downloaded {
        /// Which.
        part: CudaPartChoice,
    },
    /// Cancelled; what arrived is kept for a resume.
    Cancelled {
        /// Which.
        part: CudaPartChoice,
    },
    /// Something the user is told, not an end.
    Warning {
        /// Which.
        part: CudaPartChoice,
        /// Plain copy.
        message: String,
    },
    /// It failed.
    Failed {
        /// Which.
        part: CudaPartChoice,
        /// Plain copy.
        message: String,
        /// Whether starting it again can succeed.
        retryable: bool,
    },
}

/// Where CUDA events go.
pub type CudaSink = Arc<dyn Fn(CudaEvent) + Send + Sync>;

/// What a download that panicked reports.
pub const CUDA_PANIC_MESSAGE: &str = "The download stopped unexpectedly. Try again.";

struct Download {
    cancel: CancelToken,
    thread: JoinHandle<()>,
}

struct Inner {
    files: Arc<dyn CudaFiles>,
    sink: CudaSink,
    checks: ModelJobs,
    downloads: Mutex<BTreeMap<CudaPartChoice, Download>>,
}

/// Runs the parts' downloads and removals, the only path to either (cheap to clone; clones share
/// them). Lock order: `ModelJobs`' check slot ([`ModelJobs::without_check`]), then this one's
/// downloads.
#[derive(Clone)]
pub struct CudaJobs(Arc<Inner>);

impl CudaJobs {
    /// A runner over `files`, sending events to `sink`; `checks` keeps downloads and removals away
    /// from a running model check.
    pub fn new(files: Arc<dyn CudaFiles>, sink: CudaSink, checks: ModelJobs) -> Self {
        let sink: CudaSink = Arc::new(move |e| sink(crate::plain_text::plain_cuda_event(e)));
        Self(Arc::new(Inner {
            files,
            sink,
            checks,
            downloads: Mutex::new(BTreeMap::new()),
        }))
    }

    /// The files.
    #[must_use]
    pub fn files(&self) -> &dyn CudaFiles {
        &*self.0.files
    }

    fn downloads(&self) -> std::sync::MutexGuard<'_, BTreeMap<CudaPartChoice, Download>> {
        self.0
            .downloads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `part` downloads.
    #[must_use]
    pub fn downloading(&self, part: CudaPart) -> bool {
        self.downloads()
            .iter()
            .any(|(p, d)| p.part() == part && !d.thread.is_finished())
    }

    /// Starts downloading `part` and returns once it runs. Never before NVIDIA's terms were
    /// accepted, on a machine not offered it, or for the transcription part without the summaries
    /// part's files.
    ///
    /// # Errors
    /// `terms_not_accepted`, `not_offered`, `not_available`, `needs_summaries`; `busy` (retryable)
    /// while it downloads or the model check runs.
    pub fn start_download(
        &self,
        services: &Services,
        hardware: &Hardware,
        part: CudaPartChoice,
        accepted_terms: bool,
    ) -> Result<(), IpcError> {
        if !accepted_terms {
            return Err(refused(
                "terms_not_accepted",
                "Accept NVIDIA's terms before this download.",
            ));
        }
        if !cuda_switch::offered(hardware) {
            return Err(refused(
                "not_offered",
                "This computer's graphics card cannot use the CUDA libraries.",
            ));
        }
        let models = earshot_runtime::host::model_dir(&services.data);
        if let Some(why) = self.files().part_files(&models, part.part()).not_available {
            return Err(refused("not_available", plain(why)));
        }
        if let Some(needed) = part.part().needs()
            && self.files().part_files(&models, needed).state != PartState::Installed
        {
            return Err(refused(
                "needs_summaries",
                "Download summaries on CUDA first: this part needs its files.",
            ));
        }
        self.0.checks.without_check(|| {
            let mut downloads = self.downloads();
            downloads.retain(|_, d| !d.thread.is_finished());
            if downloads.contains_key(&part) {
                return Err(busy("This part is already downloading."));
            }
            let cancel = CancelToken::new();
            let (jobs, svc, token) = (self.clone(), services.clone(), cancel.clone());
            let thread = std::thread::Builder::new()
                .name("earshot-cuda-download".into())
                .spawn(move || jobs.run_download(&svc, part, &token))
                .map_err(|e| refused("internal", format!("cannot start the download: {e}")))?;
            downloads.insert(part, Download { cancel, thread });
            Ok(())
        })
    }

    fn run_download(&self, services: &Services, part: CudaPartChoice, cancel: &CancelToken) {
        let sink = &*self.0.sink;
        let models = earshot_runtime::host::model_dir(&services.data);
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.files()
                .download(&models, part.part(), cancel, &mut |done, total| {
                    sink(CudaEvent::Progress { part, done, total });
                })
        }));
        match result {
            Ok(Ok(())) => sink(CudaEvent::Downloaded { part }),
            Ok(Err(CudaFilesError::Cancelled)) => sink(CudaEvent::Cancelled { part }),
            Ok(Err(CudaFilesError::Failed { message, retryable })) => sink(CudaEvent::Failed {
                part,
                message,
                retryable,
            }),
            Err(_) => sink(CudaEvent::Failed {
                part,
                message: CUDA_PANIC_MESSAGE.to_owned(),
                retryable: true,
            }),
        }
    }

    /// Cancels the download of `part`; its `Cancelled` follows.
    ///
    /// # Errors
    /// `not_downloading`.
    pub fn cancel_download(&self, part: CudaPartChoice) -> Result<(), IpcError> {
        match self.downloads().get(&part) {
            Some(d) if !d.thread.is_finished() => {
                d.cancel.cancel();
                Ok(())
            }
            _ => Err(refused("not_downloading", "This part is not downloading.")),
        }
    }

    /// Whether any part or any model downloads: what a restart would cut.
    #[must_use]
    pub fn any_downloading(&self) -> bool {
        self.downloads().values().any(|d| !d.thread.is_finished())
            || self.0.checks.any_downloading()
    }

    /// [`set_switch`] with no removal running: a part cannot be switched on while it is removed.
    ///
    /// # Errors
    /// As [`set_switch`].
    pub fn set_switch(
        &self,
        services: &Services,
        models: &ModelDir,
        hardware: &Hardware,
        value: CudaSwitchValue,
    ) -> Result<CudaSwitchValue, IpcError> {
        let _downloads = self.downloads();
        set_switch(services, models, hardware, self.files(), value)
    }

    /// Removes `part`'s files. The settings are not touched; they are read inside the critical
    /// section, which [`Self::set_switch`] shares.
    ///
    /// # Errors
    /// `part_in_use` (`cuda_switch::removal_refused`); `busy` while either part downloads (they
    /// share a folder, and transcription's run needs summaries' files), the model check, a meeting
    /// or a meeting job runs (it may have the files mapped) or the start is still trying the files;
    /// the files' failure. The check slot, the gate's pass and the downloads lock stop any of them
    /// starting until the removal has finished.
    pub fn remove(
        &self,
        services: &Services,
        start: &CudaStart,
        part: CudaPartChoice,
    ) -> Result<CudaRemoved, IpcError> {
        if start.checking() {
            return Err(busy(CUDA_CHECKING));
        }
        let models = earshot_runtime::host::model_dir(&services.data);
        self.0.checks.without_check_or_work(|| {
            let downloads = self.downloads();
            if downloads.values().any(|d| !d.thread.is_finished()) {
                return Err(busy(CUDA_DOWNLOADING_MESSAGE));
            }
            let dependent = self
                .files()
                .part_files(&models, CudaPart::Transcription)
                .state;
            let (stored, _) = cuda_switch::cuda_switch(&services.store())?;
            if let Some(why) = removal_refused(part.part(), stored, &start.loaded(), dependent) {
                return Err(refused("part_in_use", why.copy()));
            }
            self.files()
                .remove(&models, part.part())
                .map(CudaRemoved::from)
                .map_err(|e| match e {
                    CudaFilesError::Failed { message, retryable } => IpcError {
                        kind: "cuda_files".into(),
                        message: plain(message),
                        retryable,
                    },
                    CudaFilesError::Cancelled => {
                        refused("cuda_files", "The removal was cancelled.")
                    }
                })
        })
    }

    /// Waits for every download.
    #[cfg(test)]
    pub fn join(&self) {
        let all: Vec<Download> = std::mem::take(&mut *self.downloads())
            .into_values()
            .collect();
        for d in all {
            let _ = d.thread.join();
        }
    }
}

/// What a part's removal did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct CudaRemoved {
    /// Bytes freed.
    pub bytes_freed: u64,
    /// What is still in the folder that belongs to no part (never deleted).
    pub left_in_folder: Vec<String>,
}

impl From<PartRemoved> for CudaRemoved {
    fn from(r: PartRemoved) -> Self {
        Self {
            bytes_freed: r.bytes_freed,
            left_in_folder: r.left_in_folder,
        }
    }
}

/// Why a part's removal is refused while either part downloads.
pub const CUDA_DOWNLOADING_MESSAGE: &str =
    "These files are still downloading. Cancel the download or wait for it, then try again.";

/// Why nothing of the folder may be removed while the start is trying it.
pub const CUDA_CHECKING: &str =
    "Earshot is still trying these files on your card. Try again when that has finished.";

/// What a restart hears while something is running.
pub const RESTART_BUSY: &str =
    "A meeting or another job is running. Restart Earshot when it has finished.";

/// What the restart bar says during a meeting.
pub const RESTART_MEETING: &str = "A meeting is on: restart Earshot when it has finished.";

/// What the restart bar says during a download, which a restart would cut.
pub const RESTART_DOWNLOADING: &str =
    "A download is running: restart Earshot when it has finished, or pause it first.";

/// Takes the gate for good, so nothing long starts between this and the process's end.
///
/// # Errors
/// `busy` while a download runs (`downloading`), or a meeting, a meeting job, the model check, the
/// voice sample or the audio check.
pub fn claim_for_restart(gate: &WorkGate, downloading: bool) -> Result<(), IpcError> {
    if downloading {
        return Err(busy(RESTART_DOWNLOADING));
    }
    let pass = gate.enter_restart().map_err(|_| busy(RESTART_BUSY))?;
    std::mem::forget(pass);
    Ok(())
}

/// The CUDA section of the Models page.
#[tauri::command]
#[specta::specta]
pub async fn cuda_settings(
    state: State<'_, AppState>,
    jobs: State<'_, CudaJobs>,
    start: State<'_, Arc<CudaLatch>>,
    driver: State<'_, MeetingDriver>,
) -> Result<CudaView, IpcError> {
    let services = state.services()?;
    let (jobs, start, driver) = (
        jobs.inner().clone(),
        Arc::clone(start.inner()),
        driver.inner().clone(),
    );
    blocking(move || {
        let start = start.current();
        let models = earshot_runtime::host::model_dir(&services.data);
        let hardware = earshot_runtime::hardware();
        let order = accelerator_order();
        cuda_view(
            &services,
            CudaHost {
                models: &models,
                hardware: &hardware,
                order: &order,
                files: jobs.files(),
            },
            &start,
            &|p| jobs.downloading(p),
            restart_blocked(driver.phase() != DriverPhase::Idle, jobs.any_downloading()),
        )
    })
    .await
}

/// Stores the two settings; they apply at the next start. `invalid_setting`: nothing stored.
#[tauri::command]
#[specta::specta]
pub async fn set_cuda_settings(
    state: State<'_, AppState>,
    jobs: State<'_, CudaJobs>,
    value: CudaSwitchValue,
) -> Result<CudaSwitchValue, IpcError> {
    let services = state.services()?;
    let jobs = jobs.inner().clone();
    blocking(move || {
        let models = earshot_runtime::host::model_dir(&services.data);
        jobs.set_switch(&services, &models, &earshot_runtime::hardware(), value)
    })
    .await
}

/// Starts downloading a part; `CudaEvent`s follow. Refused unless `accepted_terms`.
#[tauri::command]
#[specta::specta]
pub fn start_cuda_download(
    state: State<'_, AppState>,
    jobs: State<'_, CudaJobs>,
    part: CudaPartChoice,
    accepted_terms: bool,
) -> Result<(), IpcError> {
    let services = state.services()?;
    jobs.start_download(
        &services,
        &earshot_runtime::hardware(),
        part,
        accepted_terms,
    )
}

/// Cancels a part's download; what arrived is kept.
#[tauri::command]
#[specta::specta]
pub fn cancel_cuda_download(
    jobs: State<'_, CudaJobs>,
    part: CudaPartChoice,
) -> Result<(), IpcError> {
    jobs.cancel_download(part)
}

/// Removes a part's files.
#[tauri::command]
#[specta::specta]
pub async fn remove_cuda_part(
    state: State<'_, AppState>,
    jobs: State<'_, CudaJobs>,
    start: State<'_, Arc<CudaLatch>>,
    part: CudaPartChoice,
) -> Result<CudaRemoved, IpcError> {
    let services = state.services()?;
    let (jobs, start) = (jobs.inner().clone(), Arc::clone(start.inner()));
    blocking(move || jobs.remove(&services, &start.current(), part)).await
}

/// `explorer.exe` and the address: what opens a page in the user's browser.
fn open_page_command(url: &str) -> (PathBuf, Vec<OsString>) {
    let windows =
        std::env::var_os("SystemRoot").map_or_else(|| "C:\\Windows".into(), PathBuf::from);
    (windows.join("explorer.exe"), vec![OsString::from(url)])
}

/// Opens NVIDIA's terms in the user's browser. Takes a choice, never an address.
#[tauri::command]
#[specta::specta]
pub async fn open_cuda_terms(terms: CudaTerms) -> Result<(), IpcError> {
    blocking(move || {
        let url = terms.link().url;
        let (program, args) = open_page_command(&url);
        let spec = earshot_proc::ChildSpec {
            program,
            args,
            current_dir: std::env::temp_dir(),
            env_remove: Vec::new(),
            env_set: Vec::new(),
            stdin: Vec::new(),
            deadline: Duration::from_secs(10),
            max_output: 4 * 1024,
        };
        // explorer.exe's exit code says nothing about the page, so only a failed start counts.
        match earshot_proc::run(&spec, &|| false) {
            Ok(_) | Err(earshot_proc::ChildError::TimedOut(_)) => Ok(()),
            Err(_) => Err(refused(
                "open_failed",
                format!("The page could not be opened. Its address is {url}"),
            )),
        }
    })
    .await
}

/// Restarts the app, so a changed switch applies; `busy` while something runs.
#[tauri::command]
#[specta::specta]
pub fn restart_app(
    app: AppHandle,
    gate: State<'_, WorkGate>,
    jobs: State<'_, CudaJobs>,
) -> Result<(), IpcError> {
    claim_for_restart(gate.inner(), jobs.any_downloading())?;
    // Through the normal exit, so the single-instance plugin lets the new process start.
    app.request_restart();
    Ok(())
}
