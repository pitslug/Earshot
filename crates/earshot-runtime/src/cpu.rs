//! What the processor must offer before whisper.cpp or llama.cpp runs here (T9.23). Every x64
//! build compiles them for the AVX2 and FMA baseline (`GGML_NATIVE=OFF`, AGENTS.md), so on an
//! older x64 processor their first instruction would end the process with an illegal
//! instruction. The check runs once per process and refuses the load with
//! [`RuntimeError::ProcessorTooOld`] instead, which the app shows as fixed copy. ONNX Runtime
//! picks its kernels at run time, so Parakeet and the speech helpers still run.
//!
//! The only callers are [`crate::backends::select_whisper`] (every whisper load) and the
//! production llama.cpp loader in [`crate::summary`].

use std::sync::OnceLock;

use crate::RuntimeError;

/// Whether this processor runs the native builds: on x86-64 it has AVX2 and FMA; any other
/// architecture has its own baseline, checked at build time. Read once per process.
#[must_use]
pub fn runs_native_builds() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(detect)
}

#[cfg(target_arch = "x86_64")]
fn detect() -> bool {
    std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
}

#[cfg(not(target_arch = "x86_64"))]
fn detect() -> bool {
    true
}

/// `Ok` when `supported`, else [`RuntimeError::ProcessorTooOld`].
///
/// # Errors
/// [`RuntimeError::ProcessorTooOld`] when the processor lacks the baseline.
pub fn require(supported: bool) -> Result<(), RuntimeError> {
    if supported {
        Ok(())
    } else {
        Err(RuntimeError::ProcessorTooOld)
    }
}

/// [`require`] on this processor.
///
/// # Errors
/// [`RuntimeError::ProcessorTooOld`] when this processor lacks the baseline.
pub fn require_native_builds() -> Result<(), RuntimeError> {
    require(runs_native_builds())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_a_processor_without_the_baseline_is_refused_by_kind() {
        assert!(require(true).is_ok());
        assert!(matches!(require(false), Err(RuntimeError::ProcessorTooOld)));
    }

    #[test]
    fn cpu_the_build_host_runs_the_native_builds() {
        // Every host that builds Earshot runs whisper.cpp's tests, so it has the baseline;
        // a host that does not would crash in those tests instead.
        assert!(runs_native_builds());
        assert!(require_native_builds().is_ok());
    }
}
