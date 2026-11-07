//! The real files behind `cuda_switch::CudaFiles` (T8.4af): `earshot_runtime::cuda` over the
//! embedded catalogue, downloads through `earshot_runtime::download`. Here rather than in the
//! runtime because what a window may say about a failure lives in this crate: every error is
//! fixed copy, never a URL or a file name.

use std::sync::Arc;

use earshot_core::asr::CancelToken;
use earshot_runtime::backends::ModelDir;
use earshot_runtime::catalogue::{Catalogue, InstallState};
use earshot_runtime::cuda::{self, CudaError, CudaPart};
use earshot_runtime::cuda_switch::{
    CudaFiles, CudaFilesError, NOT_AVAILABLE_YET, PartFiles, PartRemoved, PartState, RemovalRefused,
};
use earshot_runtime::download::Downloader;
use earshot_runtime::{SharedClock, host};

use crate::model_jobs::download_failure_copy;

/// What the page says when the embedded manifest cannot be read (a bug).
const NOT_THIS_VERSION: &str =
    "This version of Earshot cannot use this download. An update may fix it.";
/// A part failed its check before a load.
const FAILED_CHECK: &str =
    "These files did not pass Earshot's check. Remove them and download them again.";
/// A file of the part is loaded or open.
const IN_USE: &str = "These files are in use or not writable, so nothing was removed. Restart Earshot, then remove \
     them.";
/// A removal stopped part way.
const REMOVE_FAILED: &str = "Earshot could not remove all of these files. Try again.";

fn failed(message: &str, retryable: bool) -> CudaFilesError {
    CudaFilesError::Failed {
        message: message.to_owned(),
        retryable,
    }
}

/// The part's files in the models folder, as the embedded manifest pins them.
pub struct ManifestCudaFiles {
    clock: SharedClock,
}

impl ManifestCudaFiles {
    /// Files whose downloads stamp their audit entries with `clock`.
    #[must_use]
    pub fn new(clock: SharedClock) -> Arc<Self> {
        Arc::new(Self { clock })
    }
}

fn part_state(state: InstallState) -> PartState {
    match state {
        InstallState::Installed => PartState::Installed,
        InstallState::Partial => PartState::Partial,
        InstallState::Missing => PartState::Missing,
    }
}

/// What the page is told about `part` when there is nothing to read.
fn no_part() -> PartFiles {
    PartFiles {
        not_available: Some(NOT_THIS_VERSION.to_owned()),
        download_size: 0,
        disk_size: 0,
        held: 0,
        state: PartState::Missing,
    }
}

impl CudaFiles for ManifestCudaFiles {
    fn part_files(&self, models: &ModelDir, part: CudaPart) -> PartFiles {
        let Ok(c) = Catalogue::embedded() else {
            return no_part();
        };
        let Ok(install) = cuda::install_state(c, models, part) else {
            return no_part();
        };
        PartFiles {
            not_available: (!install.not_available_yet.is_empty())
                .then(|| NOT_AVAILABLE_YET.to_owned()),
            download_size: install.total,
            disk_size: cuda::disk_size(c, part),
            held: install.held,
            state: part_state(install.state),
        }
    }

    fn download(
        &self,
        models: &ModelDir,
        part: CudaPart,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<(), CudaFilesError> {
        // Its own current-thread runtime; this is a plain thread, never inside another one.
        let downloader = Downloader::for_manifest(Arc::clone(&self.clock))
            .map_err(|_| failed(NOT_THIS_VERSION, false))?;
        host::download_cuda_part(&downloader, models, part, cancel, progress).map_err(|e| {
            if e.cancelled() {
                CudaFilesError::Cancelled
            } else {
                CudaFilesError::Failed {
                    message: download_failure_copy(&e),
                    retryable: e.retryable(),
                }
            }
        })
    }

    fn remove(&self, models: &ModelDir, part: CudaPart) -> Result<PartRemoved, CudaFilesError> {
        let removed = host::remove_cuda_part(models, part).map_err(|e| remove_failure(&e))?;
        Ok(PartRemoved {
            bytes_freed: removed.bytes_freed,
            left_in_folder: removed.left_in_folder,
        })
    }

    fn verify(&self, models: &ModelDir, part: CudaPart) -> Result<(), CudaFilesError> {
        cuda::verify_part(models, part).map_err(|_| failed(FAILED_CHECK, false))
    }
}

/// What the user is told about a removal that failed, and whether a retry can work: a file in use
/// goes free when the app restarts.
fn remove_failure(e: &CudaError) -> CudaFilesError {
    match e {
        CudaError::InUse { .. } => failed(IN_USE, true),
        CudaError::NeededBy { .. } => failed(RemovalRefused::NeededByTranscription.copy(), false),
        CudaError::Remove { .. } | CudaError::Unreadable { .. } => failed(REMOVE_FAILED, true),
        CudaError::Catalogue(_)
        | CudaError::NotAvailableYet { .. }
        | CudaError::MissingFile { .. }
        | CudaError::FailedCheck { .. }
        | CudaError::UnexpectedLibrary { .. } => failed(NOT_THIS_VERSION, false),
    }
}

#[cfg(test)]
#[path = "cuda_files_tests.rs"]
mod tests;
