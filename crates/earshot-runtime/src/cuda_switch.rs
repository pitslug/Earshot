//! The CUDA switch of Settings, Models (T8.4af, PLAN T8.4a part 3, docs/cuda-install.md): whether
//! this machine is offered NVIDIA's libraries, the two device-local settings, one per part, and
//! the rules for what may be switched and removed.
//!
//! - **The offer** is [`offered`]: [`earshot_hardware::cuda_card`] on an x64 build, nothing else.
//! - **The settings** are off by default and only ever written by [`set_cuda_switch`], in one
//!   transaction after [`CudaSwitch::validate`]: [`CudaPart::Transcription`] cannot be on without
//!   [`CudaPart::Summaries`], because its files need cuBLAS (`crate::cuda`). A value this build does
//!   not know reads as off and is reported, as `model_settings` does. Switching back is one
//!   setting and never touches a file.
//! - **The files** are behind [`CudaFiles`], so this module knows nothing about where they come
//!   from (the app's implementation: `cuda_files`, over [`crate::cuda`] and the catalogue).
//! - **Removal** has one rule, [`removal_refused`]: a loaded library cannot be deleted on Windows,
//!   and a part another part needs stays until that one goes.
//!
//! Nothing here applies a setting: a DLL cannot be unloaded, so the app reads these once, at
//! start.

use std::fmt;

use earshot_core::asr::CancelToken;
use earshot_hardware::Hardware;
use earshot_store::Store;

use crate::RuntimeError;
use crate::backends::ModelDir;
use crate::cuda::CudaPart;
use crate::local::DeviceRecord;
use crate::model_settings::UnknownSetting;

#[cfg(test)]
#[path = "cuda_switch_tests.rs"]
mod tests;

/// `local_state` key of "summaries on CUDA".
pub const CUDA_SUMMARIES_KEY: &str = "settings/cuda-summaries";
/// `local_state` key of "transcription on CUDA".
pub const CUDA_TRANSCRIPTION_KEY: &str = "settings/cuda-transcription";

const ON: &str = "on";
const OFF: &str = "off";

/// THE SLOT FOR THE MEASURED NUMBER (PLAN T8.4a part 3): how much CPU work the transcription part
/// saves, as a sentence the page shows after "its gain is less processor work". `None` until it
/// is measured on a real meeting replay; never fill it with an estimate. Measured 2026-11-06, RTX
/// 5080, a 6 minute two-track recording replayed as fast as it reads, three runs each way, no
/// summary: 151 s of processor time against 101 s, peak private memory 7.7 GB against 12.4 GB
/// (docs/cuda-install.md).
pub const MEASURED_CPU_SAVED: Option<&str> = Some(
    "Measured on one recording with an RTX 5080: about a third less processor time, and about 5 GB more memory set aside.",
);

/// Whether this machine is offered NVIDIA's libraries. x64 only: the files are x64 builds, and
/// T8.4 decides ARM64.
#[must_use]
pub fn offered(hardware: &Hardware) -> bool {
    offered_on(hardware, cfg!(target_arch = "x86_64"))
}

fn offered_on(hardware: &Hardware, x64: bool) -> bool {
    x64 && earshot_hardware::cuda_card(hardware).is_some()
}

/// A card with less dedicated memory than this transcribes on the CPU: a call
/// runs one session per track, and a meeting took 7.8 GiB of the card with 1.3 GiB already in use,
/// 9.1 GiB in all (docs/live-previews.md). The same model gives the same words on the CPU, in time.
pub const TRANSCRIPTION_MIN_CARD_MEMORY: u64 = 12 * 1024 * 1024 * 1024;

/// Whether this machine's card has room to transcribe a call. A card whose memory cannot be read
/// gets no offer, as one whose capability cannot be read does.
#[must_use]
pub fn transcription_fits_card(hardware: &Hardware) -> bool {
    crate::model_settings::graphics_memory(hardware)
        .is_some_and(|m| m >= TRANSCRIPTION_MIN_CARD_MEMORY)
}

/// The two settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CudaSwitch {
    /// Summaries run from the CUDA folder.
    pub summaries: bool,
    /// Transcription runs on ONNX Runtime's CUDA library.
    pub transcription: bool,
}

impl CudaSwitch {
    /// Whether `part` is on.
    #[must_use]
    pub fn is_on(self, part: CudaPart) -> bool {
        match part {
            CudaPart::Summaries => self.summaries,
            CudaPart::Transcription => self.transcription,
        }
    }

    /// Checks the one rule between the parts.
    ///
    /// # Errors
    /// [`CudaSwitchError::NeedsSummaries`].
    pub fn validate(self) -> Result<Self, CudaSwitchError> {
        if self.transcription && !self.summaries {
            return Err(CudaSwitchError::NeedsSummaries);
        }
        Ok(self)
    }
}

/// Why the settings were refused (nothing was stored).
#[derive(Debug)]
pub enum CudaSwitchError {
    /// Transcription needs the summaries part on.
    NeedsSummaries,
    /// Reading or writing the store failed.
    Store(RuntimeError),
}

impl fmt::Display for CudaSwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsSummaries => {
                write!(
                    f,
                    "Transcription on CUDA needs summaries on CUDA switched on"
                )
            }
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CudaSwitchError {}

fn stored(on: bool) -> &'static str {
    if on { ON } else { OFF }
}

/// The stored settings (absent values are off) and every stored value this build does not know,
/// which reads as off. A stored "transcription on" without summaries reads as off and is
/// reported too: this module never writes that.
///
/// # Errors
/// Store errors.
pub fn cuda_switch(store: &Store) -> Result<(CudaSwitch, Vec<UnknownSetting>), RuntimeError> {
    let mut unknown = Vec::new();
    let mut read = |key: &'static str| -> Result<bool, RuntimeError> {
        let Some(bytes) = store.local_value(key)? else {
            return Ok(false);
        };
        match bytes.as_slice() {
            b"on" => Ok(true),
            b"off" => Ok(false),
            other => {
                unknown.push(UnknownSetting {
                    key,
                    stored: String::from_utf8_lossy(other).into_owned(),
                });
                Ok(false)
            }
        }
    };
    let summaries = read(CUDA_SUMMARIES_KEY)?;
    let mut transcription = read(CUDA_TRANSCRIPTION_KEY)?;
    if transcription && !summaries {
        unknown.push(UnknownSetting {
            key: CUDA_TRANSCRIPTION_KEY,
            stored: ON.to_owned(),
        });
        transcription = false;
    }
    Ok((
        CudaSwitch {
            summaries,
            transcription,
        },
        unknown,
    ))
}

/// Checks and stores both settings in one transaction; returns what was stored.
///
/// # Errors
/// [`CudaSwitchError`]: nothing is stored on a refusal.
pub fn set_cuda_switch(
    store: &mut Store,
    switch: CudaSwitch,
) -> Result<CudaSwitch, CudaSwitchError> {
    let s = switch.validate()?;
    let values = [
        (DeviceRecord::CudaSummaries, stored(s.summaries)),
        (DeviceRecord::CudaTranscription, stored(s.transcription)),
    ];
    store
        .transaction(|tx| -> Result<(), earshot_store::StoreError> {
            for (record, value) in values {
                crate::local::set_tx(tx, &record.key(), value.as_bytes())?;
            }
            Ok(())
        })
        .map_err(|e| CudaSwitchError::Store(RuntimeError::from(e)))?;
    Ok(s)
}

/// How much of a part is on this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartState {
    /// Every file is there.
    Installed,
    /// Some bytes are held (a paused or cut download).
    Partial,
    /// Nothing is held.
    Missing,
}

/// What the page says about one part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartFiles {
    /// Why it cannot be downloaded yet (plain copy); `None` when it can.
    pub not_available: Option<String>,
    /// Bytes to download.
    pub download_size: u64,
    /// Bytes on disk once installed.
    pub disk_size: u64,
    /// Bytes held towards the download.
    pub held: u64,
    /// Install state.
    pub state: PartState,
}

/// How a call on the files failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CudaFilesError {
    /// Cancelled through the token; what arrived is kept for a resume.
    Cancelled,
    /// Failed.
    Failed {
        /// Plain copy, never a URL or a file name.
        message: String,
        /// Whether trying again can succeed.
        retryable: bool,
    },
}

/// What a part's removal did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartRemoved {
    /// Bytes freed.
    pub bytes_freed: u64,
    /// What is still in the folder that belongs to no part (never deleted).
    pub left_in_folder: Vec<String>,
}

/// The parts' files: their state, their download, their removal and their check before a load.
/// Every file a part holds lives in [`crate::cuda::folder`]. Every error's text is plain copy,
/// never a URL or a file name.
pub trait CudaFiles: Send + Sync {
    /// What `part` holds now. Reads sizes only and never hashes: the page polls this.
    fn part_files(&self, models: &ModelDir, part: CudaPart) -> PartFiles;

    /// Downloads every file of `part`, `progress(done, total)` over the whole part, until done or
    /// `cancel` is set. A later call resumes.
    ///
    /// # Errors
    /// [`CudaFilesError`].
    fn download(
        &self,
        models: &ModelDir,
        part: CudaPart,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), CudaFilesError>;

    /// Removes the files only `part` holds.
    ///
    /// # Errors
    /// [`CudaFilesError::Failed`].
    fn remove(&self, models: &ModelDir, part: CudaPart) -> Result<PartRemoved, CudaFilesError>;

    /// Checks every file of `part` against its SHA-256, because the folder is writable by the
    /// user. Nothing of a part that fails this may be loaded.
    ///
    /// # Errors
    /// [`CudaFilesError::Failed`].
    fn verify(&self, models: &ModelDir, part: CudaPart) -> Result<(), CudaFilesError>;
}

/// What the page says about a part the manifest does not pin every file of
/// ([`crate::cuda::not_available_yet`]; never so in a released build, tests pin it).
pub const NOT_AVAILABLE_YET: &str = "This download is not available yet, so there is nothing to \
     install. Earshot keeps running as it does now.";

/// Why a part may not be removed now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalRefused {
    /// Its switch is on.
    SwitchedOn,
    /// This run loaded it, and a loaded library cannot be deleted.
    LoadedThisRun,
    /// Another installed part needs its files.
    NeededByTranscription,
}

impl RemovalRefused {
    /// What the page says.
    #[must_use]
    pub fn copy(self) -> &'static str {
        match self {
            Self::SwitchedOn => "Switch it off first.",
            Self::LoadedThisRun => {
                "Earshot is using these files. Restart Earshot first, then remove them."
            }
            Self::NeededByTranscription => {
                "Transcription on CUDA needs these files. Remove that part first."
            }
        }
    }
}

/// Whether `part` may be removed: `switch` as stored now, `loaded` what this run has loaded, and
/// `transcription` the state of the part that depends on the summaries part's files. The one rule.
#[must_use]
pub fn removal_refused(
    part: CudaPart,
    switch: CudaSwitch,
    loaded: &[CudaPart],
    transcription: PartState,
) -> Option<RemovalRefused> {
    if switch.is_on(part) {
        return Some(RemovalRefused::SwitchedOn);
    }
    if loaded.contains(&part) {
        return Some(RemovalRefused::LoadedThisRun);
    }
    if part == CudaPart::Summaries && transcription != PartState::Missing {
        return Some(RemovalRefused::NeededByTranscription);
    }
    None
}
