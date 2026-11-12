//! What a capture that could not start means for the person at the computer (T9.23): the
//! failure's kind, read from the Windows error code and the step that failed, never from the
//! error's text. Platform independent; [`crate::wasapi::CaptureError::start_problem`] feeds it.

use crate::follow::AUDCLNT_E_DEVICE_INVALIDATED;

/// `E_ACCESSDENIED`: Windows refused the stream. For a microphone this is the privacy switch
/// (Settings, Privacy and security, Microphone) turned off for desktop apps.
pub const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;
/// `E_NOTFOUND` (`HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`): `GetDefaultAudioEndpoint` found no
/// endpoint of that direction.
pub const E_NOTFOUND: i32 = 0x8007_0490_u32 as i32;
/// `AUDCLNT_E_DEVICE_IN_USE`: another app holds the endpoint in exclusive mode.
pub const AUDCLNT_E_DEVICE_IN_USE: i32 = 0x8889_000A_u32 as i32;

/// The step whose failure means "no output device" rather than "no microphone": the default
/// render endpoint (`wasapi::default_device`).
const RENDER_ENDPOINT_CONTEXT: &str = "default render endpoint";

/// Why a capture could not start, as far as the person at the computer can act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartProblem {
    /// No microphone is connected (or it went away while opening).
    NoMicrophone,
    /// No speakers or headphones are connected, so the other side cannot be recorded.
    NoOutput,
    /// Windows' microphone privacy switch refuses desktop apps.
    MicrophoneAccessOff,
    /// Another app holds the device for itself.
    DeviceInUse,
    /// Anything else.
    Other,
}

/// The problem of a Windows call that failed with `code` during `context` (the call's fixed
/// description in the capture crate).
#[must_use]
pub fn start_problem(context: &str, code: i32) -> StartProblem {
    match code {
        E_ACCESSDENIED => StartProblem::MicrophoneAccessOff,
        AUDCLNT_E_DEVICE_IN_USE => StartProblem::DeviceInUse,
        E_NOTFOUND if context == RENDER_ENDPOINT_CONTEXT => StartProblem::NoOutput,
        E_NOTFOUND => StartProblem::NoMicrophone,
        AUDCLNT_E_DEVICE_INVALIDATED => StartProblem::NoMicrophone,
        _ => StartProblem::Other,
    }
}

#[cfg(windows)]
impl crate::wasapi::CaptureError {
    /// What this error means for the person at the computer ([`start_problem`]).
    #[must_use]
    pub fn start_problem(&self) -> StartProblem {
        match self {
            Self::Windows { context, source } => start_problem(context, source.code().0),
            Self::DeviceGone { .. } => StartProblem::NoMicrophone,
            _ => StartProblem::Other,
        }
    }

    /// The HRESULT of a failed Windows call, for a log line as a number only.
    #[must_use]
    pub fn hresult(&self) -> Option<i32> {
        match self {
            Self::Windows { source, .. } => Some(source.code().0),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::follow::AUDCLNT_E_SERVICE_NOT_RUNNING;

    #[test]
    fn problem_access_denied_is_the_microphone_privacy_switch() {
        for context in ["IAudioClient::Initialize", "activate IAudioClient", "x"] {
            assert_eq!(
                start_problem(context, E_ACCESSDENIED),
                StartProblem::MicrophoneAccessOff
            );
        }
    }

    #[test]
    fn problem_no_endpoint_names_the_missing_direction() {
        assert_eq!(
            start_problem("default console capture endpoint", E_NOTFOUND),
            StartProblem::NoMicrophone
        );
        assert_eq!(
            start_problem("default communications capture endpoint", E_NOTFOUND),
            StartProblem::NoMicrophone
        );
        assert_eq!(
            start_problem(RENDER_ENDPOINT_CONTEXT, E_NOTFOUND),
            StartProblem::NoOutput
        );
    }

    #[test]
    fn problem_in_use_and_lost_devices_and_the_rest() {
        assert_eq!(
            start_problem("IAudioClient::Initialize", AUDCLNT_E_DEVICE_IN_USE),
            StartProblem::DeviceInUse
        );
        assert_eq!(
            start_problem("IAudioClient::Initialize", AUDCLNT_E_DEVICE_INVALIDATED),
            StartProblem::NoMicrophone
        );
        // E_INVALIDARG and E_FAIL say nothing a person can act on.
        for code in [0x8007_0057_u32 as i32, 0x8000_4005_u32 as i32] {
            assert_eq!(start_problem("x", code), StartProblem::Other);
        }
        // The audio service stopping is not a missing microphone.
        assert_eq!(
            start_problem("x", AUDCLNT_E_SERVICE_NOT_RUNNING),
            StartProblem::Other
        );
    }

    #[cfg(windows)]
    #[test]
    fn problem_of_a_capture_error_reads_its_code_never_its_text() {
        use crate::wasapi::CaptureError;
        let denied = CaptureError::Windows {
            context: "IAudioClient::Initialize",
            source: windows::core::Error::from_hresult(windows::core::HRESULT(E_ACCESSDENIED)),
        };
        assert_eq!(denied.start_problem(), StartProblem::MicrophoneAccessOff);
        assert_eq!(denied.hresult(), Some(E_ACCESSDENIED));
        let gone = CaptureError::DeviceGone { what: "microphone" };
        assert_eq!(gone.start_problem(), StartProblem::NoMicrophone);
        assert_eq!(gone.hresult(), None);
        assert_eq!(
            CaptureError::ThreadExited.start_problem(),
            StartProblem::Other
        );
    }
}
