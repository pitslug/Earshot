//! Windows microphone and loopback capture through WASAPI.
//!
//! Layout (T1.0, T6.3a, T6.3b, T6.3c):
//! - [`format`]: mix-format parsing and packet -> f32 conversion: mono downmix for the
//!   mic, interleaved device channels for the loopback reference (portable).
//! - [`timeline`]: QPC-timestamped packet placement with silence synthesis (portable).
//! - [`recording`]: a finished mic + loopback recording and its `timestamps.json` (portable).
//! - [`session`]: loopback source choice, i.e. call-app detection from the audio
//!   sessions on render endpoints (portable).
//! - [`endpoint`]: microphone endpoint classification: is the mic a Bluetooth hands-free
//!   headset (T6.24; portable).
//! - [`follow`]: default-device following and device-loss re-open decisions (portable).
//! - [`devices`]: the listed endpoints, the user's microphone and output choice, and the
//!   fallback to the default when a chosen endpoint is gone (T7.4i; portable).
//! - [`call_output`]: which output a call plays on, for following it (T6.15; portable).
//! - [`observer`]: the per-track packet and re-open callbacks of the capture loop,
//!   implemented by the recorder and the live source (portable).
//! - [`live`]: packets -> core capture events, and the channel-fed consumer side of the
//!   live `AudioSource` (portable).
//! - `wasapi` (Windows only): COM, endpoints, process loopback activation, audio session
//!   enumeration, device notifications, event-driven shared-mode capture streams, the
//!   shared capture loop, [`record_raw`] (mic + loopback for a fixed duration) and
//!   [`WasapiSource`] (the same capture as an `earshot_core::capture::AudioSource` on a
//!   dedicated capture thread).

pub mod call_output;
pub mod devices;
pub mod endpoint;
pub mod follow;
pub mod format;
pub mod live;
pub mod observer;
pub mod power;
pub mod problem;
pub mod recording;
pub mod session;
pub mod timeline;

#[cfg(windows)]
pub mod wasapi;

pub use call_output::{CALL_OUTPUT_AUDIBLE, CallOutput, CallOutputFollower, POLLS_TO_MOVE};
pub use devices::{
    AudioDevice, DeviceChoice, DeviceFallback, DeviceList, DeviceSelection, FallbackCause,
};
pub use endpoint::{
    EndpointProperties, HandsFreeFollower, HandsFreeMic, MicKind, classify_mic, hands_free_warning,
};
pub use follow::{DefaultDeviceChange, Flow, ReopenReason, Role, StreamFollower};
pub use format::{
    ChannelMode, FormatError, MixFormat, SampleFormat, append_interleaved_f32, append_mono_f32,
    check_delivered, conversion_target, float_waveformat_bytes,
};
pub use live::{
    ChannelSink, ChannelSource, EmitterTrack, LIVE_QUEUE_CAP, LiveError, LiveMsg, QueueGauge,
    TrackEmitter, queue_limit,
};
pub use observer::{ReopenOutcome, TrackObserver, reopen_is_reported};
pub use power::PowerSupply;
pub use problem::{StartProblem, start_problem};
pub use recording::{
    DeviceChange, DeviceDescription, HandsFreeChange, LoopbackMode, LoopbackSource, MicRole,
    NotifyingTrack, RawRecording, RecordingTrack, TimestampsFile, TrackRecording, TrackReport,
};
pub use session::{
    AudioSession, CALL_POLL_INTERVAL, CaptureMode, FallbackReason, LoopbackChoice, LoopbackRequest,
    ProcessTarget, SelectError, SessionPeak, call_app_active, choose_loopback,
};
pub use timeline::{PacketInfo, PacketRecord, QPC_UNITS_PER_SEC, TrackTimeline};

#[cfg(windows)]
pub use wasapi::{RenderSessionPoller, WasapiSource, list_audio_devices, record_raw};
