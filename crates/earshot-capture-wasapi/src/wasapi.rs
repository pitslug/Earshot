//! Windows-only WASAPI layer: COM apartment, default endpoints, process loopback
//! activation, audio session enumeration, default-device notifications,
//! event-driven shared-mode capture streams, and one capture loop that follows device
//! changes, used by a fixed-duration mic + loopback recorder ([`record_raw`]) and by
//! the live core `AudioSource` ([`WasapiSource`]).
//!
//! Every `unsafe` block lives in this file, is as small as possible and carries a
//! `SAFETY` note. Callers only see safe types.
//!
//! References: Microsoft Learn, "Capturing a Stream", "Loopback Recording",
//! `ActivateAudioInterfaceAsync`, `AUDIOCLIENT_ACTIVATION_PARAMS`, the
//! "ApplicationLoopback" Windows classic sample (process loopback), "Audio Sessions"
//! (`IAudioSessionManager2`) and `IMMNotificationClient` (Core Audio APIs).

use std::fmt;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use earshot_core::audio::SampleRate;
use earshot_core::capture::{
    AudioSource, CaptureSession, CaptureSink, DeviceTime, EndOfStream, EndReason, SourceStatus,
    TrackId, TrackInfo,
};

use windows::Win32::Devices::FunctionDiscovery::{
    PKEY_Device_EnumeratorName, PKEY_Device_FriendlyName,
};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, PROPERTYKEY, RPC_E_CHANGED_MODE, S_OK, WAIT_FAILED,
};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDCLNT_STREAMOPTIONS_NONE, AUDCLNT_STREAMOPTIONS_RAW, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync, AudioCategory_Communications,
    AudioCategory_Other, AudioClientProperties, AudioSessionStateActive, DEVICE_STATE,
    DEVICE_STATE_ACTIVE, EDataFlow, ERole, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IAudioClient2, IAudioRenderClient, IAudioSessionControl,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, IMMEndpoint,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
    PKEY_AudioEndpoint_FormFactor, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX, eCapture, eCommunications, eConsole,
    eMultimedia, eRender,
};
use windows::Win32::Storage::EnhancedStorage::PKEY_Devices_AudioDevice_RawProcessingSupported;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PropVariantToBoolean, PropVariantToStringAlloc, PropVariantToUInt32,
};
use windows::Win32::System::Com::{
    BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, IAgileObject, IAgileObject_Impl, STGM_READ,
};
use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Power::{
    GetSystemPowerStatus, PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired,
    PowerSetRequest, SYSTEM_POWER_STATUS,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, GetThreadPriority, OpenProcess,
    POWER_REQUEST_CONTEXT_SIMPLE_STRING, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, REASON_CONTEXT, REASON_CONTEXT_0, SetThreadPriority,
    THREAD_PRIORITY_BELOW_NORMAL, WaitForMultipleObjects,
};
use windows::Win32::System::Variant::{VT_BLOB, VT_BOOL};
use windows::core::{BOOL, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement, w};

use crate::call_output::{CallOutput, CallOutputFollower};
use crate::devices::{AudioDevice, DeviceChoice, DeviceFallback, DeviceList, DeviceSelection};
use crate::endpoint::{EndpointProperties, HandsFreeMic, MicProcessing, RawRefusal, raw_refusal};
use crate::follow::{
    self, DefaultDeviceChange, Flow, OpenFailure, ReopenReason, Role, StreamFollower,
};
use crate::format::{
    ChannelMode, FormatError, MixFormat, check_delivered, conversion_target, float_waveformat_bytes,
};
use crate::live::{
    ChannelSink, ChannelSource, EmitterTrack, LiveError, LiveMsg, QueueGauge, TrackEmitter,
    queue_limit,
};
use crate::observer::{ReopenOutcome, TrackObserver};
use crate::problem::{
    LOOPBACK_ACTIVATE_CONTEXT, LOOPBACK_INITIALIZE_CONTEXT, PROCESS_LOOPBACK_CONTEXT,
    RENDER_ACTIVATE_CONTEXT, RENDER_ENDPOINT_CONTEXT, RENDER_INITIALIZE_CONTEXT,
};
use crate::recording::{
    DeviceDescription, LoopbackSource, MicRole, NotifyingTrack, RawRecording, RecordingTrack,
};
use crate::session::{
    AudioSession, CaptureMode, FallbackReason, LoopbackChoice, LoopbackRequest, SelectError,
    SessionPeak, choose_loopback,
};
use crate::timeline::{PacketInfo, QPC_UNITS_PER_SEC, TrackTimeline};

// format.rs parses the 18-byte packed WAVEFORMATEX header; the bindings must agree.
const _: () = assert!(size_of::<WAVEFORMATEX>() == 18);

/// Shared-mode buffer duration requested from the audio engine (100 ns units): 200 ms.
const BUFFER_DURATION_HNS: i64 = 2_000_000;
/// Upper bound on one wait. Also covers loopback streams whose event is not signalled
/// while nothing renders, and the polling of device notifications.
const WAIT_MS: u32 = 20;
/// Process loopback has no mix format: this rate (and stereo float) is requested and
/// the audio engine converts to it.
pub const PROCESS_LOOPBACK_RATE: u32 = 48_000;
/// Channel count requested for process loopback: stereo, kept through to the
/// reference track (T6.3c).
pub const PROCESS_LOOPBACK_CHANNELS: u16 = 2;
/// How long to wait for `ActivateAudioInterfaceAsync` to complete.
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);
/// Timestamps beyond the recording deadline plus this margin are clamped by the
/// timeline (bogus QPC values must not synthesize unbounded silence).
const QPC_LIMIT_MARGIN: u64 = QPC_UNITS_PER_SEC;

/// Errors from the WASAPI layer.
#[derive(Debug)]
pub enum CaptureError {
    /// A Windows API call failed.
    Windows {
        /// What was being attempted.
        context: &'static str,
        /// The underlying HRESULT error.
        source: windows::core::Error,
    },
    /// The device mix format cannot be converted, or a packet was malformed.
    Format(FormatError),
    /// COM is already initialised on this thread as a single-threaded apartment.
    ApartmentMismatch,
    /// An explicitly requested loopback process could not be found.
    Selection(SelectError),
    /// `ActivateAudioInterfaceAsync` did not complete in time.
    ActivationTimeout,
    /// The capture thread could not be spawned.
    Thread(std::io::Error),
    /// The capture thread exited (panicked) before capture started.
    ThreadExited,
    /// A chosen endpoint (T7.4i) is not an active endpoint of the needed direction (it was
    /// unplugged, switched off or disabled).
    DeviceGone {
        /// What is missing.
        what: &'static str,
    },
    /// A raw microphone stream (T12.32) was asked for and the endpoint cannot deliver one.
    /// Never replaced by the processed stream.
    RawMic(RawRefusal),
}

impl CaptureError {
    /// True when the error means the stream's device went away (re-open it).
    pub fn is_device_loss(&self) -> bool {
        matches!(self, Self::Windows { source, .. } if follow::is_device_loss(source.code().0))
    }

    /// What a log keeps of this error: for a Windows API error the call and its error as a
    /// number only ([`follow::windows_error_detail`]), never Windows' message text; any other
    /// error's own text.
    fn detail(&self) -> String {
        match self.windows_failure() {
            Some((context, hresult)) => follow::windows_error_detail(context, hresult),
            None => self.to_string(),
        }
    }

    /// The failed call and its HRESULT, for a Windows API error.
    fn windows_failure(&self) -> Option<(&'static str, i32)> {
        match self {
            Self::Windows { context, source } => Some((context, source.code().0)),
            _ => None,
        }
    }

    /// True when a chosen endpoint is not there (not listed, not active, or the wrong
    /// direction): [`CaptureError::DeviceGone`], or a device loss while opening it.
    pub fn is_device_gone(&self) -> bool {
        matches!(self, Self::DeviceGone { .. }) || self.is_device_loss()
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Windows { context, source } => write!(f, "{context}: {source}"),
            Self::Format(e) => write!(f, "{e}"),
            Self::ApartmentMismatch => {
                write!(
                    f,
                    "COM already initialised as STA on this thread; capture needs MTA"
                )
            }
            Self::Selection(e) => write!(f, "{e}"),
            Self::ActivationTimeout => write!(
                f,
                "process loopback activation did not complete within {} s",
                ACTIVATION_TIMEOUT.as_secs()
            ),
            Self::Thread(e) => write!(f, "cannot spawn the capture thread: {e}"),
            Self::ThreadExited => f.write_str("capture thread exited before capture started"),
            Self::DeviceGone { what } => write!(f, "the chosen {what} is not connected"),
            Self::RawMic(refusal) => write!(f, "{refusal}"),
        }
    }
}

impl std::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Windows { source, .. } => Some(source),
            Self::Format(e) => Some(e),
            Self::Thread(e) => Some(e),
            // Display already prints the selection error in full.
            Self::ApartmentMismatch
            | Self::ActivationTimeout
            | Self::Selection(_)
            | Self::ThreadExited
            | Self::DeviceGone { .. }
            | Self::RawMic(_) => None,
        }
    }
}

impl From<FormatError> for CaptureError {
    fn from(e: FormatError) -> Self {
        Self::Format(e)
    }
}

trait Context<T> {
    fn ctx(self, context: &'static str) -> Result<T, CaptureError>;
}

impl<T> Context<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T, CaptureError> {
        self.map_err(|source| CaptureError::Windows { context, source })
    }
}

/// COM multithreaded apartment for the current thread. Uninitialised on drop.
///
/// Streams borrow the apartment, so the borrow checker guarantees every COM object
/// is released before `CoUninitialize` runs.
pub struct ComApartment {
    // COM initialisation is per thread: keep this !Send and !Sync.
    _thread_bound: PhantomData<*const ()>,
}

impl ComApartment {
    /// Initialises COM as MTA on the calling thread.
    pub fn init_mta() -> Result<Self, CaptureError> {
        // SAFETY: no reserved pointer; balanced by CoUninitialize in Drop on success
        // (S_OK and S_FALSE both require it).
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            return Err(CaptureError::ApartmentMismatch);
        }
        hr.ok().ctx("CoInitializeEx")?;
        Ok(Self {
            _thread_bound: PhantomData,
        })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: paired with the successful CoInitializeEx in init_mta, same thread
        // (the type is !Send).
        unsafe { CoUninitialize() };
    }
}

/// What a capture stream records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// Default capture device for the given role.
    Microphone(MicRole),
    /// Loopback of the default console render device.
    Loopback,
    /// Process loopback of `pid` and its child processes (Windows 10 2004 or later).
    /// Device independent: it keeps working across render device changes.
    ProcessLoopback {
        /// Target process id.
        pid: u32,
    },
}

impl Endpoint {
    /// How packets are converted: the mic is downmixed to mono, a loopback (the echo
    /// canceller's reference) keeps its channels (T6.3c).
    pub fn channel_mode(self) -> ChannelMode {
        match self {
            Self::Microphone(_) => ChannelMode::Mono,
            Self::Loopback | Self::ProcessLoopback { .. } => ChannelMode::Interleaved,
        }
    }
}

/// Owned Win32 auto-reset event, closed on drop.
struct Event(HANDLE);

impl Event {
    fn new() -> Result<Self, CaptureError> {
        // SAFETY: unnamed auto-reset event with default security; the handle is
        // owned by the returned value and closed exactly once in Drop.
        let h = unsafe { CreateEventW(None, false, false, None) }.ctx("CreateEventW")?;
        Ok(Self(h))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Current QPC time converted to 100 ns units (the unit of `GetBuffer`'s QPC position).
pub fn qpc_now() -> Result<u64, CaptureError> {
    let mut count = 0i64;
    let mut freq = 0i64;
    // SAFETY: both pointers are valid, writable locals.
    unsafe {
        QueryPerformanceCounter(&mut count).ctx("QueryPerformanceCounter")?;
        QueryPerformanceFrequency(&mut freq).ctx("QueryPerformanceFrequency")?;
    }
    let units = i128::from(count) * i128::from(QPC_UNITS_PER_SEC) / i128::from(freq.max(1));
    Ok(units as u64)
}

fn device_enumerator(_com: &ComApartment) -> Result<IMMDeviceEnumerator, CaptureError> {
    // SAFETY: COM is initialised on this thread (we hold the apartment); the result
    // is an owned, reference-counted interface.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .ctx("create MMDeviceEnumerator")
}

fn default_device(
    enumerator: &IMMDeviceEnumerator,
    endpoint: Endpoint,
) -> Result<IMMDevice, CaptureError> {
    // SAFETY: plain COM calls on a live enumerator; results are owned interfaces.
    unsafe {
        match endpoint {
            Endpoint::Microphone(MicRole::Console) => enumerator
                .GetDefaultAudioEndpoint(eCapture, eConsole)
                .ctx("default console capture endpoint"),
            Endpoint::Microphone(MicRole::Communications) => enumerator
                .GetDefaultAudioEndpoint(eCapture, eCommunications)
                .ctx("default communications capture endpoint"),
            Endpoint::Loopback | Endpoint::ProcessLoopback { .. } => enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .ctx(RENDER_ENDPOINT_CONTEXT),
        }
    }
}

/// The active endpoint `id` of the direction `endpoint` records (capture for the mic,
/// render for a loopback); [`CaptureError::DeviceGone`] when Windows does not list it as
/// such (T7.4i).
fn chosen_device(
    enumerator: &IMMDeviceEnumerator,
    endpoint: Endpoint,
    id: &str,
) -> Result<IMMDevice, CaptureError> {
    let (flow, what) = match endpoint {
        Endpoint::Microphone(_) => (eCapture, crate::problem::GONE_MICROPHONE),
        Endpoint::Loopback | Endpoint::ProcessLoopback { .. } => {
            (eRender, crate::problem::GONE_OUTPUT)
        }
    };
    let gone = CaptureError::DeviceGone { what };
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string alive for the call; the result is an
    // owned interface.
    let Ok(device) = (unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }) else {
        return Err(gone);
    };
    // SAFETY: plain COM call on a live device.
    let active = unsafe { device.GetState() }.is_ok_and(|s| s == DEVICE_STATE_ACTIVE);
    let Ok(endpoint) = device.cast::<IMMEndpoint>() else {
        return Err(gone);
    };
    // SAFETY: plain COM call on a live endpoint.
    let right_flow = unsafe { endpoint.GetDataFlow() }.is_ok_and(|f| f == flow);
    if active && right_flow {
        Ok(device)
    } else {
        Err(gone)
    }
}

/// Copies a CoTaskMemAlloc'd wide string into a `String` and frees it.
///
/// # Safety
/// `p` must be a valid, NUL-terminated string allocated with CoTaskMemAlloc that the
/// caller owns; it must not be used afterwards.
unsafe fn take_co_string(p: PWSTR) -> String {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn friendly_name(device: &IMMDevice) -> Result<String, CaptureError> {
    // SAFETY: the property store and PROPVARIANT are owned locals. The PROPVARIANT
    // owns its contents and is cleared by its Drop (windows-rs runs PropVariantClear);
    // the PWSTR from PropVariantToStringAlloc is freed by take_co_string after its
    // contents were copied into a String.
    unsafe {
        let store = device
            .OpenPropertyStore(STGM_READ)
            .ctx("OpenPropertyStore")?;
        let value = store
            .GetValue(&PKEY_Device_FriendlyName)
            .ctx("read device friendly name")?;
        let pwstr = PropVariantToStringAlloc(&value).ctx("friendly name to string")?;
        Ok(take_co_string(pwstr))
    }
}

/// The device interface path of the endpoint's kernel-streaming filter: property
/// `{233164c8-1b2c-4c7d-bc68-b671687a2567},1` of the endpoint store (observed on every
/// endpoint of the desktop, 2026-10-24; not in the Windows SDK headers).
const ENDPOINT_FILTER_PATH: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0x233164c8_1b2c_4c7d_bc68_b671687a2567),
    pid: 1,
};

/// Reads what [`crate::endpoint::classify_mic`] needs from `device`'s property store.
/// Every property is optional: one that is missing or unreadable is `None`, never an
/// error (the classification only ever warns).
fn endpoint_properties(
    device: &IMMDevice,
    name: &str,
    sample_rate: Option<u32>,
) -> crate::endpoint::EndpointProperties {
    // SAFETY: a plain COM call on a live device; the store is an owned interface.
    let (enumerator, form_factor, filter_path, raw_processing) =
        match unsafe { device.OpenPropertyStore(STGM_READ) } {
            Ok(store) => {
                // SAFETY: the PROPVARIANT is an owned local that owns its contents and is
                // cleared by its Drop (windows-rs runs PropVariantClear).
                let value = |key: &PROPERTYKEY| unsafe { store.GetValue(key) }.ok();
                let string = |key: &PROPERTYKEY| {
                    let v = value(key)?;
                    // SAFETY: `v` is a valid PROPVARIANT; the result is a CoTaskMemAlloc'd
                    // string we own.
                    let p = unsafe { PropVariantToStringAlloc(&v) }.ok()?;
                    // SAFETY: `p` came from PropVariantToStringAlloc above and is not used
                    // after this call, which frees it.
                    let s = unsafe { take_co_string(p) };
                    Some(s).filter(|s| !s.is_empty())
                };
                // SAFETY: only reads the valid PROPVARIANT `v`.
                let form_factor = value(&PKEY_AudioEndpoint_FormFactor)
                    .and_then(|v| unsafe { PropVariantToUInt32(&v) }.ok());
                // Whether the endpoint supports the raw signal processing mode (T12.32;
                // Microsoft Learn, "Audio Signal Processing Modes"). Only a VT_BOOL counts:
                // a missing property reads as VT_EMPTY, which is unknown, never "no".
                let raw_processing = value(&PKEY_Devices_AudioDevice_RawProcessingSupported)
                    .filter(|v| v.vt() == VT_BOOL)
                    // SAFETY: only reads the valid PROPVARIANT `v`.
                    .and_then(|v| unsafe { PropVariantToBoolean(&v) }.ok())
                    .map(|b| b.as_bool());
                (
                    string(&PKEY_Device_EnumeratorName),
                    form_factor,
                    string(&ENDPOINT_FILTER_PATH),
                    raw_processing,
                )
            }
            Err(_) => (None, None, None, None),
        };
    crate::endpoint::EndpointProperties {
        name: name.to_owned(),
        enumerator,
        form_factor,
        filter_path,
        sample_rate,
        id: device_id(device).filter(|id| !id.is_empty()),
        raw_processing,
    }
}

fn device_id(device: &IMMDevice) -> Option<String> {
    // SAFETY: GetId returns a CoTaskMemAlloc'd string owned by us, freed by
    // take_co_string.
    unsafe { device.GetId().ok().map(|p| take_co_string(p)) }
}

/// Executable file name (for example `Zoom.exe`) of a running process.
pub fn process_image_name(pid: u32) -> Result<String, CaptureError> {
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: the handle is closed before returning; `buf` is a writable buffer of
    // `len` UTF-16 units, and `len` receives the length written (without NUL).
    let path = unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ctx("OpenProcess")?;
        let r =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(h);
        r.ctx("QueryFullProcessImageNameW")?;
        String::from_utf16_lossy(&buf[..len as usize])
    };
    Ok(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_owned())
}

/// Every audio session on every active render endpoint (the system sounds session
/// is skipped). A process with sessions on several endpoints appears once per session.
pub fn list_render_sessions(com: &ComApartment) -> Result<Vec<AudioSession>, CaptureError> {
    let mut out = Vec::new();
    each_render_session(com, |_, control, pid| {
        // SAFETY: plain COM call on a live, owned interface.
        let active = unsafe { control.GetState() }.is_ok_and(|s| s == AudioSessionStateActive);
        out.push(AudioSession {
            pid,
            process_name: process_image_name(pid).unwrap_or_default(),
            active,
        });
    })?;
    Ok(out)
}

/// Every render session on every active endpoint with its current peak (the system sounds
/// session is skipped).
pub fn render_session_peaks(com: &ComApartment) -> Result<Vec<SessionPeak>, CaptureError> {
    let mut out = Vec::new();
    each_render_session(com, |device, control, pid| {
        // SAFETY: plain COM calls on live, owned interfaces; `GetPeakValue` returns a value.
        let (active, peak) = unsafe {
            (
                control
                    .GetState()
                    .is_ok_and(|s| s == AudioSessionStateActive),
                control
                    .cast::<IAudioMeterInformation>()
                    .ok()
                    .and_then(|m| m.GetPeakValue().ok()),
            )
        };
        out.push(SessionPeak {
            endpoint: friendly_name(device).unwrap_or_default(),
            endpoint_id: device_id(device).unwrap_or_default(),
            pid,
            process_name: process_image_name(pid).unwrap_or_default(),
            active,
            peak,
        });
    })?;
    Ok(out)
}

/// Reads [`render_session_peaks`] every `interval` on a thread of its own (with its own COM
/// apartment, so no capture thread ever waits for it), for as long as it is held. The thread holds
/// no stream and reads no audio: only process names, session states and meters.
///
/// Dropping it never waits: a meeting's stop passes through here, and a reading may be inside a COM
/// call that a hung audio service never returns from. The thread is left to end by itself, at its
/// next look at the stop flag or its next send; it owns its apartment, so nothing it holds depends
/// on whoever dropped this.
#[derive(Debug)]
pub struct RenderSessionPoller {
    stop: Arc<AtomicBool>,
    readings: mpsc::Receiver<SessionReading>,
}

/// One reading of the render sessions.
type SessionReading = Result<Vec<SessionPeak>, String>;

impl RenderSessionPoller {
    /// Starts polling at once.
    ///
    /// # Errors
    /// The thread could not be started.
    pub fn start(interval: Duration) -> Result<Self, CaptureError> {
        Self::start_with(interval, || {
            let com = ComApartment::init_mta();
            move || match &com {
                Ok(com) => render_session_peaks(com).map_err(|e| e.detail()),
                Err(e) => Err(e.detail()),
            }
        })
    }

    /// [`start`](Self::start) with the reader `make` builds on the polling thread.
    fn start_with<R: FnMut() -> SessionReading>(
        interval: Duration,
        make: impl FnOnce() -> R + Send + 'static,
    ) -> Result<Self, CaptureError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, readings) = mpsc::channel();
        let flag = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("earshot-call-sessions".into())
            .spawn(move || poll_render_sessions(interval, &flag, &tx, make()))
            .map_err(CaptureError::Thread)?;
        Ok(Self { stop, readings })
    }

    /// The newest reading since the last call (`None`: nothing new); `Err` when it failed.
    pub fn latest(&mut self) -> Option<SessionReading> {
        self.readings.try_iter().last()
    }
}

impl Drop for RenderSessionPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// How often a [`RenderSessionPoller`] looks at its stop flag while it waits.
const POLLER_STOP_CHECK: Duration = Duration::from_millis(50);

fn poll_render_sessions(
    interval: Duration,
    stop: &AtomicBool,
    tx: &mpsc::Sender<SessionReading>,
    mut read: impl FnMut() -> SessionReading,
) {
    while !stop.load(Ordering::Acquire) {
        if tx.send(read()).is_err() {
            return;
        }
        let mut waited = Duration::ZERO;
        while waited < interval && !stop.load(Ordering::Acquire) {
            std::thread::sleep(POLLER_STOP_CHECK);
            waited += POLLER_STOP_CHECK;
        }
    }
}

/// Calls `f` with the endpoint, session and owning pid of every render session on every
/// active endpoint, the system sounds session skipped.
fn each_render_session(
    com: &ComApartment,
    mut f: impl FnMut(&IMMDevice, &IAudioSessionControl, u32),
) -> Result<(), CaptureError> {
    let enumerator = device_enumerator(com)?;
    // SAFETY: plain COM calls on live, owned interfaces.
    let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
        .ctx("EnumAudioEndpoints")?;
    // SAFETY: as above.
    let count = unsafe { devices.GetCount() }.ctx("IMMDeviceCollection::GetCount")?;
    for i in 0..count {
        // SAFETY: as above; `i` is below the collection count.
        let Ok(device) = (unsafe { devices.Item(i) }) else {
            continue;
        };
        // A device that cannot list sessions is skipped rather than failing the list.
        // SAFETY: as above.
        let Ok(manager) = (unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) })
        else {
            continue;
        };
        // SAFETY: as above.
        let Ok(sessions) = (unsafe { manager.GetSessionEnumerator() }) else {
            continue;
        };
        // SAFETY: as above.
        let n = unsafe { sessions.GetCount() }.unwrap_or(0);
        for j in 0..n {
            // SAFETY: as above; `j` is below the session count.
            let Ok(control) = (unsafe { sessions.GetSession(j) }) else {
                continue;
            };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            // SAFETY: as above. IsSystemSoundsSession returns S_OK for the system
            // sounds session and S_FALSE otherwise.
            if unsafe { control2.IsSystemSoundsSession() } == S_OK {
                continue;
            }
            // SAFETY: as above.
            let Ok(pid) = (unsafe { control2.GetProcessId() }) else {
                continue;
            };
            f(&device, &control, pid);
        }
    }
    Ok(())
}

/// Every active capture and render endpoint (T7.4i), in Windows' order, with its name,
/// whether it is the console default, and its properties (the mix format's rate read from an
/// `IAudioClient` that is activated but never initialised, so nothing is opened). An endpoint
/// whose id cannot be read is left out; one whose rate cannot be read has none.
pub fn list_devices(com: &ComApartment) -> Result<DeviceList, CaptureError> {
    let enumerator = device_enumerator(com)?;
    Ok(DeviceList {
        microphones: endpoints_of(&enumerator, eCapture)?,
        outputs: endpoints_of(&enumerator, eRender)?,
    })
}

fn endpoints_of(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
) -> Result<Vec<AudioDevice>, CaptureError> {
    // SAFETY: plain COM call on a live enumerator; the result is an owned interface. No
    // default (no endpoint of this direction) is not an error.
    let default_id = unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }
        .ok()
        .and_then(|d| device_id(&d));
    // SAFETY: as above.
    let devices = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }
        .ctx("EnumAudioEndpoints")?;
    // SAFETY: as above.
    let count = unsafe { devices.GetCount() }.ctx("IMMDeviceCollection::GetCount")?;
    let mut out = Vec::new();
    for i in 0..count {
        // SAFETY: as above; `i` is below the collection count.
        let Ok(device) = (unsafe { devices.Item(i) }) else {
            continue;
        };
        let Some(id) = device_id(&device).filter(|id| !id.is_empty()) else {
            continue;
        };
        let name = friendly_name(&device).unwrap_or_else(|_| "Unnamed audio device".to_owned());
        // SAFETY: COM is initialised; the client is an owned interface that is released
        // without being initialised.
        let rate = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }
            .ok()
            .and_then(|client| mix_format(&client).ok())
            .map(|(mix, _)| mix.sample_rate);
        let properties = endpoint_properties(&device, &name, rate);
        out.push(AudioDevice {
            is_default: default_id.as_deref() == Some(id.as_str()),
            id,
            name,
            properties,
        });
    }
    Ok(out)
}

/// [`list_devices`] on a short-lived thread of its own, so any thread may call it (COM is
/// per thread, and the caller's may already be a single-threaded apartment).
pub fn list_audio_devices() -> Result<DeviceList, CaptureError> {
    std::thread::Builder::new()
        .name("earshot-devices".into())
        .spawn(|| {
            let com = ComApartment::init_mta()?;
            list_devices(&com)
        })
        .map_err(CaptureError::Thread)?
        .join()
        .unwrap_or(Err(CaptureError::ThreadExited))
}

/// Heap-owned arguments of one `ActivateAudioInterfaceAsync` call: the activation
/// parameters and the VT_BLOB PROPVARIANT pointing at them. Owned by the completion
/// handler, so COM's reference count keeps both alive until the audio service has
/// released the handler (after `ActivateCompleted`), whatever the caller does.
struct ActivationArgs {
    params: *mut AUDIOCLIENT_ACTIVATION_PARAMS,
    prop: *mut PROPVARIANT,
}

impl ActivationArgs {
    fn new(params: AUDIOCLIENT_ACTIVATION_PARAMS) -> Self {
        let params = Box::into_raw(Box::new(params));
        // The Win32 PROPVARIANT implements Drop as PropVariantClear (windows 0.62
        // `extensions/Win32/System/StructuredStorage.rs`), which would CoTaskMemFree
        // the blob pointer. The PROPVARIANT is therefore never dropped as such: Drop
        // below frees its Box storage through ManuallyDrop, without clearing it.
        let mut prop = ManuallyDrop::new(PROPVARIANT::default());
        // SAFETY: `prop` is a fresh, zeroed PROPVARIANT we exclusively own; this
        // writes its VT_BLOB arm. The blob points at the boxed `params`, which is
        // freed only in Drop, after the PROPVARIANT can no longer be read.
        unsafe {
            let inner = &mut prop.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob = BLOB {
                cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                pBlobData: params.cast::<u8>(),
            };
        }
        Self {
            params,
            prop: Box::into_raw(Box::new(prop)).cast::<PROPVARIANT>(),
        }
    }
}

impl Drop for ActivationArgs {
    fn drop(&mut self) {
        // SAFETY: both pointers came from Box::into_raw in `new` and are freed only
        // here. `prop` is reconstituted as the ManuallyDrop it was boxed as (same
        // layout, repr(transparent)), so PropVariantClear never runs on it.
        unsafe {
            drop(Box::from_raw(self.prop.cast::<ManuallyDrop<PROPVARIANT>>()));
            drop(Box::from_raw(self.params));
        }
    }
}

/// Completion handler for `ActivateAudioInterfaceAsync`: owns the call's arguments
/// and signals the waiting thread. Agile (`IAgileObject`) as the API requires.
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct ActivationHandler {
    done: Mutex<mpsc::Sender<()>>,
    // Held only to keep the call's arguments alive as long as the handler.
    _args: ActivationArgs,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        if let Ok(tx) = self.done.lock() {
            // The waiter may have timed out and dropped the receiver.
            let _ = tx.send(());
        }
        Ok(())
    }
}

impl IAgileObject_Impl for ActivationHandler_Impl {}

/// Activates an `IAudioClient` for process loopback of `pid` and its child processes.
/// Fails on Windows versions before 10 2004 (build 19041).
///
/// Everything handed to `ActivateAudioInterfaceAsync` stays alive until
/// `ActivateCompleted` fires: the arguments are owned by the handler, and on a
/// timeout our references to the handler and the operation are leaked, never freed.
fn activate_process_loopback(pid: u32) -> Result<IAudioClient, CaptureError> {
    let args = ActivationArgs::new(AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    });
    let prop: *const PROPVARIANT = args.prop;
    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler {
        done: Mutex::new(tx),
        _args: args,
    }
    .into();
    // SAFETY: the device path is a static wide string and the IID matches the
    // requested interface. `prop` points into the handler's heap-owned arguments,
    // which live as long as any reference to the handler (ours, or the service's
    // until after ActivateCompleted). If the call fails synchronously nothing is in
    // flight and dropping `handler` frees them.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(prop),
            &handler,
        )
    }
    .ctx("ActivateAudioInterfaceAsync (process loopback)")?;
    if rx.recv_timeout(ACTIVATION_TIMEOUT).is_err() {
        // Still in flight: never free anything the service may yet touch.
        std::mem::forget(handler);
        std::mem::forget(operation);
        return Err(CaptureError::ActivationTimeout);
    }
    let mut result = HRESULT(0);
    let mut activated: Option<IUnknown> = None;
    // SAFETY: both out-pointers are valid locals; activation has completed.
    unsafe { operation.GetActivateResult(&mut result, &mut activated) }.ctx("GetActivateResult")?;
    result.ok().ctx(PROCESS_LOOPBACK_CONTEXT)?;
    let unknown = activated.ok_or(CaptureError::Windows {
        context: PROCESS_LOOPBACK_CONTEXT,
        source: windows::core::Error::empty(),
    })?;
    unknown
        .cast::<IAudioClient>()
        .ctx("process loopback IAudioClient")
}

/// Copies the device mix format into a portable [`MixFormat`] and frees the original.
///
/// Returns the raw bytes too: `Initialize` needs the exact structure back.
fn mix_format(client: &IAudioClient) -> Result<(MixFormat, Vec<u8>), CaptureError> {
    // SAFETY: GetMixFormat returns a CoTaskMemAlloc'd WAVEFORMATEX whose total size is
    // 18 + cbSize bytes (size asserted at compile time above). It is read once into an
    // owned Vec and freed before returning.
    unsafe {
        let ptr = client.GetMixFormat().ctx("GetMixFormat")?;
        let len = size_of::<WAVEFORMATEX>() + usize::from((*ptr).cbSize);
        let bytes = std::slice::from_raw_parts(ptr as *const u8, len).to_vec();
        CoTaskMemFree(Some(ptr as *const _));
        Ok((MixFormat::from_waveformat_bytes(&bytes)?, bytes))
    }
}

/// The failed call named when a raw stream's options are refused.
const RAW_PROPERTIES_CONTEXT: &str = "IAudioClient2::SetClientProperties (raw microphone)";

/// Declares `AUDCLNT_STREAMOPTIONS_RAW` on a capture client that is not initialised yet
/// (T12.32; Microsoft Learn, `IAudioClient2::SetClientProperties` and
/// `AUDCLNT_STREAMOPTIONS`), with the category a stream that declares nothing gets.
fn set_raw(client: &IAudioClient) -> Result<(), CaptureError> {
    let client2: IAudioClient2 = client.cast().ctx("get IAudioClient2")?;
    let properties = AudioClientProperties {
        cbSize: size_of::<AudioClientProperties>() as u32,
        bIsOffload: false.into(),
        eCategory: AudioCategory_Other,
        Options: AUDCLNT_STREAMOPTIONS_RAW,
    };
    // SAFETY: `properties` is a complete, initialised struct alive for the call, and the
    // client is not initialised yet, as SetClientProperties requires.
    unsafe { client2.SetClientProperties(&raw const properties) }.ctx(RAW_PROPERTIES_CONTEXT)
}

/// One event-driven, shared-mode capture stream. Stopped on drop.
pub struct CaptureStream<'com> {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
    /// Format of the delivered packets.
    format: MixFormat,
    /// Native mix format of the endpoint (the requested format for process loopback).
    device_format: MixFormat,
    device_name: String,
    device_id: Option<String>,
    /// The endpoint's properties (device endpoints only; `None` for process loopback).
    properties: Option<EndpointProperties>,
    /// Mono downmix (mic) or all channels interleaved (loopback).
    mode: ChannelMode,
    scratch: Vec<f32>,
    _com: PhantomData<&'com ComApartment>,
}

impl<'com> CaptureStream<'com> {
    /// Opens the default endpoint for `endpoint` in shared, event-driven mode with
    /// the device mix format (no conversion by the engine). Does not start it.
    pub fn open_default(com: &'com ComApartment, endpoint: Endpoint) -> Result<Self, CaptureError> {
        Self::open(com, endpoint, None, None)
    }

    /// Opens `endpoint` in shared, event-driven mode. Does not start it.
    ///
    /// Device endpoints use their mix format unless `rate` or `channels` is given and
    /// differs from it ([`conversion_target`]); then 32-bit float at that rate and
    /// channel count is requested and the audio engine converts (used when a
    /// re-opened device must keep the track's sample rate and channels). Process
    /// loopback always requests float at `rate` (default [`PROCESS_LOOPBACK_RATE`])
    /// with `channels` (default [`PROCESS_LOOPBACK_CHANNELS`]), since it has no mix
    /// format. A stream that would deliver another rate or channel count than requested
    /// is an error ([`check_delivered`]).
    pub fn open(
        com: &'com ComApartment,
        endpoint: Endpoint,
        rate: Option<u32>,
        channels: Option<u16>,
    ) -> Result<Self, CaptureError> {
        Self::open_on(com, endpoint, None, rate, channels)
    }

    /// [`open`](Self::open) on the endpoint with id `device` instead of the default one
    /// (T7.4i): a capture endpoint for the mic, a render endpoint for [`Endpoint::Loopback`];
    /// `None` is the default endpoint. A `device` that is not an active endpoint of that
    /// direction is [`CaptureError::DeviceGone`]. Process loopback has no endpoint and
    /// ignores `device`.
    pub fn open_on(
        com: &'com ComApartment,
        endpoint: Endpoint,
        device: Option<&str>,
        rate: Option<u32>,
        channels: Option<u16>,
    ) -> Result<Self, CaptureError> {
        Self::open_with(
            com,
            endpoint,
            device,
            rate,
            channels,
            MicProcessing::Default,
        )
    }

    /// [`open_on`](Self::open_on) with the microphone's signal processing chosen (T12.32).
    ///
    /// [`MicProcessing::Raw`] declares `AUDCLNT_STREAMOPTIONS_RAW` before the stream is
    /// initialised, so Windows leaves out the endpoint's default processing. It is an
    /// explicit request and never falls back: an endpoint that does not say it supports raw
    /// processing ([`raw_refusal`]), a stream that is not a microphone, or a refused
    /// `SetClientProperties` fails the open ([`CaptureError::RawMic`], or the Windows error).
    /// The raw stream's mix format is handled like any other: delivered as it is at the
    /// first open, converted by the audio engine to `rate` on a re-open, and refused when
    /// it is not a format [`MixFormat`] reads.
    pub fn open_with(
        com: &'com ComApartment,
        endpoint: Endpoint,
        device: Option<&str>,
        rate: Option<u32>,
        channels: Option<u16>,
        processing: MicProcessing,
    ) -> Result<Self, CaptureError> {
        let raw = processing == MicProcessing::Raw;
        if raw && !matches!(endpoint, Endpoint::Microphone(_)) {
            return Err(CaptureError::RawMic(RawRefusal::NotAMicrophone));
        }
        let convert = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let (client, device_name, device_id, properties, device_format, wfx, mut flags) =
            match endpoint {
                Endpoint::ProcessLoopback { pid } => {
                    let client = activate_process_loopback(pid)?;
                    let wfx = float_waveformat_bytes(
                        rate.unwrap_or(PROCESS_LOOPBACK_RATE),
                        channels.unwrap_or(PROCESS_LOOPBACK_CHANNELS),
                    )
                    .to_vec();
                    let fmt = MixFormat::from_waveformat_bytes(&wfx)?;
                    let name = match process_image_name(pid) {
                        Ok(exe) => format!("process loopback: {exe} (pid {pid})"),
                        Err(_) => format!("process loopback: pid {pid}"),
                    };
                    (
                        client,
                        name,
                        None,
                        None,
                        fmt,
                        wfx,
                        AUDCLNT_STREAMFLAGS_LOOPBACK | convert,
                    )
                }
                Endpoint::Microphone(_) | Endpoint::Loopback => {
                    let enumerator = device_enumerator(com)?;
                    let device = match device {
                        Some(id) => chosen_device(&enumerator, endpoint, id)?,
                        None => default_device(&enumerator, endpoint)?,
                    };
                    let name = friendly_name(&device)?;
                    let id = device_id(&device);
                    // SAFETY: COM is initialised; the result is an owned interface.
                    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.ctx(
                        if endpoint == Endpoint::Loopback {
                            LOOPBACK_ACTIVATE_CONTEXT
                        } else {
                            "activate IAudioClient"
                        },
                    )?;
                    let mut properties = endpoint_properties(&device, &name, None);
                    if raw {
                        if let Some(refusal) = raw_refusal(&properties) {
                            return Err(CaptureError::RawMic(refusal));
                        }
                        // Before the mix format is read: it is the raw stream's format that
                        // the stream is initialised with.
                        set_raw(&client)?;
                    }
                    let (mix, mix_bytes) = mix_format(&client)?;
                    properties.sample_rate = Some(mix.sample_rate);
                    let (wfx, flags) = match conversion_target(&mix, rate, channels) {
                        Some((r, ch)) => (float_waveformat_bytes(r, ch).to_vec(), convert),
                        None => (mix_bytes, 0),
                    };
                    let flags = if endpoint == Endpoint::Loopback {
                        flags | AUDCLNT_STREAMFLAGS_LOOPBACK
                    } else {
                        flags
                    };
                    (client, name, id, Some(properties), mix, wfx, flags)
                }
            };
        flags |= AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        let format = MixFormat::from_waveformat_bytes(&wfx)?;
        // Re-opened streams must keep their track's rate and channels; the observers
        // assert the channel count on every packet.
        check_delivered(&format, rate, channels)?;
        let event = Event::new()?;
        // SAFETY: `wfx` is a complete WAVEFORMATEX image (from GetMixFormat or built
        // with cbSize 0), alive for the duration of the call. The event handle
        // outlives the client (both are fields of Self; `client` is declared first
        // and dropped first).
        let capture: IAudioCaptureClient = unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    BUFFER_DURATION_HNS,
                    0,
                    wfx.as_ptr() as *const WAVEFORMATEX,
                    None,
                )
                .ctx(if flags & AUDCLNT_STREAMFLAGS_LOOPBACK != 0 {
                    LOOPBACK_INITIALIZE_CONTEXT
                } else {
                    "IAudioClient::Initialize"
                })?;
            client.SetEventHandle(event.0).ctx("SetEventHandle")?;
            client.GetService().ctx("get IAudioCaptureClient")?
        };
        Ok(Self {
            client,
            capture,
            event,
            format,
            device_format,
            device_name,
            device_id,
            properties,
            mode: endpoint.channel_mode(),
            scratch: Vec::new(),
            _com: PhantomData,
        })
    }

    /// Format of the delivered packets.
    pub fn format(&self) -> MixFormat {
        self.format
    }

    /// Channels per frame of what [`drain`](Self::drain) hands over: 1 for the mic,
    /// the delivered format's channels for a loopback.
    pub fn track_channels(&self) -> u16 {
        self.mode.track_channels(&self.format)
    }

    /// Native mix format of the endpoint (the requested format for process loopback).
    pub fn device_format(&self) -> MixFormat {
        self.device_format
    }

    /// Endpoint friendly name (or a process loopback label).
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// The endpoint's properties as read when it was opened (`None` for process
    /// loopback).
    pub fn endpoint_properties(&self) -> Option<&EndpointProperties> {
        self.properties.as_ref()
    }

    /// Endpoint id (`None` for process loopback).
    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }

    /// Name and delivered format.
    pub fn description(&self) -> DeviceDescription {
        DeviceDescription {
            name: self.device_name.clone(),
            format: self.format,
        }
    }

    /// Starts the stream.
    pub fn start(&self) -> Result<(), CaptureError> {
        // SAFETY: initialised client.
        unsafe { self.client.Start() }.ctx("IAudioClient::Start")
    }

    /// Stops the stream (buffered packets can still be drained).
    pub fn stop(&self) -> Result<(), CaptureError> {
        // SAFETY: initialised client.
        unsafe { self.client.Stop() }.ctx("IAudioClient::Stop")
    }

    /// Reads every pending packet, converts it to f32 (mono for the mic, interleaved
    /// with [`track_channels`](Self::track_channels) for a loopback) and hands it to
    /// `sink`.
    /// For packets flagged silent the slice is empty. A QPC position of 0 is never a
    /// valid timestamp and is reported as a timestamp error.
    pub fn drain(&mut self, mut sink: impl FnMut(PacketInfo, &[f32])) -> Result<(), CaptureError> {
        loop {
            // SAFETY: initialised capture client.
            let pending = unsafe { self.capture.GetNextPacketSize() }.ctx("GetNextPacketSize")?;
            if pending == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut device_position = 0u64;
            let mut qpc = 0u64;
            self.scratch.clear();
            // SAFETY: all out-pointers are valid locals. Between GetBuffer and
            // ReleaseBuffer, `data` points to frames * block_align readable bytes owned
            // by the engine; they are copied (converted) into `scratch` before release
            // and never touched afterwards.
            let converted = unsafe {
                self.capture
                    .GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut flags,
                        Some(&mut device_position),
                        Some(&mut qpc),
                    )
                    .ctx("IAudioCaptureClient::GetBuffer")?;
                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                let result = if silent || frames == 0 || data.is_null() {
                    Ok(0)
                } else {
                    let len = frames as usize * usize::from(self.format.block_align);
                    let bytes = std::slice::from_raw_parts(data, len);
                    self.mode.append_f32(bytes, &self.format, &mut self.scratch)
                };
                self.capture
                    .ReleaseBuffer(frames)
                    .ctx("IAudioCaptureClient::ReleaseBuffer")?;
                result
            };
            converted?;
            if frames == 0 {
                return Ok(());
            }
            let info = PacketInfo {
                qpc,
                device_position,
                frames,
                silent: flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0,
                discontinuity: flags & (AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32) != 0,
                timestamp_error: qpc == 0
                    || flags & (AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32) != 0,
            };
            sink(info, &self.scratch);
        }
    }
}

impl Drop for CaptureStream<'_> {
    fn drop(&mut self) {
        // SAFETY: initialised client; stopping a stopped stream is a no-op (S_FALSE).
        let _ = unsafe { self.client.Stop() };
    }
}

/// The id of the render endpoint `choice` names: the chosen endpoint when it is an active
/// output, else the Windows default output; `None` when neither can be read.
fn output_endpoint_id(com: &ComApartment, choice: &DeviceChoice) -> Option<String> {
    let enumerator = device_enumerator(com).ok()?;
    let device = match choice.id() {
        Some(id) => chosen_device(&enumerator, Endpoint::Loopback, id)
            .or_else(|_| default_device(&enumerator, Endpoint::Loopback)),
        None => default_device(&enumerator, Endpoint::Loopback),
    }
    .ok()?;
    device_id(&device)
}

/// How long [`RenderStream::play`] waits for the engine to play out what it was given
/// beyond the signal's own length (a stalled device ends the playback with an error).
const RENDER_DRAIN_GRACE: Duration = Duration::from_secs(2);
/// Polling interval of [`RenderStream::play`].
const RENDER_POLL: Duration = Duration::from_millis(5);

/// The audio category a render stream declares to Windows (`AUDIO_STREAM_CATEGORY`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderCategory {
    /// `AudioCategory_Other`, what a stream that declares nothing gets.
    Other,
    /// `AudioCategory_Communications`, as a call app declares.
    Communications,
}

/// One shared-mode render stream on an output endpoint (T7.4j: the audio check plays its
/// chirps through it). 32-bit float at the endpoint's mix rate, at most two channels (the
/// audio engine converts to the device format); polled, not event driven. Stopped on drop.
pub struct RenderStream<'com> {
    client: IAudioClient,
    render: IAudioRenderClient,
    rate: u32,
    channels: u16,
    buffer_frames: u32,
    device_name: String,
    device_id: Option<String>,
    _com: PhantomData<&'com ComApartment>,
}

impl<'com> RenderStream<'com> {
    /// Opens the output endpoint `device` (an id; `None` for the Windows default output) in
    /// shared mode. Does not start it. An id that is not an active output is
    /// [`CaptureError::DeviceGone`].
    pub fn open(com: &'com ComApartment, device: Option<&str>) -> Result<Self, CaptureError> {
        Self::open_as(com, device, RenderCategory::Other)
    }

    /// [`open`](Self::open), declaring `category` before the stream is initialised.
    pub fn open_as(
        com: &'com ComApartment,
        device: Option<&str>,
        category: RenderCategory,
    ) -> Result<Self, CaptureError> {
        let enumerator = device_enumerator(com)?;
        let endpoint = match device {
            Some(id) => chosen_device(&enumerator, Endpoint::Loopback, id)?,
            None => default_device(&enumerator, Endpoint::Loopback)?,
        };
        let device_name = friendly_name(&endpoint)?;
        let device_id = device_id(&endpoint);
        // SAFETY: COM is initialised; the result is an owned interface.
        let client: IAudioClient =
            unsafe { endpoint.Activate(CLSCTX_ALL, None) }.ctx(RENDER_ACTIVATE_CONTEXT)?;
        if category != RenderCategory::Other {
            let client2: IAudioClient2 = client.cast().ctx("get IAudioClient2")?;
            let properties = AudioClientProperties {
                cbSize: size_of::<AudioClientProperties>() as u32,
                bIsOffload: false.into(),
                eCategory: AudioCategory_Communications,
                Options: AUDCLNT_STREAMOPTIONS_NONE,
            };
            // SAFETY: `properties` is a complete, initialised struct alive for the call,
            // and the client is not initialised yet, as SetClientProperties requires.
            unsafe { client2.SetClientProperties(&raw const properties) }
                .ctx("IAudioClient2::SetClientProperties")?;
        }
        let (mix, _) = mix_format(&client)?;
        let channels = mix.channels.clamp(1, 2);
        let wfx = float_waveformat_bytes(mix.sample_rate, channels);
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        // SAFETY: `wfx` is a complete WAVEFORMATEX image built with cbSize 0, alive for the
        // call; the results are an owned interface and a frame count.
        let (render, buffer_frames) = unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    BUFFER_DURATION_HNS,
                    0,
                    wfx.as_ptr() as *const WAVEFORMATEX,
                    None,
                )
                .ctx(RENDER_INITIALIZE_CONTEXT)?;
            let frames = client.GetBufferSize().ctx("IAudioClient::GetBufferSize")?;
            let render: IAudioRenderClient = client.GetService().ctx("get IAudioRenderClient")?;
            (render, frames)
        };
        Ok(Self {
            client,
            render,
            rate: mix.sample_rate,
            channels,
            buffer_frames,
            device_name,
            device_id,
            _com: PhantomData,
        })
    }

    /// The rate the stream plays at (Hz): the signal given to [`play`](Self::play) must be
    /// at this rate.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Endpoint friendly name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Endpoint id.
    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }

    /// Plays the mono `signal` (at [`rate`](Self::rate), copied to every channel) once and
    /// returns when the engine has played it out, or soon after `stop` is set (the rest is
    /// not played). The stream is stopped either way.
    pub fn play(&mut self, signal: &[f32], stop: &AtomicBool) -> Result<(), CaptureError> {
        let channels = usize::from(self.channels);
        let length = Duration::from_secs_f64(signal.len() as f64 / f64::from(self.rate.max(1)));
        let deadline = std::time::Instant::now() + length + RENDER_DRAIN_GRACE;
        let mut written = 0usize;
        let mut started = false;
        let result = loop {
            if stop.load(Ordering::Acquire) {
                break Ok(());
            }
            // SAFETY: initialised client.
            let padding = match unsafe { self.client.GetCurrentPadding() }
                .ctx("IAudioClient::GetCurrentPadding")
            {
                Ok(p) => p,
                Err(e) => break Err(e),
            };
            if written >= signal.len() {
                if padding == 0 {
                    break Ok(());
                }
            } else {
                let n = (self.buffer_frames.saturating_sub(padding) as usize)
                    .min(signal.len() - written);
                if n > 0 {
                    if let Err(e) = self.write(&signal[written..written + n], channels) {
                        break Err(e);
                    }
                    written += n;
                }
                if !started {
                    // SAFETY: initialised client.
                    if let Err(e) =
                        unsafe { self.client.Start() }.ctx("IAudioClient::Start (render)")
                    {
                        break Err(e);
                    }
                    started = true;
                }
            }
            if std::time::Instant::now() > deadline {
                break Err(CaptureError::Windows {
                    context: "the output did not play the sound in time",
                    source: windows::core::Error::empty(),
                });
            }
            std::thread::sleep(RENDER_POLL);
        };
        // SAFETY: initialised client; stopping a stopped stream is a no-op (S_FALSE).
        let _ = unsafe { self.client.Stop() };
        result
    }

    /// Hands `frames` (mono, copied to every channel) to the engine.
    fn write(&self, frames: &[f32], channels: usize) -> Result<(), CaptureError> {
        let n = u32::try_from(frames.len()).map_err(|_| CaptureError::Windows {
            context: "render buffer too large",
            source: windows::core::Error::empty(),
        })?;
        // SAFETY: GetBuffer returns a writable buffer of `n` frames of `channels` f32
        // samples (the float format requested at Initialize; `n` is at most the free space
        // the padding left), owned by the engine until ReleaseBuffer; exactly that many
        // samples are written and the pointer is not used after the release.
        unsafe {
            let data = self
                .render
                .GetBuffer(n)
                .ctx("IAudioRenderClient::GetBuffer")?;
            let out = std::slice::from_raw_parts_mut(data.cast::<f32>(), frames.len() * channels);
            for (frame, &v) in out.chunks_exact_mut(channels).zip(frames) {
                frame.fill(v);
            }
            self.render
                .ReleaseBuffer(n, 0)
                .ctx("IAudioRenderClient::ReleaseBuffer")
        }
    }
}

impl Drop for RenderStream<'_> {
    fn drop(&mut self) {
        // SAFETY: initialised client; stopping a stopped stream is a no-op (S_FALSE).
        let _ = unsafe { self.client.Stop() };
    }
}

fn map_flow(flow: EDataFlow) -> Option<Flow> {
    if flow == eRender {
        Some(Flow::Render)
    } else if flow == eCapture {
        Some(Flow::Capture)
    } else {
        None
    }
}

fn map_role(role: ERole) -> Option<Role> {
    if role == eConsole {
        Some(Role::Console)
    } else if role == eMultimedia {
        Some(Role::Multimedia)
    } else if role == eCommunications {
        Some(Role::Communications)
    } else {
        None
    }
}

type ChangeQueue = Arc<Mutex<Vec<DefaultDeviceChange>>>;

/// `IMMNotificationClient` that queues default-device changes for the capture thread.
/// Callbacks arrive on other threads; they only push into the shared queue (they must
/// not block or call back into the enumerator).
#[implement(IMMNotificationClient)]
struct DeviceNotifier {
    queue: ChangeQueue,
}

impl IMMNotificationClient_Impl for DeviceNotifier_Impl {
    fn OnDeviceStateChanged(
        &self,
        _id: &PCWSTR,
        _state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        id: &PCWSTR,
    ) -> windows::core::Result<()> {
        let (Some(flow), Some(role)) = (map_flow(flow), map_role(role)) else {
            return Ok(());
        };
        let device_id = if id.is_null() {
            None
        } else {
            // SAFETY: a non-null id is a NUL-terminated string valid for the call.
            unsafe { id.to_string() }.ok()
        };
        if let Ok(mut q) = self.queue.lock() {
            q.push(DefaultDeviceChange {
                flow,
                role,
                device_id,
            });
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Registered default-device notifications; unregistered on drop.
pub struct DeviceNotifications<'com> {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    queue: ChangeQueue,
    _com: PhantomData<&'com ComApartment>,
}

impl<'com> DeviceNotifications<'com> {
    /// Registers for default-device changes.
    pub fn register(com: &'com ComApartment) -> Result<Self, CaptureError> {
        let enumerator = device_enumerator(com)?;
        let queue = ChangeQueue::default();
        let client: IMMNotificationClient = DeviceNotifier {
            queue: queue.clone(),
        }
        .into();
        // SAFETY: live enumerator and client; unregistered in Drop with the same
        // client before either is released.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client) }
            .ctx("RegisterEndpointNotificationCallback")?;
        Ok(Self {
            enumerator,
            client,
            queue,
            _com: PhantomData,
        })
    }

    /// Takes the queued changes, oldest first.
    pub fn take(&self) -> Vec<DefaultDeviceChange> {
        self.queue
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }
}

impl Drop for DeviceNotifications<'_> {
    fn drop(&mut self) {
        // SAFETY: the client was registered on this enumerator in `register`.
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.client)
        };
    }
}

/// Waits until any of `events` signals or `timeout` passes.
fn wait_any(events: &[HANDLE], timeout: Duration) -> Result<(), CaptureError> {
    if events.is_empty() {
        std::thread::sleep(timeout);
        return Ok(());
    }
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: every handle is a live event owned by a stream the caller borrows.
    let r = unsafe { WaitForMultipleObjects(events, false, ms) };
    if r == WAIT_FAILED {
        return Err(CaptureError::Windows {
            context: "WaitForMultipleObjects",
            source: windows::core::Error::from_thread(),
        });
    }
    Ok(())
}

/// One stream as the capture loop uses it (T9.21a): [`CaptureStream`] in production, a fake in
/// the loop's tests. Opened by [`LoopDriver::open`], not started.
trait LoopStream {
    /// The event the stream signals when a packet is ready.
    fn event(&self) -> HANDLE;
    /// [`CaptureStream::start`].
    fn start(&self) -> Result<(), CaptureError>;
    /// [`CaptureStream::stop`].
    fn stop(&self) -> Result<(), CaptureError>;
    /// [`CaptureStream::drain`].
    fn drain(&mut self, sink: &mut dyn FnMut(PacketInfo, &[f32])) -> Result<(), CaptureError>;
    /// [`CaptureStream::format`].
    fn format(&self) -> MixFormat;
    /// [`CaptureStream::device_format`].
    fn device_format(&self) -> MixFormat;
    /// [`CaptureStream::device_name`].
    fn device_name(&self) -> &str;
    /// [`CaptureStream::device_id`].
    fn device_id(&self) -> Option<&str>;
    /// [`CaptureStream::endpoint_properties`].
    fn endpoint_properties(&self) -> Option<&EndpointProperties>;
}

impl LoopStream for CaptureStream<'_> {
    fn event(&self) -> HANDLE {
        self.event.0
    }

    fn start(&self) -> Result<(), CaptureError> {
        CaptureStream::start(self)
    }

    fn stop(&self) -> Result<(), CaptureError> {
        CaptureStream::stop(self)
    }

    fn drain(&mut self, sink: &mut dyn FnMut(PacketInfo, &[f32])) -> Result<(), CaptureError> {
        CaptureStream::drain(self, sink)
    }

    fn format(&self) -> MixFormat {
        CaptureStream::format(self)
    }

    fn device_format(&self) -> MixFormat {
        CaptureStream::device_format(self)
    }

    fn device_name(&self) -> &str {
        CaptureStream::device_name(self)
    }

    fn device_id(&self) -> Option<&str> {
        CaptureStream::device_id(self)
    }

    fn endpoint_properties(&self) -> Option<&EndpointProperties> {
        CaptureStream::endpoint_properties(self)
    }
}

/// Everything else the capture loop asks of the platform (T9.21a): the wait, the clock,
/// opening a stream again, the default output's id and the default-device notifications.
/// [`Wasapi`] in production; the loop's tests drive a fake that can fail any call.
trait LoopDriver {
    /// The streams it opens.
    type Stream: LoopStream;
    /// [`wait_any`].
    fn wait_any(&mut self, events: &[HANDLE], timeout: Duration) -> Result<(), CaptureError>;
    /// [`qpc_now`].
    fn qpc_now(&mut self) -> Result<u64, CaptureError>;
    /// [`CaptureStream::open_with`].
    fn open(
        &mut self,
        endpoint: Endpoint,
        device: Option<&str>,
        rate: Option<u32>,
        channels: Option<u16>,
        processing: MicProcessing,
    ) -> Result<Self::Stream, CaptureError>;
    /// The id of the default render endpoint ([`output_endpoint_id`]).
    fn default_output_id(&mut self) -> Option<String>;
    /// The default-device changes queued since the last call ([`DeviceNotifications::take`]).
    fn take_changes(&mut self) -> Vec<DefaultDeviceChange>;
}

/// The production [`LoopDriver`]: the WASAPI calls, unchanged.
struct Wasapi<'com, 'n> {
    com: &'com ComApartment,
    notifications: &'n DeviceNotifications<'com>,
}

impl<'com> LoopDriver for Wasapi<'com, '_> {
    type Stream = CaptureStream<'com>;

    fn wait_any(&mut self, events: &[HANDLE], timeout: Duration) -> Result<(), CaptureError> {
        wait_any(events, timeout)
    }

    fn qpc_now(&mut self) -> Result<u64, CaptureError> {
        qpc_now()
    }

    fn open(
        &mut self,
        endpoint: Endpoint,
        device: Option<&str>,
        rate: Option<u32>,
        channels: Option<u16>,
        processing: MicProcessing,
    ) -> Result<Self::Stream, CaptureError> {
        CaptureStream::open_with(self.com, endpoint, device, rate, channels, processing)
    }

    fn default_output_id(&mut self) -> Option<String> {
        output_endpoint_id(self.com, &DeviceChoice::Default)
    }

    fn take_changes(&mut self) -> Vec<DefaultDeviceChange> {
        self.notifications.take()
    }
}

/// One captured track: its stream (absent while the device is lost), re-open
/// bookkeeping and the observer that receives its packets and re-opens.
struct Track<S, O> {
    endpoint: Endpoint,
    /// The endpoint opened: the chosen one (T7.4i), or the default after a fallback.
    device: DeviceChoice,
    /// The chosen endpoint's name, from when it is given up until the default opens (that
    /// re-open reports it).
    gave_up: Option<String>,
    /// `device` is an output the track followed a call onto (T6.15), not one the user chose:
    /// giving it up names no choice.
    followed_call: bool,
    /// Rate every re-opened stream is asked to deliver (the first stream's rate).
    rate: u32,
    /// Channels every re-opened stream is asked to deliver when the track keeps them
    /// (a loopback: the first stream's channels); `None` for the downmixed mic.
    channels: Option<u16>,
    /// The signal processing every re-opened stream is asked for (T12.32): a raw mic stays
    /// raw, so a re-open on an endpoint without raw support fails like any other failed
    /// re-open and the track never mixes raw and processed audio.
    processing: MicProcessing,
    stream: Option<S>,
    follower: StreamFollower,
    /// The last stream error that was neither a device loss nor a programming fault (T9.21):
    /// the failed call and its HRESULT.
    stream_error: Option<(&'static str, i32)>,
    /// The observer has heard that the track ended.
    end_reported: bool,
    observer: O,
}

impl<S: LoopStream, O: TrackObserver> Track<S, O> {
    fn new(
        stream: S,
        endpoint: Endpoint,
        device: DeviceChoice,
        mut follower: StreamFollower,
        observer: O,
    ) -> Self {
        follower.opened(stream.device_id().map(str::to_owned));
        let channels = match endpoint.channel_mode() {
            ChannelMode::Mono => None,
            ChannelMode::Interleaved => Some(stream.format().channels),
        };
        Self {
            endpoint,
            device,
            gave_up: None,
            followed_call: false,
            rate: stream.format().sample_rate,
            channels,
            processing: MicProcessing::Default,
            stream: Some(stream),
            follower,
            stream_error: None,
            end_reported: false,
            observer,
        }
    }

    /// The track of a stream that was opened with `processing`: re-opens ask for the same.
    /// Only the raw recorder asks for anything but the default.
    fn with_processing(mut self, processing: MicProcessing) -> Self {
        self.processing = processing;
        self
    }

    fn event(&self) -> Option<HANDLE> {
        self.stream.as_ref().map(LoopStream::event)
    }

    /// Drains pending packets into the observer at `now`; packets stamped later than `now`
    /// plus a margin are bogus. A device loss marks the stream for re-open instead of failing
    /// capture, and so does any other Windows error except a programming fault
    /// ([`follow::PROGRAMMING_FAULTS`]), but those only for
    /// [`follow::STREAM_ERROR_GRACE_QPC`], after which this track ends (T9.21). A programming
    /// fault, or an error that is not Windows' (a malformed packet), ends the capture.
    fn drain(&mut self, now: u64) -> Result<(), CaptureError> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        let observer = &mut self.observer;
        observer.set_horizon(now.saturating_add(QPC_LIMIT_MARGIN));
        match stream.drain(&mut |p, s| observer.packet(p, s)) {
            Ok(()) => {
                self.follower.stream_ran(now);
                Ok(())
            }
            Err(e) if e.is_device_loss() => {
                self.follower.on_stream_error();
                Ok(())
            }
            Err(e) => match e.windows_failure() {
                Some(failure) if !follow::is_programming_fault(failure.1) => {
                    self.stream_error = Some(failure);
                    self.follower.on_unknown_stream_error(now);
                    self.end_if_over(now);
                    Ok(())
                }
                _ => Err(e),
            },
        }
    }

    /// Once the follower has ended the track: closes its stream (keeping its last packets)
    /// and tells the observer, once.
    fn end_if_over(&mut self, now: u64) {
        if !self.follower.ended() || self.end_reported {
            return;
        }
        if let Some(mut old) = self.stream.take() {
            let _ = old.stop();
            let observer = &mut self.observer;
            observer.set_horizon(now.saturating_add(QPC_LIMIT_MARGIN));
            let _ = old.drain(&mut |p, s| observer.packet(p, s));
        }
        self.end_reported = true;
        let hresult = self.stream_error.map_or(0, |(_, hr)| hr);
        self.observer.track_ended(now, hresult);
    }

    /// The error that ended the track (T9.21), as a [`CaptureError`]; `None` while it records.
    fn ended_error(&self) -> Option<CaptureError> {
        if !self.follower.ended() {
            return None;
        }
        let (context, hresult) = self.stream_error?;
        Some(CaptureError::Windows {
            context,
            source: windows::core::Error::from_hresult(HRESULT(hresult)),
        })
    }

    /// Closes the current stream (keeping its last packets) and opens `endpoint` again
    /// at the track's rate (and, for a loopback, its channel count); the observer hears
    /// about every attempt. [`CaptureStream::open`] fails when the stream would deliver
    /// another rate or channel count. Failure leaves the track without a stream; the
    /// follower retries later. A chosen endpoint (T7.4i) that cannot be opened again is
    /// retried for [`follow::CHOSEN_DEVICE_GRACE_QPC`], then given up: the default endpoint
    /// is opened on the next pass of the loop, followed from then on, and that re-open names
    /// the chosen endpoint it replaces ([`ReopenOutcome::Opened::gave_up`];
    /// [`crate::devices`] module docs).
    fn reopen<D: LoopDriver<Stream = S>>(
        &mut self,
        driver: &mut D,
        reason: ReopenReason,
        now: u64,
    ) {
        if let Some(mut old) = self.stream.take() {
            let _ = old.stop();
            let observer = &mut self.observer;
            observer.set_horizon(now.saturating_add(QPC_LIMIT_MARGIN));
            let _ = old.drain(&mut |p, s| observer.packet(p, s));
        }
        let opened = driver
            .open(
                self.endpoint,
                self.device.id(),
                Some(self.rate),
                self.channels,
                self.processing,
            )
            .and_then(|s| {
                s.start()?;
                Ok(s)
            });
        match opened {
            Ok(s) => {
                self.follower.opened(s.device_id().map(str::to_owned));
                let gave_up = self.gave_up.take();
                self.observer.reopened(
                    now,
                    reason,
                    ReopenOutcome::Opened {
                        device_name: s.device_name(),
                        device_format: s.device_format(),
                        rate: s.format().sample_rate,
                        endpoint: s.endpoint_properties(),
                        gave_up: gave_up.as_deref(),
                    },
                );
                self.stream = Some(s);
            }
            Err(e) => {
                let failure = if e.is_device_gone() {
                    OpenFailure::DeviceGone
                } else {
                    OpenFailure::Other
                };
                self.follower.open_failed(now, failure);
                let error = e.detail();
                self.observer
                    .reopened(now, reason, ReopenOutcome::Failed { error: &error });
                if matches!(e, CaptureError::RawMic(_)) {
                    self.observer.raw_refused(now);
                }
                self.end_if_over(now);
                if self.follower.give_up_chosen(now)
                    && let DeviceChoice::Endpoint { name, .. } = std::mem::take(&mut self.device)
                    && !std::mem::take(&mut self.followed_call)
                {
                    self.gave_up = Some(name);
                }
            }
        }
    }

    /// Moves a device loopback onto the output a call plays on (T6.15). The new stream is opened
    /// and started before the old one is touched, so an output that cannot be used changes
    /// nothing: the error is returned and the track goes on recording where it was. From then on
    /// the track stays on that output like a chosen one, or follows the default if it is the
    /// default; a lost output is given up for the default ([`follow::CHOSEN_DEVICE_GRACE_QPC`]),
    /// and that re-open names no chosen endpoint, because the user chose none.
    fn move_output<D: LoopDriver<Stream = S>>(
        &mut self,
        driver: &mut D,
        target: &CallOutput,
        now: u64,
    ) -> Result<(), CaptureError> {
        let new = driver.open(
            self.endpoint,
            Some(&target.id),
            Some(self.rate),
            self.channels,
            self.processing,
        )?;
        new.start()?;
        if let Some(mut old) = self.stream.take() {
            let _ = old.stop();
            let observer = &mut self.observer;
            observer.set_horizon(now.saturating_add(QPC_LIMIT_MARGIN));
            let _ = old.drain(&mut |p, s| observer.packet(p, s));
        }
        let on_default = driver.default_output_id().as_deref() == Some(&target.id);
        (self.device, self.follower) = if on_default {
            (
                DeviceChoice::Default,
                StreamFollower::new(Flow::Render, Role::Console, true),
            )
        } else {
            (
                DeviceChoice::Endpoint {
                    id: target.id.clone(),
                    name: target.name.clone(),
                },
                StreamFollower::chosen(Flow::Render, Role::Console),
            )
        };
        self.follower.opened(new.device_id().map(str::to_owned));
        self.followed_call = !on_default;
        self.gave_up = None;
        self.observer.reopened(
            now,
            ReopenReason::CallOutputMoved,
            ReopenOutcome::Opened {
                device_name: new.device_name(),
                device_format: new.device_format(),
                rate: new.format().sample_rate,
                endpoint: new.endpoint_properties(),
                gave_up: None,
            },
        );
        self.stream = Some(new);
        Ok(())
    }

    fn stop(&self) {
        if let Some(s) = &self.stream {
            let _ = s.stop();
        }
    }

    /// After [`stop`](Self::stop): drains what the stream had already captured (errors
    /// ignored: the capture is over).
    fn flush<D: LoopDriver>(&mut self, driver: &mut D) {
        let now = driver.qpc_now().unwrap_or(u64::MAX);
        let _ = self.drain(now);
    }
}

/// A loopback stream, opened but not started, with how it was chosen.
struct OpenedLoopback<'com> {
    stream: CaptureStream<'com>,
    endpoint: Endpoint,
    source: LoopbackSource,
    /// The render endpoint of a device loopback: the chosen output, or the default after a
    /// fallback (always the default for process loopback, which has none).
    device: DeviceChoice,
    /// The chosen output could not be opened and the default is recorded instead.
    fallback: Option<DeviceFallback>,
    /// `LoopbackRequest::FollowCallOutput` with no output chosen (T6.15).
    follows_call: bool,
}

impl<'com> OpenedLoopback<'com> {
    /// The loopback track of the started stream: default-device loopback follows the
    /// console render default, a chosen output stays on it, process loopback (device
    /// independent) only recovers from stream errors.
    fn into_track<O: TrackObserver>(self, observer: O) -> Track<CaptureStream<'com>, O> {
        let follower = match (&self.device, self.endpoint) {
            (DeviceChoice::Endpoint { .. }, _) => {
                StreamFollower::chosen(Flow::Render, Role::Console)
            }
            (DeviceChoice::Default, e) => {
                StreamFollower::new(Flow::Render, Role::Console, e == Endpoint::Loopback)
            }
        };
        Track::new(self.stream, self.endpoint, self.device, follower, observer)
    }
}

/// Moves a loopback track onto the output a call plays on (T6.15, [`crate::call_output`]). The
/// sessions are read on their own thread, so the capture loop only looks at the newest reading.
struct CallOutputFollow {
    poller: RenderSessionPoller,
    rule: CallOutputFollower,
}

impl CallOutputFollow {
    fn start() -> Result<Self, CaptureError> {
        Ok(Self {
            poller: RenderSessionPoller::start(crate::session::CALL_POLL_INTERVAL)?,
            rule: CallOutputFollower::new(),
        })
    }

    /// Acts on the newest reading, if there is one.
    fn step<D: LoopDriver, O: TrackObserver>(
        &mut self,
        driver: &mut D,
        loopback: &mut Track<D::Stream, O>,
        now: u64,
    ) {
        let reading = self.poller.latest();
        follow_call_reading(&mut self.rule, reading, driver, loopback, now);
    }
}

/// [`CallOutputFollow::step`] on `reading` (`None`: nothing new). An output that cannot be
/// opened is reported once, its error as [`CaptureError::detail`] (never Windows' message
/// text), and the track stays where it is.
fn follow_call_reading<D: LoopDriver, O: TrackObserver>(
    rule: &mut CallOutputFollower,
    reading: Option<SessionReading>,
    driver: &mut D,
    loopback: &mut Track<D::Stream, O>,
    now: u64,
) {
    let sessions = match reading {
        None => return,
        Some(Err(_)) => {
            rule.no_reading();
            return;
        }
        Some(Ok(sessions)) => sessions,
    };
    let Some(target) = rule.reading(loopback.follower.current_id(), &sessions) else {
        return;
    };
    if let Err(e) = loopback.move_output(driver, &target, now) {
        rule.refused(&target.id);
        let used = loopback.stream.as_ref().map(LoopStream::device_name);
        loopback
            .observer
            .call_output_refused(now, &target.name, used, &e.detail());
    }
}

/// A stream opened for a choice (not started): the endpoint used and, when a chosen one
/// could not be opened, the fallback to the default.
type ChosenStream<'com> = (CaptureStream<'com>, DeviceChoice, Option<DeviceFallback>);

/// Opens `endpoint` on `choice` for `track` (not started); a chosen endpoint that cannot be
/// opened is replaced by the default one ([`crate::devices::fallback`]). An error opening
/// the default is the capture's.
fn open_chosen<'com>(
    com: &'com ComApartment,
    endpoint: Endpoint,
    track: TrackId,
    choice: &DeviceChoice,
) -> Result<ChosenStream<'com>, CaptureError> {
    let Some(id) = choice.id() else {
        return Ok((
            CaptureStream::open(com, endpoint, None, None)?,
            DeviceChoice::Default,
            None,
        ));
    };
    match CaptureStream::open_on(com, endpoint, Some(id), None, None) {
        Ok(stream) => Ok((stream, choice.clone(), None)),
        Err(e) => {
            let stream = CaptureStream::open(com, endpoint, None, None)?;
            let fallback = crate::devices::fallback(
                track,
                choice,
                e.is_device_gone(),
                &e.detail(),
                stream.device_name(),
            );
            Ok((stream, DeviceChoice::Default, fallback))
        }
    }
}

/// Opens the microphone for `choice` (the default for `mic_role` unless one is chosen;
/// not started).
fn open_mic<'com>(
    com: &'com ComApartment,
    mic_role: MicRole,
    choice: &DeviceChoice,
) -> Result<ChosenStream<'com>, CaptureError> {
    open_chosen(com, Endpoint::Microphone(mic_role), TrackId::Mic, choice)
}

/// The mic track of the started stream on `device`: the default follows its role's default
/// capture device, a chosen mic stays on it.
fn mic_track<'com, O: TrackObserver>(
    stream: CaptureStream<'com>,
    mic_role: MicRole,
    device: DeviceChoice,
    observer: O,
) -> Track<CaptureStream<'com>, O> {
    let role = mic_follow_role(mic_role);
    let follower = match device {
        DeviceChoice::Default => StreamFollower::new(Flow::Capture, role, true),
        DeviceChoice::Endpoint { .. } => StreamFollower::chosen(Flow::Capture, role),
    };
    Track::new(
        stream,
        Endpoint::Microphone(mic_role),
        device,
        follower,
        observer,
    )
}

/// Takes the origin QPC and starts every stream immediately after it: frame 0 of every
/// track is the origin, and no track records its own start.
fn start_streams(
    mic: &CaptureStream<'_>,
    loopback: Option<&CaptureStream<'_>>,
) -> Result<u64, CaptureError> {
    let origin = qpc_now()?;
    mic.start()?;
    if let Some(lb) = loopback {
        lb.start()?;
    }
    Ok(origin)
}

/// The event-driven capture loop shared by [`record_raw`] and [`WasapiSource`]: waits
/// on the stream events (at most [`WAIT_MS`]), drains every track (the mic, and the
/// loopback unless capturing in the room), feeds default-device notifications to the
/// followers and re-opens streams as they decide. Returns the QPC time at which
/// `done(now)` held or an observer stopped wanting packets. Streams are left running;
/// the caller closes the tracks.
fn run_capture<D: LoopDriver, M: TrackObserver, L: TrackObserver>(
    driver: &mut D,
    mic: &mut Track<D::Stream, M>,
    mut loopback: Option<&mut Track<D::Stream, L>>,
    mut call_output: Option<CallOutputFollow>,
    mut done: impl FnMut(u64) -> bool,
) -> Result<u64, CaptureError> {
    loop {
        let events: Vec<HANDLE> = [mic.event(), loopback.as_ref().and_then(|t| t.event())]
            .into_iter()
            .flatten()
            .collect();
        driver.wait_any(&events, Duration::from_millis(u64::from(WAIT_MS)))?;
        let drained_at = driver.qpc_now()?;
        mic.drain(drained_at)?;
        if let Some(lb) = loopback.as_deref_mut() {
            lb.drain(drained_at)?;
        }
        for change in driver.take_changes() {
            mic.follower.on_default_changed(&change);
            if let Some(lb) = loopback.as_deref_mut() {
                lb.follower.on_default_changed(&change);
            }
        }
        let now = driver.qpc_now()?;
        let loopback_done = loopback.as_ref().is_some_and(|t| !t.observer.wants_more());
        if done(now) || !mic.observer.wants_more() || loopback_done {
            return Ok(now);
        }
        if let Some(reason) = mic.follower.take_reopen(now) {
            mic.reopen(driver, reason, now);
        }
        if let Some(lb) = loopback.as_deref_mut()
            && let Some(reason) = lb.follower.take_reopen(now)
        {
            lb.reopen(driver, reason, now);
        }
        if let (Some(follow), Some(lb)) = (call_output.as_mut(), loopback.as_deref_mut()) {
            follow.step(driver, lb, now);
        }
        // A track ended by stream errors (T9.21) leaves the other recording; with none left
        // the capture has failed.
        if let Some(e) = mic.ended_error()
            && loopback.as_ref().is_none_or(|t| t.follower.ended())
        {
            return Err(e);
        }
    }
}

fn mic_follow_role(role: MicRole) -> Role {
    match role {
        MicRole::Console => Role::Console,
        MicRole::Communications => Role::Communications,
    }
}

/// Opens the loopback stream for `request`: process loopback of the chosen process
/// tree, or the render endpoint `output` (the default one unless one is chosen, T7.4i).
/// `Auto` falls back to that render endpoint when no call app is found, sessions cannot be
/// listed or process loopback is unavailable; explicit names and pids never fall back.
fn open_loopback<'com>(
    com: &'com ComApartment,
    request: &LoopbackRequest,
    output: &DeviceChoice,
) -> Result<OpenedLoopback<'com>, CaptureError> {
    let auto = *request == LoopbackRequest::Auto;
    let choice = match request {
        LoopbackRequest::DefaultDevice | LoopbackRequest::FollowCallOutput => {
            LoopbackChoice::DefaultDevice(FallbackReason::Requested)
        }
        _ => match list_render_sessions(com) {
            Ok(sessions) => choose_loopback(request, &sessions).map_err(CaptureError::Selection)?,
            Err(e) if auto => {
                LoopbackChoice::DefaultDevice(FallbackReason::SessionQueryFailed(e.detail()))
            }
            Err(_) if matches!(request, LoopbackRequest::Pid(_)) => {
                choose_loopback(request, &[]).map_err(CaptureError::Selection)?
            }
            Err(e) => return Err(e),
        },
    };
    let default_device = |choice: LoopbackChoice| {
        // The device's own channels (stereo on a desktop), kept for the reference.
        let (stream, device, fallback) =
            open_chosen(com, Endpoint::Loopback, TrackId::Reference, output)?;
        Ok(OpenedLoopback {
            stream,
            endpoint: Endpoint::Loopback,
            source: choice.source(request),
            device,
            fallback,
            // A chosen output always wins, even one replaced by the default for now.
            follows_call: *request == LoopbackRequest::FollowCallOutput && output.id().is_none(),
        })
    };
    match choice {
        LoopbackChoice::Process(ref target) => {
            let endpoint = Endpoint::ProcessLoopback { pid: target.pid };
            // Process loopback of a pid that does not exist activates fine and
            // records silence, so check the process first.
            let opened = process_image_name(target.pid)
                .map_err(|_| CaptureError::Selection(SelectError::NoSuchProcess(target.pid)))
                // Stereo (PROCESS_LOOPBACK_CHANNELS) at PROCESS_LOOPBACK_RATE.
                .and_then(|exe| Ok((exe, CaptureStream::open(com, endpoint, None, None)?)));
            match opened {
                Ok((exe, stream)) => {
                    let mut source = choice.source(request);
                    source.process_name.get_or_insert(exe);
                    Ok(OpenedLoopback {
                        stream,
                        endpoint,
                        source,
                        device: DeviceChoice::Default,
                        fallback: None,
                        follows_call: false,
                    })
                }
                Err(e) if auto => default_device(LoopbackChoice::DefaultDevice(
                    FallbackReason::ProcessLoopbackUnavailable(e.detail()),
                )),
                Err(e) => Err(e),
            }
        }
        LoopbackChoice::DefaultDevice(_) => default_device(choice),
    }
}

/// Records the default microphone (for `mic_role`) and the loopback source chosen for
/// `loopback` (see [`LoopbackRequest`]) for `seconds`, with gaps filled by silence
/// from QPC timestamps.
///
/// `announce` receives the mic and loopback devices and the loopback decision after
/// both streams are opened and before capture starts. The origin QPC is taken after
/// opening and immediately before `Start` on either stream; frame 0 of both tracks is
/// placed at that origin.
///
/// `on_mic_warning` hears about a Bluetooth hands-free mic (T6.24): the mic opened at
/// the start (right after `announce`, before capture starts) and, during the recording,
/// each re-open that switches onto one (the [`HandsFreeFollower`](crate::endpoint::HandsFreeFollower)
/// rule of the live mic track), as it happens; the latter are also logged in the
/// change's [`DeviceChange::hands_free`](crate::recording::DeviceChange::hands_free).
///
/// The mic track is mono; the loopback track keeps its channels (interleaved, stereo
/// on a desktop and for process loopback).
///
/// Default-device changes re-open the mic (and the default-device loopback) on the
/// new endpoint at the track's original rate (and the loopback's channel count);
/// process loopback is device independent and is only re-opened after a stream error.
/// Each re-open is recorded as a [`DeviceChange`] and the gap is filled with silence.
///
/// `mic_processing` is how the mic stream is opened (T12.32). [`MicProcessing::Raw`] asks
/// Windows for the mic without the endpoint's default signal processing, for measurements:
/// a mic that cannot be opened raw fails the recording before anything is captured
/// ([`CaptureError::RawMic`]), and a re-open during the recording asks for raw again, so a
/// new default mic without raw support leaves the mic track silent (the failed re-open is in
/// its [`DeviceChange`]s, the silence counted in
/// [`TrackRecording::raw_refused_frames`](crate::recording::TrackRecording::raw_refused_frames))
/// instead of recording processed audio into a raw recording. Only
/// this recorder can ask for it: live sessions always open the mic with the default.
///
/// Runs on the calling thread and initialises COM (MTA) for the duration of the call.
pub fn record_raw(
    seconds: f64,
    mic_role: MicRole,
    mic_processing: MicProcessing,
    loopback: &LoopbackRequest,
    announce: impl FnOnce(&DeviceDescription, &DeviceDescription, &LoopbackSource),
    mut on_mic_warning: impl FnMut(&HandsFreeMic),
) -> Result<RawRecording, CaptureError> {
    // As for a live session; see `capture_thread`.
    let _awake = KeepAwake::hold();
    let com = ComApartment::init_mta()?;
    // Registered before the streams open so no change in between is missed.
    let notifications = DeviceNotifications::register(&com)?;
    let (mic_stream, mic_device) = match mic_processing {
        MicProcessing::Default => {
            let (stream, device, _) = open_mic(&com, mic_role, &DeviceChoice::Default)?;
            (stream, device)
        }
        // Raw is refused, never replaced: no fallback of any kind.
        MicProcessing::Raw => (
            CaptureStream::open_with(
                &com,
                Endpoint::Microphone(mic_role),
                None,
                None,
                None,
                MicProcessing::Raw,
            )?,
            DeviceChoice::Default,
        ),
    };
    let opened = open_loopback(&com, loopback, &DeviceChoice::Default)?;
    announce(
        &mic_stream.description(),
        &opened.stream.description(),
        &opened.source,
    );
    let mic_properties = mic_stream.endpoint_properties().cloned();
    if let Some(w) = mic_properties
        .as_ref()
        .and_then(crate::endpoint::hands_free_warning)
    {
        on_mic_warning(&w);
    }

    let start = start_streams(&mic_stream, Some(&opened.stream))?;
    let deadline = start + (seconds.max(0.0) * QPC_UNITS_PER_SEC as f64) as u64;
    let limit = deadline.saturating_add(QPC_LIMIT_MARGIN);
    let recording = |s: &CaptureStream<'_>| {
        let mut timeline =
            TrackTimeline::new(s.format().sample_rate, start).with_channels(s.track_channels());
        timeline.set_qpc_limit(limit);
        RecordingTrack::new(s.description(), timeline)
    };
    let mic_obs = NotifyingTrack::new(
        recording(&mic_stream).with_mic_warnings(mic_properties.as_ref()),
        on_mic_warning,
    );
    let lb_obs = recording(&opened.stream);
    let loopback_source = opened.source.clone();
    let mut mic =
        mic_track(mic_stream, mic_role, mic_device, mic_obs).with_processing(mic_processing);
    let follow = opened
        .follows_call
        .then(CallOutputFollow::start)
        .transpose()?;
    let mut lb = opened.into_track(lb_obs);

    let mut driver = Wasapi {
        com: &com,
        notifications: &notifications,
    };
    let captured = run_capture(&mut driver, &mut mic, Some(&mut lb), follow, |now| {
        now >= deadline
    });
    mic.stop();
    lb.stop();
    let end = captured?;
    // Packets already captured before the stop.
    mic.flush(&mut driver);
    lb.flush(&mut driver);
    let mic = mic.observer.into_track().finish(end);
    let refused_call_outputs = lb.observer.refused_call_outputs().to_vec();
    let lb = lb.observer.finish(end);
    drop(notifications);

    Ok(RawRecording {
        mic_role,
        mic_processing,
        requested_seconds: seconds,
        refused_call_outputs,
        start_qpc: start,
        end_qpc: end,
        loopback_source,
        mic,
        loopback: lb,
    })
}

/// What the capture thread reports once every stream has started.
struct Started {
    session: CaptureSession,
    mic: DeviceDescription,
    /// The mic endpoint's properties (T6.24).
    mic_properties: Option<EndpointProperties>,
    /// The loopback device and how it was chosen; `None` in the room.
    loopback: Option<(DeviceDescription, LoopbackSource)>,
    /// Chosen endpoints replaced by the default (T7.4i).
    fallbacks: Vec<DeviceFallback>,
    /// The render endpoint the reference belongs to (T7.4j,
    /// [`WasapiSource::output_endpoint_id`]).
    output_endpoint: Option<String>,
}

fn track_info(s: &CaptureStream<'_>) -> Result<TrackInfo, CaptureError> {
    let rate = SampleRate::new(s.format().sample_rate)
        .ok_or_else(|| FormatError::Unsupported("sample rate 0 Hz".into()))?;
    let channels = NonZeroU16::new(s.track_channels())
        .ok_or_else(|| FormatError::Unsupported("zero channels".into()))?;
    Ok(TrackInfo::new(rate)
        .with_channels(channels)
        .with_device_name(s.device_name()))
}

/// Live mic + loopback capture as the core [`AudioSource`] (T6.3b), or the mic alone in
/// the room (T6.27).
///
/// A dedicated thread (`earshot-capture`) owns the COM apartment and every stream
/// (COM objects are `!Send`). It runs the same event-driven loop as [`record_raw`],
/// including default-device following and device-loss recovery, and forwards
/// [`CaptureEvent`](earshot_core::capture::CaptureEvent)s over a channel; this value is
/// the consumer side and is `Send`.
///
/// - Tracks: [`TrackId::Mic`] is the default capture device for the mic role, or the
///   chosen microphone ([`start_on`](Self::start_on), T7.4i). In a call
///   ([`CaptureMode::Call`]) [`TrackId::Reference`] is the loopback source chosen like
///   `record-raw` (`--loopback-process`): process loopback of a call app, or the default
///   (or chosen) render endpoint. A chosen endpoint that cannot be opened at the start is
///   replaced by the default ([`fallbacks`](Self::fallbacks)); one lost during the capture
///   is given up for the default ([`crate::devices`] module docs). In the room ([`CaptureMode::InRoom`]) no loopback is opened and the
///   session has no reference track (`CaptureSession::reference` is `None`): the
///   pipeline then runs no echo canceller and the meeting has one track.
/// - Frames: `f32` at the device's native rate (normally 48 kHz); the pipeline
///   resamples (PLAN.md 3.3). The mic is mono; the reference keeps the loopback's
///   channels interleaved (stereo on a desktop; `TrackInfo::channels`), and consumers
///   that need one channel mix it with `earshot_core::capture::mix_to_mono`.
///   Timestamps are QPC positions passed through unchanged as [`DeviceTime`] (both are
///   100 ns ticks).
/// - Origin: the QPC time taken immediately before both streams start; frame 0 of
///   both tracks, as in a raw recording. Frames stamped before it are trimmed.
/// - Discontinuities: `DataLost` for packets flagged `DATA_DISCONTINUITY`,
///   `DeviceChanged` (new endpoint name, `None` while no device is available, the
///   rate of the following frames, and on the mic track a warning once per switch onto a
///   Bluetooth hands-free mic) for every reported re-open.
/// - Backpressure: the channel to the consumer holds at most
///   [`LIVE_QUEUE_CAP`](crate::LIVE_QUEUE_CAP) of audio (both tracks at their opening
///   rates). A consumer that stalls longer loses frames; when it catches up, each track's
///   next frames are preceded by one `DataLost` marker (see [`ChannelSink`]).
/// - [`pump`](AudioSource::pump) never blocks; [`stop`](AudioSource::stop) waits for
///   the thread to flush and exit (one wait period, about 20 ms, unless a re-open is in
///   progress: process loopback activation can take up to 5 s), after which the next
///   pump delivers the rest and [`EndOfStream`] with [`EndReason::Stopped`]. A thread
///   that has not ended within [`STOP_WAIT`](crate::STOP_WAIT) (stuck in a COM call) is
///   given up and never joined, and the source ends without it (T9.20,
///   [`ChannelSource`]). A fatal
///   capture error is returned once by `pump`, followed by [`EndOfStream`] with
///   [`EndReason::Failed`]. The end of stream is delivered exactly once, last.
pub struct WasapiSource {
    inner: ChannelSource<CaptureError>,
    mic: DeviceDescription,
    mic_properties: Option<EndpointProperties>,
    loopback: Option<(DeviceDescription, LoopbackSource)>,
    fallbacks: Vec<DeviceFallback>,
    output_endpoint: Option<String>,
}

impl fmt::Debug for WasapiSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasapiSource")
            .field("inner", &self.inner)
            .field("mic", &self.mic)
            .field("mic_properties", &self.mic_properties)
            .field("loopback", &self.loopback)
            .field("fallbacks", &self.fallbacks)
            .field("output_endpoint", &self.output_endpoint)
            .finish()
    }
}

impl WasapiSource {
    /// Opens the default microphone for `mic_role` and, in a call, the loopback source
    /// of `mode`; starts the streams on a new capture thread and returns once capture is
    /// running. Explicit loopback processes that do not exist fail here (only `Auto`
    /// falls back to the default device). In the room only the microphone is opened.
    pub fn start(mic_role: MicRole, mode: &CaptureMode) -> Result<Self, CaptureError> {
        Self::start_on(mic_role, mode, &DeviceSelection::default())
    }

    /// [`start`](Self::start) on the endpoints of `devices` (T7.4i): the chosen microphone
    /// instead of the default for `mic_role`, and the chosen output for a device loopback. A
    /// chosen endpoint that cannot be opened is replaced by the default and listed in
    /// [`fallbacks`](Self::fallbacks).
    pub fn start_on(
        mic_role: MicRole,
        mode: &CaptureMode,
        devices: &DeviceSelection,
    ) -> Result<Self, CaptureError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (tx, rx) = mpsc::channel();
        let request = LiveRequest {
            mic_role,
            loopback: mode.loopback().cloned(),
            devices: devices.clone(),
        };
        let flag = Arc::clone(&stop);
        let gauge = QueueGauge::new();
        let producer_gauge = gauge.clone();
        let handle = std::thread::Builder::new()
            .name("earshot-capture".into())
            .spawn(move || {
                capture_thread(&request, &flag, &ready_tx, tx, producer_gauge);
            })
            .map_err(CaptureError::Thread)?;
        let started = match ready_rx.recv() {
            Ok(Ok(started)) => started,
            Ok(Err(e)) => {
                let _ = handle.join();
                return Err(e);
            }
            Err(_) => {
                let _ = handle.join();
                return Err(CaptureError::ThreadExited);
            }
        };
        let stopper = move || {
            stop.store(true, Ordering::Release);
            // A panicked capture thread surfaces through the channel instead.
            let _ = handle.join();
        };
        Ok(Self {
            inner: ChannelSource::new(started.session, rx, stopper).with_gauge(gauge),
            mic: started.mic,
            mic_properties: started.mic_properties,
            loopback: started.loopback,
            fallbacks: started.fallbacks,
            output_endpoint: started.output_endpoint,
        })
    }

    /// The id of the render endpoint the reference track belongs to (T7.4j: the key of the
    /// audio check's stored delay): the endpoint whose loopback is recorded, or for process
    /// loopback the output the selection names (the chosen endpoint when it is active, else
    /// the Windows default), read when capture started. `None` in the room or when Windows
    /// gave no id.
    pub fn output_endpoint_id(&self) -> Option<&str> {
        self.output_endpoint.as_deref()
    }

    /// The chosen endpoints that could not be opened at the start, each replaced by the
    /// default one (empty when every choice was used or nothing was chosen).
    pub fn fallbacks(&self) -> &[DeviceFallback] {
        &self.fallbacks
    }

    /// The microphone endpoint and the format it delivered when capture started.
    pub fn mic_device(&self) -> &DeviceDescription {
        &self.mic
    }

    /// The mic endpoint's properties as read when capture started (T6.24).
    pub fn mic_endpoint(&self) -> Option<&EndpointProperties> {
        self.mic_properties.as_ref()
    }

    /// The warning for a mic that is a Bluetooth hands-free headset
    /// ([`crate::endpoint::hands_free_warning`]); `None` for any other mic. Judged on the
    /// endpoint opened at the start; a later switch onto a hands-free mic is announced
    /// in that re-open's `DeviceChanged` marker (`EndpointWarning::BluetoothHandsFree`).
    pub fn mic_warning(&self) -> Option<HandsFreeMic> {
        self.mic_properties
            .as_ref()
            .and_then(crate::endpoint::hands_free_warning)
    }

    /// The loopback endpoint (or process loopback label) and its delivered format;
    /// `None` in the room.
    pub fn loopback_device(&self) -> Option<&DeviceDescription> {
        self.loopback.as_ref().map(|(d, _)| d)
    }

    /// How the loopback track is captured; `None` in the room.
    pub fn loopback_source(&self) -> Option<&LoopbackSource> {
        self.loopback.as_ref().map(|(_, s)| s)
    }
}

impl AudioSource for WasapiSource {
    type Error = LiveError<CaptureError>;

    fn session(&self) -> &CaptureSession {
        self.inner.session()
    }

    fn pump(&mut self, sink: &mut dyn CaptureSink) -> Result<SourceStatus, Self::Error> {
        self.inner.pump(sink)
    }

    fn stop(&mut self) {
        self.inner.stop();
    }
}

type ReadyTx = mpsc::SyncSender<Result<Started, CaptureError>>;

/// Body of the `earshot-capture` thread: owns the COM apartment for its lifetime.
/// `request` is the loopback of a call; `None` captures the mic alone (in the room).
fn capture_thread(
    request: &LiveRequest,
    stop: &AtomicBool,
    ready: &ReadyTx,
    tx: mpsc::Sender<LiveMsg<CaptureError>>,
    gauge: QueueGauge,
) {
    // Held for as long as capture runs. A refusal costs nothing but the protection: a recording
    // never depends on it, and any gap a sleep leaves is reported by the runtime.
    let _awake = KeepAwake::hold();
    match ComApartment::init_mta() {
        Ok(com) => live_capture(&com, request, stop, ready, tx, gauge),
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

/// What a live capture opens: the mic for its role or choice, and the loopback of a call
/// (`None` in the room) on the chosen output.
struct LiveRequest {
    mic_role: MicRole,
    loopback: Option<LoopbackRequest>,
    devices: DeviceSelection,
}

fn live_capture(
    com: &ComApartment,
    request: &LiveRequest,
    stop: &AtomicBool,
    ready: &ReadyTx,
    tx: mpsc::Sender<LiveMsg<CaptureError>>,
    gauge: QueueGauge,
) {
    let setup = (|| {
        // Registered before the streams open so no change in between is missed.
        let notifications = DeviceNotifications::register(com)?;
        let (mic_stream, mic_device, mic_fallback) =
            open_mic(com, request.mic_role, &request.devices.microphone)?;
        let mut opened = request
            .loopback
            .as_ref()
            .map(|r| open_loopback(com, r, &request.devices.output))
            .transpose()?;
        let fallbacks: Vec<DeviceFallback> = mic_fallback
            .into_iter()
            .chain(opened.as_mut().and_then(|o| o.fallback.take()))
            .collect();
        let mic = track_info(&mic_stream)?;
        let loopback = opened.as_ref().map(|o| track_info(&o.stream)).transpose()?;
        let infos = (mic.clone(), loopback.clone());
        let origin = start_streams(&mic_stream, opened.as_ref().map(|o| &o.stream))?;
        let started = Started {
            session: CaptureSession::new(DeviceTime::from_ticks(origin), mic, loopback),
            mic: mic_stream.description(),
            mic_properties: mic_stream.endpoint_properties().cloned(),
            loopback: opened
                .as_ref()
                .map(|o| (o.stream.description(), o.source.clone())),
            fallbacks,
            output_endpoint: opened.as_ref().and_then(|o| {
                o.stream
                    .device_id()
                    .map(str::to_owned)
                    .or_else(|| output_endpoint_id(com, &request.devices.output))
            }),
        };
        Ok::<_, CaptureError>((
            notifications,
            (mic_stream, mic_device),
            opened,
            started,
            infos,
        ))
    })();
    let (notifications, (mic_stream, mic_device), opened, started, (mic_info, lb_info)) =
        match setup {
            Ok(s) => s,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
    let call_output = match opened
        .as_ref()
        .filter(|o| o.follows_call)
        .map(|_| CallOutputFollow::start())
        .transpose()
    {
        Ok(follow) => follow,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let origin = started.session.origin;
    let limit = queue_limit(&mic_info, lb_info.as_ref());
    let emitter = |track, info: &TrackInfo| EmitterTrack {
        emitter: TrackEmitter::new(track, info.rate, origin).with_channels(info.channels),
        sink: ChannelSink::with_cap(tx.clone(), gauge.clone(), limit),
    };
    let mut mic_obs = emitter(TrackId::Mic, &mic_info);
    // A mid-meeting switch onto a hands-free mic is warned in its DeviceChanged marker;
    // the mic opened now is warned by the caller (`WasapiSource::mic_warning`).
    mic_obs.emitter = mic_obs
        .emitter
        .with_mic_warnings(started.mic_properties.as_ref());
    let lb_obs = lb_info.as_ref().map(|i| emitter(TrackId::Reference, i));
    if ready.send(Ok(started)).is_err() {
        return;
    }
    let mut mic = mic_track(mic_stream, request.mic_role, mic_device, mic_obs);
    // `opened` and `lb_obs` are both present exactly when a loopback was requested.
    let mut lb = opened.zip(lb_obs).map(|(o, obs)| o.into_track(obs));
    let mut driver = Wasapi {
        com,
        notifications: &notifications,
    };
    let result = run_capture(&mut driver, &mut mic, lb.as_mut(), call_output, |_| {
        stop.load(Ordering::Acquire)
    });
    mic.stop();
    if let Some(lb) = &lb {
        lb.stop();
    }
    mic.flush(&mut driver);
    if let Some(lb) = &mut lb {
        lb.flush(&mut driver);
    }
    let last = mic
        .observer
        .emitter
        .end()
        .max(lb.as_ref().and_then(|t| t.observer.emitter.end()));
    let now = qpc_now().map(DeviceTime::from_ticks).unwrap_or(origin);
    let at = last.map_or(now, |last| last.max(now));
    let mut sink = ChannelSink::new(tx);
    let reason = match result {
        Ok(_) => EndReason::Stopped,
        Err(e) => {
            sink.send(LiveMsg::Failed(e));
            EndReason::Failed
        }
    };
    sink.end(EndOfStream { at, reason });
}

/// Ctrl+C / Ctrl+Break presses seen by the console control handler, and how many of them
/// the process handles itself ([`install_ctrl_c_handler`]).
#[derive(Debug)]
pub struct CtrlC {
    presses: AtomicU32,
    handled: AtomicU32,
}

impl CtrlC {
    const fn new() -> Self {
        CtrlC {
            presses: AtomicU32::new(0),
            handled: AtomicU32::new(1),
        }
    }

    /// Presses handled so far (a press beyond the allowance ends the process instead).
    #[must_use]
    pub fn presses(&self) -> u32 {
        self.presses.load(Ordering::SeqCst)
    }

    /// Lets the next press be handled too (for example to cancel the post-meeting cleanup
    /// after the first press stopped the recording); a press after that ends the process.
    pub fn allow_one_more(&self) {
        self.handled.store(self.presses() + 1, Ordering::SeqCst);
    }
}

static CTRL_C: CtrlC = CtrlC::new();

/// What the console control handler does with `ctrl`: a Ctrl+C or Ctrl+Break within the
/// allowance (one by default, raised by [`CtrlC::allow_one_more`]) is counted and handled,
/// so the process keeps running and can stop capture, finalise its recording or cancel
/// its cleanup; one beyond the allowance is not handled, so the default handler ends the
/// process (the user insists, and the process can always be killed). Other events (closing
/// the console, logoff, shutdown) go to the default handler.
fn console_ctrl(ctrl: u32, state: &CtrlC) -> bool {
    if ctrl != CTRL_C_EVENT && ctrl != CTRL_BREAK_EVENT {
        return false;
    }
    let handled = state.handled.load(Ordering::SeqCst);
    state
        .presses
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < handled).then_some(n + 1)
        })
        .is_ok()
}

/// The registered handler. The system calls it on a thread of its own; it only touches
/// atomics.
unsafe extern "system" fn on_console_ctrl(ctrl: u32) -> BOOL {
    console_ctrl(ctrl, &CTRL_C).into()
}

/// Installs (once per process) a console control handler that turns the first Ctrl+C or
/// Ctrl+Break into a stop request instead of ending the process ([`console_ctrl`]); a
/// press beyond the allowance ends it as usual. Returns the press counter; poll it.
pub fn install_ctrl_c_handler() -> Result<&'static CtrlC, CaptureError> {
    static INSTALLED: Mutex<bool> = Mutex::new(false);
    let mut installed = INSTALLED.lock().unwrap_or_else(|p| p.into_inner());
    if !*installed {
        // SAFETY: `on_console_ctrl` is a plain function that lives for the whole process
        // and only reads and writes static atomics; registering it once is guarded by
        // `INSTALLED`.
        unsafe { SetConsoleCtrlHandler(Some(on_console_ctrl), true) }
            .ctx("SetConsoleCtrlHandler")?;
        *installed = true;
    }
    Ok(&CTRL_C)
}

/// Asks Windows not to put the computer to sleep while it is held (T9.16): idle Modern Standby
/// stops capture and the speech in that time is lost. The screen may still turn off. A closed lid
/// or the power button still sleeps; the runtime reports the gap that leaves. Dropping it releases
/// the request, and Windows also releases it if the process ends.
pub struct KeepAwake(HANDLE);

impl KeepAwake {
    /// Takes the power request, shown by `powercfg /requests` with a fixed reason.
    ///
    /// # Errors
    /// [`CaptureError::Windows`] if Windows refuses. A recording never depends on it.
    pub fn hold() -> Result<Self, CaptureError> {
        let reason = REASON_CONTEXT {
            Version: POWER_REQUEST_CONTEXT_VERSION,
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(w!("Earshot is recording a meeting").as_ptr().cast_mut()),
            },
        };
        // SAFETY: `reason` is complete and alive for the call, and its string is a static that
        // Windows only reads; the result is a handle this value owns.
        let handle = unsafe { PowerCreateRequest(&raw const reason) }.ctx("PowerCreateRequest")?;
        let held = Self(handle);
        // SAFETY: `held.0` is the open power request just created.
        unsafe { PowerSetRequest(held.0, PowerRequestSystemRequired) }.ctx("PowerSetRequest")?;
        Ok(held)
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: `self.0` is an open power request owned by this value, closed only here.
        // Closing also ends a request still set, so a failed clear leaves nothing behind.
        unsafe {
            let _ = PowerClearRequest(self.0, PowerRequestSystemRequired);
            let _ = CloseHandle(self.0);
        }
    }
}

/// `POWER_REQUEST_CONTEXT_VERSION`, not generated by `windows` 0.62 under the features used here.
const POWER_REQUEST_CONTEXT_VERSION: u32 = 0;

/// Windows' below-normal thread priority (`THREAD_PRIORITY_BELOW_NORMAL`), as
/// [`current_thread_priority`] reports it.
pub const BELOW_NORMAL_PRIORITY: i32 = THREAD_PRIORITY_BELOW_NORMAL.0;

/// Lowers the calling thread to below-normal priority, so it runs only on CPU time that
/// normal-priority threads (live transcription, capture, other applications) leave
/// unused. Used for background work such as the live speaker windows.
///
/// # Errors
/// [`CaptureError::Windows`] if Windows refuses the change.
pub fn lower_current_thread_priority() -> Result<(), CaptureError> {
    // SAFETY: `GetCurrentThread` returns a pseudo-handle for the calling thread that needs
    // no closing and stays valid for the call; `SetThreadPriority` only changes that
    // thread's scheduling priority.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) }
        .ctx("SetThreadPriority")
}

/// What `GetThreadPriority` returns on failure (`THREAD_PRIORITY_ERROR_RETURN`).
const THREAD_PRIORITY_ERROR_RETURN: i32 = 0x7FFF_FFFF;

/// The calling thread's priority (`GetThreadPriority`: 0 is normal,
/// [`BELOW_NORMAL_PRIORITY`] below normal).
///
/// # Errors
/// [`CaptureError::Windows`] if Windows cannot report it.
pub fn current_thread_priority() -> Result<i32, CaptureError> {
    // SAFETY: the calling thread's pseudo-handle, as in `lower_current_thread_priority`;
    // `GetThreadPriority` only reads its priority.
    let priority = unsafe { GetThreadPriority(GetCurrentThread()) };
    if priority == THREAD_PRIORITY_ERROR_RETURN {
        return Err(CaptureError::Windows {
            context: "GetThreadPriority",
            source: windows::core::Error::from_thread(),
        });
    }
    Ok(priority)
}

/// The machine's power supply now (`GetSystemPowerStatus`, classified by
/// [`crate::power::classify`]). The call reads state Windows keeps up to date and does not
/// block, so it is cheap enough to poll every few seconds.
///
/// # Errors
/// [`CaptureError::Windows`] if Windows cannot report it.
pub fn power_supply() -> Result<crate::power::PowerSupply, CaptureError> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` is a live, writable `SYSTEM_POWER_STATUS` owned by this frame for
    // the whole call; `GetSystemPowerStatus` only fills it in.
    unsafe { GetSystemPowerStatus(&raw mut status) }.ctx("GetSystemPowerStatus")?;
    Ok(crate::power::classify(
        status.ACLineStatus,
        status.BatteryFlag,
    ))
}

#[cfg(test)]
mod loop_tests;

#[cfg(test)]
mod device_tests {
    use earshot_core::capture::CaptureEvent;

    use super::*;

    #[test]
    fn live_stop_wait_outlasts_the_longest_wait_the_capture_loop_makes() {
        // T9.20: a stop landing in a process-loopback activation must not give up a thread
        // that is only waiting as designed.
        assert!(crate::STOP_WAIT > ACTIVATION_TIMEOUT + Duration::from_secs(1));
    }

    #[test]
    fn lowered_thread_runs_below_normal_and_other_threads_stay_normal() {
        let seen = std::thread::spawn(|| {
            let before = current_thread_priority().unwrap();
            lower_current_thread_priority().unwrap();
            (before, current_thread_priority().unwrap())
        })
        .join()
        .unwrap();
        assert_eq!(seen, (0, BELOW_NORMAL_PRIORITY));
        assert_eq!(BELOW_NORMAL_PRIORITY, -1);
        // Only the calling thread changed.
        assert_eq!(
            std::thread::spawn(current_thread_priority)
                .join()
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[test]
    fn power_supply_is_reported_by_windows() {
        // Whatever this machine is, Windows answers; whether it has a battery does not
        // change between two calls (the cable may move, a battery does not appear).
        let no_battery = |p| p == crate::power::PowerSupply::NoBattery;
        let first = power_supply().unwrap();
        let again = power_supply().unwrap();
        assert_eq!(
            no_battery(first),
            no_battery(again),
            "{first:?} then {again:?}"
        );
    }

    #[test]
    fn qpc_now_is_monotonic() {
        let a = qpc_now().unwrap();
        let b = qpc_now().unwrap();
        assert!(b >= a);
    }

    #[test]
    fn ctrl_c_first_press_requests_a_stop_and_a_repeat_ends_the_process() {
        let state = CtrlC::new();
        // Not a stop key: left to the default handler, nothing counted.
        for other in [2, 5, 6] {
            assert!(!console_ctrl(other, &state), "event {other}");
        }
        assert_eq!(state.presses(), 0);
        // First Ctrl+C: handled, stop requested.
        assert!(console_ctrl(CTRL_C_EVENT, &state));
        assert_eq!(state.presses(), 1);
        // A repeat (Ctrl+C or Ctrl+Break): not handled, so the process ends.
        assert!(!console_ctrl(CTRL_C_EVENT, &state));
        assert!(!console_ctrl(CTRL_BREAK_EVENT, &state));
        assert_eq!(state.presses(), 1);
        // Ctrl+Break alone also requests a stop.
        let state = CtrlC::new();
        assert!(console_ctrl(CTRL_BREAK_EVENT, &state));
        assert_eq!(state.presses(), 1);
    }

    #[test]
    fn ctrl_c_one_more_press_can_be_allowed_and_then_the_process_ends() {
        // T6.18: after the first press stopped the recording, the CLI allows one more to
        // cancel the cleanup; a press after that still ends the process.
        let state = CtrlC::new();
        assert!(console_ctrl(CTRL_C_EVENT, &state));
        state.allow_one_more();
        assert!(console_ctrl(CTRL_C_EVENT, &state));
        assert_eq!(state.presses(), 2);
        assert!(!console_ctrl(CTRL_C_EVENT, &state));
        // Allowing before any press still lets only one more through.
        let state = CtrlC::new();
        state.allow_one_more();
        assert!(console_ctrl(CTRL_C_EVENT, &state));
        assert!(!console_ctrl(CTRL_C_EVENT, &state));
    }

    #[test]
    fn com_apartment_inits_and_tears_down_twice() {
        drop(ComApartment::init_mta().unwrap());
        drop(ComApartment::init_mta().unwrap());
    }

    /// Windows has no call that reads a request back without elevation, so this proves the calls
    /// are accepted, repeatedly and side by side; the real proof is a run past the idle timeout.
    #[test]
    fn keep_awake_can_be_held_released_and_held_again() {
        let first = KeepAwake::hold().unwrap();
        let second = KeepAwake::hold().unwrap();
        drop(first);
        drop(second);
        drop(KeepAwake::hold().unwrap());
    }

    #[test]
    fn maps_flow_and_role() {
        assert_eq!(map_flow(eRender), Some(Flow::Render));
        assert_eq!(map_flow(eCapture), Some(Flow::Capture));
        assert_eq!(map_role(eConsole), Some(Role::Console));
        assert_eq!(map_role(eMultimedia), Some(Role::Multimedia));
        assert_eq!(map_role(eCommunications), Some(Role::Communications));
    }

    #[test]
    fn own_process_image_name_is_found() {
        let name = process_image_name(std::process::id()).unwrap();
        assert!(name.to_ascii_lowercase().ends_with(".exe"), "{name}");
    }

    #[test]
    fn device_loss_errors_are_classified() {
        let lost = CaptureError::Windows {
            context: "x",
            source: windows::core::Error::from_hresult(HRESULT(
                follow::AUDCLNT_E_DEVICE_INVALIDATED,
            )),
        };
        assert!(lost.is_device_loss());
        let other = CaptureError::Windows {
            context: "x",
            source: windows::core::Error::from_hresult(HRESULT(0x8007_0057_u32 as i32)),
        };
        assert!(!other.is_device_loss());
        assert!(!CaptureError::ActivationTimeout.is_device_loss());
    }

    #[test]
    #[ignore = "needs a real capture and render device"]
    fn opens_default_endpoints() {
        let com = ComApartment::init_mta().unwrap();
        for ep in [
            Endpoint::Microphone(MicRole::Console),
            Endpoint::Microphone(MicRole::Communications),
            Endpoint::Loopback,
        ] {
            let s = CaptureStream::open_default(&com, ep).unwrap();
            assert!(s.format().sample_rate > 0, "{ep:?}");
            assert!(!s.device_name().is_empty(), "{ep:?}");
            assert!(s.device_id().is_some(), "{ep:?}");
        }
    }

    #[test]
    fn raw_on_a_loopback_is_refused_before_any_device_is_opened() {
        let com = ComApartment::init_mta().unwrap();
        for ep in [
            Endpoint::Loopback,
            // Never a valid pid: refused before the process is looked at.
            Endpoint::ProcessLoopback { pid: 0xFFFF_FFF1 },
        ] {
            let refused = CaptureStream::open_with(&com, ep, None, None, None, MicProcessing::Raw)
                .err()
                .expect("raw is the mic's alone");
            assert!(
                matches!(refused, CaptureError::RawMic(RawRefusal::NotAMicrophone)),
                "{ep:?}: {refused}"
            );
        }
    }

    #[test]
    #[ignore = "opens the default microphone in raw mode: cargo test -p earshot-capture-wasapi raw_default_mic_opens_raw_or_is_refused_with_the_reason -- --ignored"]
    fn raw_default_mic_opens_raw_or_is_refused_with_the_reason() {
        let com = ComApartment::init_mta().unwrap();
        let mic = Endpoint::Microphone(MicRole::Console);
        // What the endpoint says, read from an ordinary open.
        let says = CaptureStream::open_default(&com, mic)
            .unwrap()
            .endpoint_properties()
            .map(crate::endpoint::raw_support);
        let raw = CaptureStream::open_with(&com, mic, None, None, None, MicProcessing::Raw);
        eprintln!("raw support: {says:?}");
        match says {
            Some(crate::endpoint::RawSupport::Supported) => {
                let s = raw.unwrap();
                assert!(s.format().sample_rate > 0);
                assert_eq!(s.track_channels(), 1);
                eprintln!("raw mic {}: {:?}", s.device_name(), s.format());
            }
            _ => {
                let refused = raw.err().expect("no raw support, so no stream");
                assert!(matches!(refused, CaptureError::RawMic(_)), "{refused}");
                eprintln!("refused: {refused}");
            }
        }
    }

    #[test]
    #[ignore = "needs a real capture device"]
    fn reopens_mic_at_a_requested_rate() {
        let com = ComApartment::init_mta().unwrap();
        let ep = Endpoint::Microphone(MicRole::Console);
        let native = CaptureStream::open_default(&com, ep)
            .unwrap()
            .format()
            .sample_rate;
        let other = if native == 44_100 { 48_000 } else { 44_100 };
        let s = CaptureStream::open(&com, ep, Some(other), None).unwrap();
        assert_eq!(s.format().sample_rate, other);
        assert_eq!(s.device_format().sample_rate, native);
        assert_eq!(s.track_channels(), 1, "the mic is downmixed");
    }

    #[test]
    fn endpoints_choose_mono_mic_and_multichannel_loopback() {
        assert_eq!(
            Endpoint::Microphone(MicRole::Console).channel_mode(),
            ChannelMode::Mono
        );
        assert_eq!(Endpoint::Loopback.channel_mode(), ChannelMode::Interleaved);
        assert_eq!(
            Endpoint::ProcessLoopback { pid: 4 }.channel_mode(),
            ChannelMode::Interleaved
        );
    }

    #[test]
    #[ignore = "needs a real render device"]
    fn loopback_keeps_its_channels_and_reopens_with_them() {
        let com = ComApartment::init_mta().unwrap();
        let s = CaptureStream::open_default(&com, Endpoint::Loopback).unwrap();
        let native = s.format();
        assert_eq!(s.track_channels(), native.channels, "no downmix");
        drop(s);
        // A re-open asks for the track's rate and channels; the engine converts.
        let other = if native.sample_rate == 44_100 {
            48_000
        } else {
            44_100
        };
        let s = CaptureStream::open(&com, Endpoint::Loopback, Some(other), Some(native.channels))
            .unwrap();
        assert_eq!(
            (s.format().sample_rate, s.track_channels()),
            (other, native.channels)
        );
        let s = CaptureStream::open(&com, Endpoint::Loopback, None, Some(1)).unwrap();
        assert_eq!((s.format().channels, s.track_channels()), (1, 1));
        eprintln!(
            "loopback {}: {} Hz, {} ch",
            s.device_name(),
            native.sample_rate,
            native.channels
        );
    }

    #[test]
    #[ignore = "needs a real render device; plays silence only"]
    fn render_plays_silence_on_the_default_output_and_stops() {
        // T7.4j: open, play 200 ms of zeros (inaudible), stop; then a stop request ends a
        // long signal at once. A chosen id that is not an output is refused.
        let com = ComApartment::init_mta().unwrap();
        let mut r = RenderStream::open(&com, None).unwrap();
        assert!(r.rate() >= 8_000, "{}", r.rate());
        let id = r.device_id().map(str::to_owned);
        let silence = vec![0.0f32; usize::try_from(r.rate() / 5).unwrap()];
        let started = std::time::Instant::now();
        r.play(&silence, &AtomicBool::new(false)).unwrap();
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(150), "{took:?}");
        assert!(took < Duration::from_secs(2), "{took:?}");
        let long = vec![0.0f32; usize::try_from(r.rate() * 10).unwrap()];
        let started = std::time::Instant::now();
        r.play(&long, &AtomicBool::new(true)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        if let Some(id) = &id {
            let again = RenderStream::open(&com, Some(id)).unwrap();
            assert_eq!(again.device_id(), Some(id.as_str()));
        }
        assert!(matches!(
            RenderStream::open(&com, Some("{0.0.0.00000000}.{no-such-device}")),
            Err(CaptureError::DeviceGone { .. })
        ));
        eprintln!("render {}: {} Hz", r.device_name(), r.rate());
    }

    #[test]
    fn win32_propvariant_clears_on_drop() {
        // The ActivationArgs and friendly_name SAFETY notes rely on this: windows-rs
        // implements Drop (PropVariantClear) for the Win32 PROPVARIANT.
        assert!(std::mem::needs_drop::<PROPVARIANT>());
    }

    #[test]
    fn activation_args_free_without_clearing_the_blob() {
        // Dropping must free both boxes without PropVariantClear touching the blob
        // (a Rust allocation): a clear would corrupt the heap and abort the test.
        let args = ActivationArgs::new(AUDIOCLIENT_ACTIVATION_PARAMS::default());
        // SAFETY: `prop` is alive until `args` is dropped below.
        let vt = unsafe { (&*args.prop).vt() };
        assert_eq!(vt, VT_BLOB);
        drop(args);
    }

    /// Never a valid pid: Windows pids are multiples of 4.
    const NO_SUCH_PID: u32 = 0xFFFF_FFF1;

    #[test]
    fn missing_process_has_no_image_name() {
        assert!(process_image_name(NO_SUCH_PID).is_err());
    }

    #[test]
    #[ignore = "needs the audio service"]
    fn explicit_pid_of_missing_process_is_a_selection_error() {
        let com = ComApartment::init_mta().unwrap();
        let r = open_loopback(
            &com,
            &LoopbackRequest::Pid(NO_SUCH_PID),
            &DeviceChoice::Default,
        );
        assert!(
            matches!(
                r,
                Err(CaptureError::Selection(SelectError::NoSuchProcess(
                    NO_SUCH_PID
                )))
            ),
            "{:?}",
            r.err()
        );
    }

    #[test]
    #[ignore = "needs Windows 10 2004+ and a render device; plays a short system sound"]
    fn process_loopback_of_own_process_delivers_valid_timestamps() {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
        use windows::core::w;

        let com = ComApartment::init_mta().unwrap();
        let pid = std::process::id();
        let mut s =
            CaptureStream::open(&com, Endpoint::ProcessLoopback { pid }, None, None).unwrap();
        assert_eq!(s.format().sample_rate, PROCESS_LOOPBACK_RATE);
        assert_eq!(s.format().channels, PROCESS_LOOPBACK_CHANNELS);
        assert_eq!(s.track_channels(), 2, "process loopback is kept stereo");
        assert!(s.device_name().contains(&pid.to_string()));
        s.start().unwrap();
        // SAFETY: static NUL-terminated path; asynchronous playback from this process.
        let played = unsafe {
            PlaySoundW(
                w!(r"C:\Windows\Media\chimes.wav"),
                None,
                SND_FILENAME | SND_ASYNC | SND_NODEFAULT,
            )
        };
        assert!(played.as_bool(), "PlaySoundW failed");
        let mut packets = Vec::new();
        let mut peak = 0f32;
        let until = qpc_now().unwrap() + 2 * QPC_UNITS_PER_SEC;
        while qpc_now().unwrap() < until {
            wait_any(&[s.event.0], Duration::from_millis(20)).unwrap();
            s.drain(|p, x| {
                assert!(p.silent || x.len() == 2 * p.frames as usize, "{p:?}");
                packets.push((p, qpc_now().unwrap()));
                peak = x.iter().fold(peak, |m, v| m.max(v.abs()));
            })
            .unwrap();
        }
        // SAFETY: a null sound name stops any asynchronous playback of this process.
        let _ = unsafe { PlaySoundW(PCWSTR::null(), None, SND_ASYNC) };
        assert!(!packets.is_empty(), "no packets from process loopback");
        assert!(peak > 0.0, "process loopback delivered only silence");
        for (p, now) in &packets {
            assert_ne!(p.qpc, 0, "{p:?}");
            assert!(!p.timestamp_error, "{p:?}");
            // A packet's first frame is at most one buffer plus a wait in the past.
            assert!(
                *now >= p.qpc && now - p.qpc < QPC_UNITS_PER_SEC,
                "{p:?} now {now}"
            );
        }
    }

    /// Bounds a wait that only a failure runs into.
    const POLLER_GUARD: Duration = Duration::from_secs(60);

    #[test]
    fn call_sessions_poller_drop_never_waits_for_a_reading_that_hangs() {
        let (entered_tx, entered) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        // Goes with the thread, so its end shows when the thread has gone.
        let (alive, gone) = mpsc::channel::<()>();
        let poller = RenderSessionPoller::start_with(Duration::from_secs(3_600), move || {
            let _alive = alive;
            move || {
                let _ = &_alive;
                let _ = entered_tx.send(());
                // A COM call that does not come back until the test says so.
                let _ = released.recv();
                Ok(Vec::new())
            }
        })
        .unwrap();
        entered
            .recv_timeout(POLLER_GUARD)
            .expect("no reading began");

        // On another thread, so a drop that waits fails this test instead of hanging it.
        let (dropped_tx, dropped) = mpsc::channel();
        std::thread::spawn(move || {
            drop(poller);
            let _ = dropped_tx.send(());
        });
        dropped
            .recv_timeout(POLLER_GUARD)
            .expect("the drop waited for the hung reading");

        // Once the call returns, the thread ends by itself.
        release.send(()).unwrap();
        assert_eq!(
            gone.recv_timeout(POLLER_GUARD),
            Err(mpsc::RecvTimeoutError::Disconnected),
            "the thread outlived its reading"
        );
    }

    #[test]
    fn call_sessions_poller_hands_over_the_newest_reading() {
        let mut n = 0;
        let mut poller = RenderSessionPoller::start_with(Duration::from_millis(1), move || {
            move || {
                n += 1;
                if n == 1 {
                    Ok(Vec::new())
                } else {
                    Err(format!("reading {n}"))
                }
            }
        })
        .unwrap();
        let deadline = std::time::Instant::now() + POLLER_GUARD;
        loop {
            assert!(std::time::Instant::now() < deadline, "no second reading");
            if let Some(Err(e)) = poller.latest() {
                assert!(e.starts_with("reading "), "{e}");
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Counts what a track reports.
    #[derive(Default)]
    struct CountingTrack {
        reopened: Vec<ReopenReason>,
    }

    impl TrackObserver for CountingTrack {
        fn packet(&mut self, _: PacketInfo, _: &[f32]) {}

        fn reopened(&mut self, _: u64, reason: ReopenReason, _: ReopenOutcome<'_>) {
            self.reopened.push(reason);
        }
    }

    #[test]
    #[ignore = "needs a render device; opens its loopback, plays nothing"]
    fn call_output_a_move_that_cannot_open_leaves_the_stream_recording() {
        let com = ComApartment::init_mta().unwrap();
        let notifications = DeviceNotifications::register(&com).unwrap();
        let mut driver = Wasapi {
            com: &com,
            notifications: &notifications,
        };
        let stream = CaptureStream::open(&com, Endpoint::Loopback, None, None).unwrap();
        stream.start().unwrap();
        let on = stream.device_id().map(str::to_owned).unwrap();
        let mut track = Track::new(
            stream,
            Endpoint::Loopback,
            DeviceChoice::Default,
            StreamFollower::new(Flow::Render, Role::Console, true),
            CountingTrack::default(),
        );

        let gone = CallOutput {
            id: "{no-such-output}".into(),
            name: "Gone".into(),
        };
        assert!(track.move_output(&mut driver, &gone, 1).is_err());
        assert!(track.stream.is_some(), "still recording");
        assert_eq!(track.follower.current_id(), Some(on.as_str()));
        assert!(!track.follower.is_lost());
        assert_eq!(track.follower.take_reopen(2), None);
        assert_eq!(track.device, DeviceChoice::Default);
        assert!(track.observer.reopened.is_empty(), "nothing happened");

        // The move itself, onto the output it is on, which is the default: it follows the default.
        let here = CallOutput {
            id: on.clone(),
            name: "Here".into(),
        };
        track.move_output(&mut driver, &here, 3).unwrap();
        assert!(track.stream.is_some());
        assert_eq!(track.follower.current_id(), Some(on.as_str()));
        assert!(track.follower.follows_default());
        assert_eq!(track.device, DeviceChoice::Default);
        assert_eq!(track.observer.reopened, [ReopenReason::CallOutputMoved]);
    }

    #[test]
    #[ignore = "needs a render device"]
    fn lists_render_sessions_and_registers_notifications() {
        let com = ComApartment::init_mta().unwrap();
        let sessions = list_render_sessions(&com).unwrap();
        assert!(sessions.iter().all(|s| s.pid != 0));
        let n = DeviceNotifications::register(&com).unwrap();
        assert!(n.take().is_empty());
    }

    #[test]
    #[ignore = "needs a real capture and render device; plays a short system sound"]
    fn records_one_second_of_both_tracks() {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
        use windows::core::w;

        let mut played = false;
        let r = record_raw(
            1.0,
            MicRole::Console,
            MicProcessing::Default,
            &LoopbackRequest::DefaultDevice,
            |_, _, _| {
                // Render something so the default-device loopback delivers packets (it
                // delivers none while nothing renders).
                // SAFETY: static NUL-terminated path; asynchronous playback from this
                // process.
                played = unsafe {
                    PlaySoundW(
                        w!(r"C:\Windows\Media\chimes.wav"),
                        None,
                        SND_FILENAME | SND_ASYNC | SND_NODEFAULT,
                    )
                }
                .as_bool();
            },
            |_| {},
        );
        // SAFETY: a null sound name stops any asynchronous playback of this process.
        let _ = unsafe { PlaySoundW(PCWSTR::null(), None, SND_ASYNC) };
        let r = r.unwrap();
        assert!(
            played,
            r"PlaySoundW could not play C:\Windows\Media\chimes.wav (missing file or no render device?)"
        );
        // The origin is taken just before Start, so no track records its own start: the
        // first packet sits within a few ms of the origin. A started stream may still
        // hand over frames captured just before Start (measured: 3.2 ms on a USB mic);
        // those are trimmed, and a later first packet is preceded by silence.
        for t in [&r.mic, &r.loopback] {
            let packets = t.timeline.packets();
            assert!(!packets.is_empty(), "{}: no packets", t.device_name);
            let first = &packets[0];
            assert!(!first.timestamp_error, "{}: {first:?}", t.device_name);
            assert_eq!(first.track_frame, first.silence_before, "{}", t.device_name);
            assert!(
                first.trimmed_frames == 0 || first.silence_before == 0,
                "{}: {first:?}",
                t.device_name
            );
            let rate = u64::from(t.sample_rate());
            let early_ms = first.trimmed_frames * 1000 / rate;
            let late_ms = first.silence_before * 1000 / rate;
            // The loopback's first packet waits for the chime to start rendering (about
            // 30 ms warm, more on a cold audio engine), so it gets a looser bound: the
            // chime must have started rendering well within the 1 s recording.
            let late_limit_ms = if std::ptr::eq(t, &r.loopback) {
                500
            } else {
                50
            };
            assert!(
                early_ms < 50 && late_ms < late_limit_ms,
                "{}: first packet {early_ms} ms early, {late_ms} ms late",
                t.device_name
            );
            eprintln!(
                "{}: {} packets, first {early_ms} ms early, {late_ms} ms late",
                t.device_name,
                packets.len()
            );
        }
        assert_eq!(
            r.loopback_source.mode,
            crate::recording::LoopbackMode::DefaultDevice
        );
        assert_eq!(r.mic.channels(), 1, "the mic is downmixed");
        assert_eq!(
            r.loopback.channels(),
            r.loopback.format.channels,
            "the loopback keeps its device channels"
        );
        for t in [&r.mic, &r.loopback] {
            let secs = t.timeline.frames() as f64 / f64::from(t.sample_rate());
            assert!((0.9..1.3).contains(&secs), "{}: {secs} s", t.device_name);
            assert!(t.device_changes.is_empty(), "{}", t.device_name);
            assert_eq!(
                t.samples().len(),
                t.timeline.frames() as usize * usize::from(t.channels())
            );
        }
    }

    #[test]
    fn wasapi_source_can_move_to_a_consumer_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<WasapiSource>();
    }

    /// Pumps `src` like a consumer thread for `seconds`, stops it and pumps until the
    /// end; returns every event. Checks that pump never blocks for long.
    fn run_live(src: &mut WasapiSource, seconds: f64) -> Vec<CaptureEvent> {
        let mut out = Vec::new();
        let until = std::time::Instant::now() + Duration::from_secs_f64(seconds);
        while std::time::Instant::now() < until {
            let t = std::time::Instant::now();
            let status = src.pump(&mut out).unwrap();
            assert!(t.elapsed() < Duration::from_millis(50), "pump blocked");
            assert_ne!(status, SourceStatus::Ended, "ended before stop");
            std::thread::sleep(Duration::from_millis(10));
        }
        src.stop();
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        assert_eq!(src.pump(&mut out).unwrap(), SourceStatus::Ended);
        out
    }

    /// The core contract on real devices: per-track timestamps monotonic (strictly
    /// increasing frame starts, none before the origin), markers on known tracks, the
    /// end exactly once and last, with `Stopped` at or after every frame.
    /// Returns the frames delivered per track (mic, reference).
    fn check_live_events(session: &CaptureSession, out: &[CaptureEvent]) -> [u64; 2] {
        let ends: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                CaptureEvent::End(end) => Some(end.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ends.len(), 1, "end exactly once");
        assert!(matches!(out.last(), Some(CaptureEvent::End(_))), "end last");
        let end = &ends[0];
        assert_eq!(end.reason, EndReason::Stopped);
        let mut last: [Option<DeviceTime>; 2] = [None, None];
        let mut frames = [0u64; 2];
        for e in out {
            match e {
                CaptureEvent::Frames {
                    track,
                    device_ts,
                    frames: f,
                } => {
                    let i = usize::from(*track == TrackId::Reference);
                    assert!(*device_ts >= session.origin, "{track:?} before origin");
                    assert!(*device_ts <= end.at, "{track:?} after the end");
                    if let Some(prev) = last[i] {
                        assert!(*device_ts > prev, "{track:?} went backwards");
                    }
                    last[i] = Some(*device_ts);
                    // Interleaved: whole frames of the track's channel count.
                    let Some(info) = session.track(*track) else {
                        panic!("{track:?} is not in the session");
                    };
                    let ch = usize::from(info.channels.get());
                    assert!(f.len().is_multiple_of(ch), "{track:?} partial frame");
                    frames[i] += (f.len() / ch) as u64;
                    assert!(f.iter().all(|x| x.is_finite()));
                }
                CaptureEvent::Discontinuity { track, .. } => {
                    assert!(session.track(*track).is_some());
                    // A marker may legally reset the monotonic order.
                    last[usize::from(*track == TrackId::Reference)] = None;
                }
                CaptureEvent::End(_) => {}
            }
        }
        let mic_secs = frames[0] as f64 / f64::from(session.mic.rate.hz());
        assert!(
            (1.5..2.6).contains(&mic_secs),
            "mic delivered {mic_secs} s in a 2 s run"
        );
        eprintln!(
            "mic {} frames ({mic_secs:.3} s), reference {} frames, end at +{:?}",
            frames[0],
            frames[1],
            session.meeting_time(end.at)
        );
        frames
    }

    #[test]
    #[ignore = "needs a real capture and render device; plays a short system sound"]
    fn live_source_runs_two_seconds_on_real_devices() {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
        use windows::core::w;

        let mut src = WasapiSource::start(
            MicRole::Console,
            &CaptureMode::Call(LoopbackRequest::DefaultDevice),
        )
        .unwrap();
        let session = src.session().clone();
        assert!(session.reference.is_some());
        assert_eq!(
            session.mic.rate.hz(),
            src.mic_device().format.sample_rate,
            "native rate, no resampling"
        );
        assert_eq!(
            session.mic.device_name.as_deref(),
            Some(src.mic_device().name.as_str())
        );
        assert_eq!(session.mic.channels.get(), 1, "the mic is downmixed");
        assert_eq!(
            session.reference.as_ref().map(|r| r.channels.get()),
            src.loopback_device().map(|d| d.format.channels),
            "the reference keeps the loopback's channels"
        );
        assert_eq!(
            src.loopback_source().map(|s| s.mode),
            Some(crate::recording::LoopbackMode::DefaultDevice)
        );
        // Something must render for the loopback track to deliver packets.
        // SAFETY: static NUL-terminated path; asynchronous playback from this process.
        let played = unsafe {
            PlaySoundW(
                w!(r"C:\Windows\Media\chimes.wav"),
                None,
                SND_FILENAME | SND_ASYNC | SND_NODEFAULT,
            )
        };
        assert!(played.as_bool(), "PlaySoundW failed");
        let out = run_live(&mut src, 2.0);
        // SAFETY: a null sound name stops any asynchronous playback of this process.
        let _ = unsafe { PlaySoundW(PCWSTR::null(), None, SND_ASYNC) };
        let frames = check_live_events(&session, &out);
        assert!(
            frames[1] > 0,
            "loopback delivered no frames while a sound played"
        );
    }

    #[test]
    #[ignore = "needs a real capture and render device"]
    fn live_source_with_auto_loopback_runs_two_seconds() {
        let mut src =
            WasapiSource::start(MicRole::Console, &CaptureMode::Call(LoopbackRequest::Auto))
                .unwrap();
        let session = src.session().clone();
        let out = run_live(&mut src, 2.0);
        check_live_events(&session, &out);
    }

    /// Every active capture endpoint with its properties.
    fn capture_endpoints(com: &ComApartment) -> Vec<crate::endpoint::EndpointProperties> {
        let e = device_enumerator(com).unwrap();
        // SAFETY: plain COM calls on live interfaces; every result is owned.
        let coll = unsafe { e.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) }.unwrap();
        // SAFETY: as above.
        let n = unsafe { coll.GetCount() }.unwrap();
        (0..n)
            .map(|i| {
                // SAFETY: `i` is below the collection's count.
                let d = unsafe { coll.Item(i) }.unwrap();
                let name = friendly_name(&d).unwrap();
                // SAFETY: COM is initialised; the client is an owned interface.
                let client: IAudioClient = unsafe { d.Activate(CLSCTX_ALL, None) }.unwrap();
                let (mix, _) = mix_format(&client).unwrap();
                endpoint_properties(&d, &name, Some(mix.sample_rate))
            })
            .collect()
    }

    #[test]
    #[ignore = "needs a real capture device"]
    fn endpoint_properties_of_the_default_mic_are_read_and_classified() {
        let com = ComApartment::init_mta().unwrap();
        let s = CaptureStream::open_default(&com, Endpoint::Microphone(MicRole::Console)).unwrap();
        let p = s
            .endpoint_properties()
            .expect("a device endpoint has properties");
        eprintln!("{p:?} -> {:?}", crate::endpoint::classify_mic(p));
        assert_eq!(p.name, s.device_name());
        assert_eq!(p.sample_rate, Some(s.device_format().sample_rate));
        assert!(
            p.enumerator.is_some(),
            "every endpoint names its enumerator"
        );
        assert!(p.form_factor.is_some(), "every endpoint has a form factor");
        assert!(p.filter_path.is_some(), "every endpoint has a filter path");
        let lb = CaptureStream::open_default(&com, Endpoint::Loopback).unwrap();
        assert!(lb.endpoint_properties().is_some());
        // Mics like these (USB interface, webcam, virtual) are none of them Bluetooth.
        for p in capture_endpoints(&com) {
            eprintln!("{p:?} -> {:?}", crate::endpoint::classify_mic(&p));
        }
    }

    #[test]
    #[ignore = "needs a paired Bluetooth headset (none on the desktop): connect one, then run \
                cargo test -p earshot-capture-wasapi --lib bluetooth_headset -- --ignored"]
    fn endpoint_bluetooth_headset_mic_is_classified_hands_free() {
        let com = ComApartment::init_mta().unwrap();
        let all = capture_endpoints(&com);
        let bluetooth: Vec<_> = all.iter().filter(|p| p.is_bluetooth()).collect();
        assert!(
            !bluetooth.is_empty(),
            "no Bluetooth capture endpoint is active: {all:#?}"
        );
        for p in bluetooth {
            eprintln!("{p:#?}");
            assert_eq!(
                crate::endpoint::classify_mic(p),
                crate::endpoint::MicKind::BluetoothHandsFree,
                "{p:?}"
            );
            assert!(crate::endpoint::hands_free_warning(p).is_some());
        }
    }

    #[test]
    #[ignore = "needs a real capture device"]
    fn live_source_in_the_room_captures_the_mic_only() {
        let mut src = WasapiSource::start(MicRole::Console, &CaptureMode::InRoom).unwrap();
        let session = src.session().clone();
        assert!(
            session.reference.is_none(),
            "no reference track in the room"
        );
        assert_eq!(session.tracks().collect::<Vec<_>>(), [TrackId::Mic]);
        assert!(src.loopback_device().is_none());
        assert!(src.loopback_source().is_none());
        assert_eq!(session.mic.channels.get(), 1, "the mic is downmixed");
        let out = run_live(&mut src, 2.0);
        let frames = check_live_events(&session, &out);
        assert_eq!(frames[1], 0, "nothing on a reference track");
        assert!(
            out.iter().all(|e| match e {
                CaptureEvent::Frames { track, .. } | CaptureEvent::Discontinuity { track, .. } =>
                    *track == TrackId::Mic,
                CaptureEvent::End(_) => true,
            }),
            "every event is on the mic"
        );
    }

    fn chosen(id: &str, name: &str) -> DeviceChoice {
        DeviceChoice::Endpoint {
            id: id.into(),
            name: name.into(),
        }
    }

    /// An id no endpoint has (the shape of a real one).
    const MISSING_ID: &str = "{0.0.1.00000000}.{00000000-0000-0000-0000-000000000000}";

    #[test]
    #[ignore = "GATE-01a: needs real audio devices (makes no sound); run with cargo test -p \
                earshot-capture-wasapi --lib devices_ -- --ignored"]
    fn devices_are_listed_with_the_default_marked_once() {
        let list = list_audio_devices().unwrap();
        for d in list.microphones.iter().chain(&list.outputs) {
            eprintln!(
                "{} {:?} default={} rate={:?} kind={:?}",
                d.id,
                d.name,
                d.is_default,
                d.properties.sample_rate,
                d.mic_kind()
            );
        }
        for (what, devices) in [("mic", &list.microphones), ("output", &list.outputs)] {
            assert!(!devices.is_empty(), "no active {what}");
            assert_eq!(
                devices.iter().filter(|d| d.is_default).count(),
                1,
                "exactly one default {what}"
            );
            let mut ids: Vec<&str> = devices.iter().map(|d| d.id.as_str()).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), devices.len(), "{what} ids are unique");
            for d in devices {
                assert!(!d.name.is_empty());
                assert_eq!(d.properties.id.as_deref(), Some(d.id.as_str()));
                assert!(d.properties.sample_rate.is_some(), "{d:?}");
            }
        }
        // The listed defaults are what a default stream opens.
        let com = ComApartment::init_mta().unwrap();
        let mic =
            CaptureStream::open_default(&com, Endpoint::Microphone(MicRole::Console)).unwrap();
        assert_eq!(
            mic.device_id(),
            list.default_microphone().map(|d| d.id.as_str())
        );
        let lb = CaptureStream::open_default(&com, Endpoint::Loopback).unwrap();
        assert_eq!(lb.device_id(), list.default_output().map(|d| d.id.as_str()));
        // Listing works from a thread that already holds COM.
        assert_eq!(
            list_devices(&com).unwrap().microphones.len(),
            list.microphones.len()
        );
    }

    #[test]
    #[ignore = "GATE-01a: needs real audio devices (makes no sound); run with cargo test -p \
                earshot-capture-wasapi --lib devices_ -- --ignored"]
    fn devices_chosen_endpoints_open_by_id_and_a_missing_one_is_gone() {
        let list = list_audio_devices().unwrap();
        let com = ComApartment::init_mta().unwrap();
        for d in &list.microphones {
            let s = CaptureStream::open_on(
                &com,
                Endpoint::Microphone(MicRole::Console),
                Some(&d.id),
                None,
                None,
            )
            .unwrap();
            assert_eq!(s.device_id(), Some(d.id.as_str()));
            assert_eq!(s.device_name(), d.name);
        }
        for d in &list.outputs {
            let s =
                CaptureStream::open_on(&com, Endpoint::Loopback, Some(&d.id), None, None).unwrap();
            assert_eq!(s.device_id(), Some(d.id.as_str()));
        }
        let gone = |endpoint, id: &str| {
            CaptureStream::open_on(&com, endpoint, Some(id), None, None)
                .err()
                .expect("not opened")
        };
        let e = gone(Endpoint::Microphone(MicRole::Console), MISSING_ID);
        assert!(e.is_device_gone(), "{e}");
        assert_eq!(e.to_string(), "the chosen microphone is not connected");
        let e = gone(Endpoint::Loopback, MISSING_ID);
        assert_eq!(e.to_string(), "the chosen output is not connected");
        // An output is not a microphone, and the other way round.
        let out = &list.outputs[0].id;
        assert!(gone(Endpoint::Microphone(MicRole::Console), out).is_device_gone());
        let mic = &list.microphones[0].id;
        assert!(gone(Endpoint::Loopback, mic).is_device_gone());
    }

    #[test]
    #[ignore = "GATE-01a: needs real audio devices (makes no sound); run with cargo test -p \
                earshot-capture-wasapi --lib devices_ -- --ignored"]
    fn devices_live_source_on_a_missing_mic_falls_back_to_the_default() {
        let list = list_audio_devices().unwrap();
        let default = list.default_microphone().unwrap().name.clone();
        let devices = DeviceSelection {
            microphone: chosen(MISSING_ID, "Old headset"),
            output: DeviceChoice::Default,
        };
        let mut src =
            WasapiSource::start_on(MicRole::Console, &CaptureMode::InRoom, &devices).unwrap();
        assert_eq!(src.mic_device().name, default);
        let [fallback] = src.fallbacks() else {
            panic!("one fallback: {:?}", src.fallbacks());
        };
        assert_eq!(fallback.track, TrackId::Mic);
        assert_eq!(fallback.chosen_name, "Old headset");
        assert_eq!(fallback.cause, crate::devices::FallbackCause::NotConnected);
        assert_eq!(fallback.used, default);
        let session = src.session().clone();
        let out = run_live(&mut src, 2.0);
        check_live_events(&session, &out);
    }

    #[test]
    #[ignore = "GATE-01a: needs real audio devices (makes no sound); run with cargo test -p \
                earshot-capture-wasapi --lib devices_ -- --ignored"]
    fn devices_live_source_records_the_chosen_endpoints() {
        let list = list_audio_devices().unwrap();
        // The last listed endpoints: not the default when there is more than one.
        let mic = list.microphones.last().unwrap();
        let output = list.outputs.last().unwrap();
        let devices = DeviceSelection {
            microphone: chosen(&mic.id, &mic.name),
            output: chosen(&output.id, &output.name),
        };
        let mut src = WasapiSource::start_on(
            MicRole::Console,
            &CaptureMode::Call(LoopbackRequest::DefaultDevice),
            &devices,
        )
        .unwrap();
        assert!(src.fallbacks().is_empty(), "{:?}", src.fallbacks());
        assert_eq!(src.mic_device().name, mic.name);
        assert_eq!(src.loopback_device().unwrap().name, output.name);
        let session = src.session().clone();
        let out = run_live(&mut src, 2.0);
        check_live_events(&session, &out);
    }

    #[test]
    #[ignore = "needs a real capture and render device"]
    fn live_source_drop_without_stop_joins_the_capture_thread() {
        let src = WasapiSource::start(
            MicRole::Console,
            &CaptureMode::Call(LoopbackRequest::DefaultDevice),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let t = std::time::Instant::now();
        drop(src);
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        // COM and the devices are free again: a second source starts.
        let mut again = WasapiSource::start(
            MicRole::Console,
            &CaptureMode::Call(LoopbackRequest::DefaultDevice),
        )
        .unwrap();
        again.stop();
        let mut out = Vec::new();
        assert_eq!(again.pump(&mut out).unwrap(), SourceStatus::Ended);
        assert!(matches!(out.last(), Some(CaptureEvent::End(_))));
    }

    #[test]
    #[ignore = "needs the audio service"]
    fn live_source_with_missing_process_fails_to_start() {
        let r = WasapiSource::start(
            MicRole::Console,
            &CaptureMode::Call(LoopbackRequest::Pid(NO_SUCH_PID)),
        );
        assert!(
            matches!(
                r,
                Err(CaptureError::Selection(SelectError::NoSuchProcess(
                    NO_SUCH_PID
                )))
            ),
            "{r:?}"
        );
    }

    #[test]
    #[ignore = "needs a real capture and render device"]
    fn auto_loopback_records_one_second() {
        let r = record_raw(
            1.0,
            MicRole::Console,
            MicProcessing::Default,
            &LoopbackRequest::Auto,
            |_, _, _| {},
            |_| {},
        )
        .unwrap();
        assert_eq!(r.loopback_source.requested, "auto");
        let t = &r.loopback;
        let secs = t.timeline.frames() as f64 / f64::from(t.sample_rate());
        assert!((0.9..1.3).contains(&secs), "{}: {secs} s", t.device_name);
        assert_eq!(t.channels(), t.format.channels, "{}", t.device_name);
    }
}
