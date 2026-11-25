//! The capture loop ([`run_capture`]) driven through a fake platform (T9.21a): fake streams
//! that deliver one packet per drain, a fake clock that moves 20 ms per wait, and faults that
//! fail a chosen call with a chosen HRESULT. No audio device is opened.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use super::*;

/// 20 ms in QPC units: what one fake wait advances the clock by.
const WAIT_QPC: u64 = WAIT_MS as u64 * QPC_UNITS_PER_SEC / 1000;
const RATE: u32 = 48_000;

/// A call of the loop the fake can fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Call {
    Drain(TrackId),
    WaitAny,
    QpcNow,
    Open(TrackId),
}

/// Fails calls `first..=last` (counted from 1 per kind of call) of `call` with `hresult`.
#[derive(Clone, Copy, Debug)]
struct Fault {
    call: Call,
    first: usize,
    last: usize,
    hresult: i32,
}

impl Fault {
    /// Fails only the `nth` call.
    fn on(call: Call, nth: usize, hresult: i32) -> Self {
        Self {
            call,
            first: nth,
            last: nth,
            hresult,
        }
    }

    /// Fails the `nth` call and every one after it.
    fn from(call: Call, nth: usize, hresult: i32) -> Self {
        Self {
            last: usize::MAX,
            ..Self::on(call, nth, hresult)
        }
    }
}

/// What the fake platform shares with its streams.
#[derive(Default)]
struct Platform {
    clock: Cell<u64>,
    counts: RefCell<HashMap<Call, usize>>,
    faults: Vec<Fault>,
    changes: RefCell<Vec<DefaultDeviceChange>>,
    /// What every opened stream was asked for, in order (T12.32).
    opened_with: RefCell<Vec<(TrackId, MicProcessing)>>,
    /// The mic there is to open says it does not support raw processing.
    mic_without_raw: Cell<bool>,
    /// Every raw open fails later in the open with this call and HRESULT (T12.36: a format,
    /// `Initialize` or device failure after the raw request was accepted).
    raw_open_error: Cell<Option<(&'static str, i32)>>,
}

impl Platform {
    /// Counts one `call` and fails it when a fault says so.
    fn call(&self, call: Call) -> Result<(), CaptureError> {
        let mut counts = self.counts.borrow_mut();
        let n = counts.entry(call).or_default();
        *n += 1;
        match self
            .faults
            .iter()
            .find(|f| f.call == call && (f.first..=f.last).contains(n))
        {
            Some(f) => Err(hresult_error(f.hresult)),
            None => Ok(()),
        }
    }

    fn count(&self, call: Call) -> usize {
        self.counts.borrow().get(&call).copied().unwrap_or(0)
    }
}

fn hresult_error(hresult: i32) -> CaptureError {
    CaptureError::Windows {
        context: "fake",
        source: windows::core::Error::from_hresult(HRESULT(hresult)),
    }
}

fn track_of(endpoint: Endpoint) -> TrackId {
    match endpoint {
        Endpoint::Microphone(_) => TrackId::Mic,
        Endpoint::Loopback | Endpoint::ProcessLoopback { .. } => TrackId::Reference,
    }
}

/// A stream that, once started, delivers everything since its last packet on each drain.
struct FakeStream {
    platform: Rc<Platform>,
    track: TrackId,
    format: MixFormat,
    channels: u16,
    name: String,
    id: Option<String>,
    started: Cell<Option<u64>>,
    /// QPC end of the last delivered packet.
    delivered_to: Option<u64>,
}

impl FakeStream {
    fn new(platform: &Rc<Platform>, endpoint: Endpoint, channels: u16) -> Self {
        let track = track_of(endpoint);
        let format = MixFormat::from_waveformat_bytes(&float_waveformat_bytes(RATE, channels))
            .expect("a float format");
        let name = match track {
            TrackId::Mic => "Fake mic",
            TrackId::Reference => "Fake output",
        };
        Self {
            platform: Rc::clone(platform),
            track,
            format,
            channels: endpoint.channel_mode().track_channels(&format),
            name: name.into(),
            id: Some(name.to_lowercase().replace(' ', "-")),
            started: Cell::new(None),
            delivered_to: None,
        }
    }
}

impl LoopStream for FakeStream {
    fn event(&self) -> HANDLE {
        HANDLE::default()
    }

    fn start(&self) -> Result<(), CaptureError> {
        self.started.set(Some(self.platform.clock.get()));
        Ok(())
    }

    fn stop(&self) -> Result<(), CaptureError> {
        Ok(())
    }

    fn drain(&mut self, sink: &mut dyn FnMut(PacketInfo, &[f32])) -> Result<(), CaptureError> {
        self.platform.call(Call::Drain(self.track))?;
        let Some(started) = self.started.get() else {
            return Ok(());
        };
        let from = self.delivered_to.unwrap_or(started);
        let now = self.platform.clock.get();
        let frames = (now.saturating_sub(from) * u64::from(RATE) / QPC_UNITS_PER_SEC) as u32;
        if frames == 0 {
            return Ok(());
        }
        let samples = vec![0.25; frames as usize * usize::from(self.channels)];
        sink(
            PacketInfo {
                qpc: from,
                device_position: 0,
                frames,
                silent: false,
                discontinuity: false,
                timestamp_error: false,
            },
            &samples,
        );
        self.delivered_to = Some(from + u64::from(frames) * QPC_UNITS_PER_SEC / u64::from(RATE));
        Ok(())
    }

    fn format(&self) -> MixFormat {
        self.format
    }

    fn device_format(&self) -> MixFormat {
        self.format
    }

    fn device_name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    fn endpoint_properties(&self) -> Option<&EndpointProperties> {
        None
    }
}

struct FakeDriver {
    platform: Rc<Platform>,
}

impl LoopDriver for FakeDriver {
    type Stream = FakeStream;

    fn wait_any(&mut self, _: &[HANDLE], timeout: Duration) -> Result<(), CaptureError> {
        self.platform.call(Call::WaitAny)?;
        assert_eq!(timeout, Duration::from_millis(u64::from(WAIT_MS)));
        let clock = &self.platform.clock;
        clock.set(clock.get() + WAIT_QPC);
        Ok(())
    }

    fn qpc_now(&mut self) -> Result<u64, CaptureError> {
        self.platform.call(Call::QpcNow)?;
        Ok(self.platform.clock.get())
    }

    fn open(
        &mut self,
        endpoint: Endpoint,
        _: Option<&str>,
        rate: Option<u32>,
        channels: Option<u16>,
        processing: MicProcessing,
    ) -> Result<FakeStream, CaptureError> {
        self.platform.call(Call::Open(track_of(endpoint)))?;
        self.platform
            .opened_with
            .borrow_mut()
            .push((track_of(endpoint), processing));
        // As `CaptureStream::open_with`: a raw request on a mic without raw support is refused.
        if processing == MicProcessing::Raw && self.platform.mic_without_raw.get() {
            return Err(CaptureError::RawMic(RawRefusal::NotSupported {
                device_name: "Fake mic".into(),
            }));
        }
        if processing == MicProcessing::Raw
            && let Some((context, hresult)) = self.platform.raw_open_error.get()
        {
            return Err(CaptureError::Windows {
                context,
                source: windows::core::Error::from_hresult(HRESULT(hresult)),
            });
        }
        assert_eq!(rate, Some(RATE), "a re-open keeps the track's rate");
        Ok(FakeStream::new(
            &self.platform,
            endpoint,
            channels.unwrap_or(1),
        ))
    }

    fn default_output_id(&mut self) -> Option<String> {
        Some("fake-output".into())
    }

    fn take_changes(&mut self) -> Vec<DefaultDeviceChange> {
        std::mem::take(&mut *self.platform.changes.borrow_mut())
    }
}

/// What a track's observer heard.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Heard {
    /// Packets delivered back to back, from `from` to `to` (QPC).
    Audio {
        from: u64,
        to: u64,
    },
    Reopened {
        at: u64,
        reason: ReopenReason,
        opened: bool,
    },
    Ended {
        at: u64,
        hresult: i32,
    },
}

#[derive(Default)]
struct Ear {
    heard: Vec<Heard>,
    /// When the track said a raw re-open fell back to the default stream (T12.36).
    fell_back: Vec<u64>,
    /// When the track said a fallen-back mic is raw again (T12.36a).
    restored: Vec<u64>,
}

impl Ear {
    /// QPC time covered by packets.
    fn audio(&self) -> u64 {
        self.heard
            .iter()
            .map(|h| match h {
                Heard::Audio { from, to } => to - from,
                Heard::Reopened { .. } | Heard::Ended { .. } => 0,
            })
            .sum()
    }

    /// The reported re-opens ([`reopen_is_reported`]: every attempt but a failed retry).
    fn reopens(&self) -> Vec<(ReopenReason, bool)> {
        self.heard
            .iter()
            .filter_map(|h| match h {
                Heard::Reopened { reason, opened, .. }
                    if crate::observer::reopen_is_reported(*reason, *opened) =>
                {
                    Some((*reason, *opened))
                }
                Heard::Reopened { .. } => None,
                Heard::Audio { .. } | Heard::Ended { .. } => None,
            })
            .collect()
    }
}

impl TrackObserver for Ear {
    fn packet(&mut self, info: PacketInfo, samples: &[f32]) {
        assert!(!samples.is_empty());
        let end = info.qpc + u64::from(info.frames) * QPC_UNITS_PER_SEC / u64::from(RATE);
        match self.heard.last_mut() {
            Some(Heard::Audio { to, .. }) if *to == info.qpc => *to = end,
            _ => self.heard.push(Heard::Audio {
                from: info.qpc,
                to: end,
            }),
        }
    }

    fn reopened(&mut self, at: u64, reason: ReopenReason, outcome: ReopenOutcome<'_>) {
        self.heard.push(Heard::Reopened {
            at,
            reason,
            opened: outcome.is_opened(),
        });
    }

    fn track_ended(&mut self, at: u64, hresult: i32) {
        self.heard.push(Heard::Ended { at, hresult });
    }

    fn raw_fell_back(&mut self, at: u64) {
        self.fell_back.push(at);
    }

    fn raw_restored(&mut self, at: u64) {
        self.restored.push(at);
    }
}

/// A finished fake capture.
struct Run {
    result: Result<u64, CaptureError>,
    mic: Ear,
    reference: Ear,
    platform: Rc<Platform>,
}

/// Runs the loop on a fake mic and default-device loopback until the clock passes `seconds`.
fn run(faults: &[Fault], seconds: u64) -> Run {
    let platform = Rc::new(Platform {
        faults: faults.to_vec(),
        ..Platform::default()
    });
    let mut driver = FakeDriver {
        platform: Rc::clone(&platform),
    };
    let mic_endpoint = Endpoint::Microphone(MicRole::Console);
    let mic_stream = FakeStream::new(&platform, mic_endpoint, 1);
    let lb_stream = FakeStream::new(&platform, Endpoint::Loopback, 2);
    mic_stream.start().unwrap();
    lb_stream.start().unwrap();
    let mut mic = Track::new(
        mic_stream,
        mic_endpoint,
        DeviceChoice::Default,
        StreamFollower::new(Flow::Capture, Role::Console, true),
        Ear::default(),
    );
    let mut lb = Track::new(
        lb_stream,
        Endpoint::Loopback,
        DeviceChoice::Default,
        StreamFollower::new(Flow::Render, Role::Console, true),
        Ear::default(),
    );
    let end = seconds * QPC_UNITS_PER_SEC;
    let result = run_capture(&mut driver, &mut mic, Some(&mut lb), None, |now| now >= end);
    Run {
        result,
        mic: mic.observer,
        reference: lb.observer,
        platform,
    }
}

fn hresult_of(e: &CaptureError) -> Option<i32> {
    match e {
        CaptureError::Windows { source, .. } => Some(source.code().0),
        _ => None,
    }
}

const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

#[test]
fn capture_loop_fake_streams_reach_both_observers_until_done() {
    let run = run(&[], 2);
    assert_eq!(run.result.unwrap(), 2 * QPC_UNITS_PER_SEC);
    for ear in [&run.mic, &run.reference] {
        assert_eq!(
            ear.heard,
            [Heard::Audio {
                from: 0,
                to: 2 * QPC_UNITS_PER_SEC
            }],
            "one unbroken run of audio, nothing re-opened"
        );
    }
    assert_eq!(run.platform.count(Call::Open(TrackId::Mic)), 0);
    assert_eq!(run.platform.count(Call::WaitAny), 100);
}

#[test]
fn capture_loop_device_loss_on_the_third_drain_reopens_only_that_track() {
    let run = run(
        &[Fault::on(
            Call::Drain(TrackId::Mic),
            3,
            follow::AUDCLNT_E_DEVICE_INVALIDATED,
        )],
        2,
    );
    assert!(run.result.is_ok());
    assert_eq!(run.mic.reopens(), [(ReopenReason::StreamError, true)]);
    assert_eq!(run.platform.count(Call::Open(TrackId::Mic)), 1);
    // The loss lands on the third pass (60 ms); the old stream's last packets and the new
    // stream's audio follow, the re-opened stream from the moment it started.
    assert!(
        matches!(run.mic.heard[1], Heard::Reopened { at, .. } if at == 3 * WAIT_QPC),
        "{:?}",
        run.mic.heard
    );
    // The reference never noticed.
    assert!(run.reference.reopens().is_empty());
    assert_eq!(run.reference.audio(), 2 * QPC_UNITS_PER_SEC);
}

#[test]
fn capture_loop_a_reopen_that_fails_is_retried_after_the_interval() {
    let run = run(
        &[
            Fault::on(
                Call::Drain(TrackId::Mic),
                3,
                follow::AUDCLNT_E_DEVICE_INVALIDATED,
            ),
            Fault::on(
                Call::Open(TrackId::Mic),
                1,
                follow::AUDCLNT_E_DEVICE_INVALIDATED,
            ),
        ],
        3,
    );
    assert!(run.result.is_ok());
    assert_eq!(
        run.mic.reopens(),
        [
            (ReopenReason::StreamError, false),
            (ReopenReason::Retry, true)
        ]
    );
    let at: Vec<u64> = run
        .mic
        .heard
        .iter()
        .filter_map(|h| match h {
            Heard::Reopened { at, .. } => Some(*at),
            Heard::Audio { .. } | Heard::Ended { .. } => None,
        })
        .collect();
    assert_eq!(
        at,
        [3 * WAIT_QPC, 3 * WAIT_QPC + follow::RETRY_INTERVAL_QPC]
    );
    // About a second without a stream is missing from the mic's packets.
    assert_eq!(
        run.mic.audio(),
        3 * QPC_UNITS_PER_SEC - follow::RETRY_INTERVAL_QPC
    );
}

#[test]
fn capture_loop_a_programming_fault_on_a_drain_ends_the_capture() {
    let run = run(&[Fault::on(Call::Drain(TrackId::Mic), 3, E_INVALIDARG)], 2);
    let err = run.result.expect_err("an invalid argument fails fast");
    assert_eq!(hresult_of(&err), Some(E_INVALIDARG));
    assert_eq!(run.platform.count(Call::Open(TrackId::Mic)), 0);
}

#[test]
fn capture_loop_a_failed_wait_or_clock_ends_the_capture() {
    for call in [Call::WaitAny, Call::QpcNow] {
        let run = run(&[Fault::on(call, 4, E_INVALIDARG)], 2);
        let err = run.result.expect_err("the capture ends");
        assert_eq!(hresult_of(&err), Some(E_INVALIDARG), "{call:?}");
        assert_eq!(run.platform.count(call), 4, "{call:?}");
    }
}

#[test]
fn capture_loop_follows_a_default_device_change() {
    let platform = Rc::new(Platform::default());
    platform.changes.borrow_mut().push(DefaultDeviceChange {
        flow: Flow::Capture,
        role: Role::Console,
        device_id: Some("another-mic".into()),
    });
    let mut driver = FakeDriver {
        platform: Rc::clone(&platform),
    };
    let endpoint = Endpoint::Microphone(MicRole::Console);
    let stream = FakeStream::new(&platform, endpoint, 1);
    stream.start().unwrap();
    let mut mic = Track::new(
        stream,
        endpoint,
        DeviceChoice::Default,
        StreamFollower::new(Flow::Capture, Role::Console, true),
        Ear::default(),
    );
    run_capture::<_, _, Ear>(&mut driver, &mut mic, None, None, |now| {
        now >= QPC_UNITS_PER_SEC
    })
    .unwrap();
    assert_eq!(
        mic.observer.reopens(),
        [(ReopenReason::DefaultDeviceChanged, true)]
    );
    assert_eq!(platform.count(Call::Open(TrackId::Mic)), 1);
}

/// An error the capture loop has no rule for: neither a device loss nor a programming fault.
const AUDCLNT_E_CPUUSAGE_EXCEEDED: i32 = 0x8889_0017_u32 as i32;

#[test]
fn capture_loop_an_unknown_error_on_the_third_drain_reopens_that_track_only() {
    let run = run(
        &[Fault::on(
            Call::Drain(TrackId::Mic),
            3,
            AUDCLNT_E_CPUUSAGE_EXCEEDED,
        )],
        3,
    );
    assert!(run.result.is_ok(), "the capture goes on");
    assert_eq!(run.mic.reopens(), [(ReopenReason::StreamError, true)]);
    assert!(
        matches!(run.mic.heard[1], Heard::Reopened { at, .. } if at == 3 * WAIT_QPC),
        "{:?}",
        run.mic.heard
    );
    assert!(
        !run.mic
            .heard
            .iter()
            .any(|h| matches!(h, Heard::Ended { .. }))
    );
    assert_eq!(run.platform.count(Call::Open(TrackId::Mic)), 1);
    // The re-opened stream records to the end.
    assert!(
        matches!(run.mic.heard.last(), Some(Heard::Audio { to, .. }) if *to == 3 * QPC_UNITS_PER_SEC)
    );
    assert!(run.reference.reopens().is_empty());
    assert_eq!(run.reference.audio(), 3 * QPC_UNITS_PER_SEC);
}

#[test]
fn capture_loop_unknown_errors_past_the_bound_end_only_that_track() {
    let seconds = 15;
    let run = run(
        &[
            Fault::on(Call::Drain(TrackId::Mic), 3, AUDCLNT_E_CPUUSAGE_EXCEEDED),
            Fault::from(Call::Open(TrackId::Mic), 1, AUDCLNT_E_CPUUSAGE_EXCEEDED),
        ],
        seconds,
    );
    assert!(run.result.is_ok(), "the other track keeps recording");
    let first = 3 * WAIT_QPC;
    let end = first + follow::STREAM_ERROR_GRACE_QPC;
    // Re-opened at once, then every retry interval, until the bound: then the track ends.
    let attempts = 1 + follow::STREAM_ERROR_GRACE_QPC / follow::RETRY_INTERVAL_QPC;
    assert_eq!(
        run.platform.count(Call::Open(TrackId::Mic)) as u64,
        attempts
    );
    assert_eq!(
        run.mic.heard.last(),
        Some(&Heard::Ended {
            at: end,
            hresult: AUDCLNT_E_CPUUSAGE_EXCEEDED
        })
    );
    // Only the first failure is reported; the periodic retries are not.
    assert_eq!(run.mic.reopens(), [(ReopenReason::StreamError, false)]);
    assert_eq!(run.mic.audio(), first);
    // The reference never noticed.
    assert!(run.reference.reopens().is_empty());
    assert_eq!(run.reference.audio(), seconds * QPC_UNITS_PER_SEC);
}

#[test]
fn capture_loop_an_unknown_error_then_a_device_loss_is_retried_until_the_device_returns() {
    // One unknown error (the re-open works), then the device is lost: the next 21 re-opens,
    // a second apart, find it gone (twice the stream-error grace), then it comes back.
    let run = run(
        &[
            Fault::on(Call::Drain(TrackId::Mic), 3, AUDCLNT_E_CPUUSAGE_EXCEEDED),
            Fault::on(
                Call::Drain(TrackId::Mic),
                10,
                follow::AUDCLNT_E_DEVICE_INVALIDATED,
            ),
            Fault {
                call: Call::Open(TrackId::Mic),
                first: 2,
                last: 22,
                hresult: follow::AUDCLNT_E_DEVICE_INVALIDATED,
            },
        ],
        30,
    );
    assert!(run.result.is_ok());
    assert!(
        !run.mic
            .heard
            .iter()
            .any(|h| matches!(h, Heard::Ended { .. })),
        "the track never ended"
    );
    assert_eq!(
        run.mic.reopens().last(),
        Some(&(ReopenReason::Retry, true)),
        "the device was opened again when it came back"
    );
    assert!(
        matches!(run.mic.heard.last(), Some(Heard::Audio { to, .. }) if *to == 30 * QPC_UNITS_PER_SEC)
    );
}

#[test]
fn capture_loop_a_failed_reopen_logs_the_error_as_a_number_only() {
    let mut heard = Vec::new();
    let detail = CaptureError::Windows {
        context: "IAudioClient::Initialize",
        source: windows::core::Error::from_hresult(HRESULT(AUDCLNT_E_CPUUSAGE_EXCEEDED)),
    }
    .detail();
    heard.push(detail);
    heard.push(CaptureError::DeviceGone { what: "microphone" }.detail());
    assert_eq!(
        heard,
        [
            "IAudioClient::Initialize: error 0x88890017",
            "the chosen microphone is not connected"
        ]
    );
}

/// Hears what [`TrackObserver::call_output_refused`] is told.
#[derive(Default)]
struct RefusalEar {
    refused: Vec<(String, Option<String>, String)>,
}

impl TrackObserver for RefusalEar {
    fn packet(&mut self, _: PacketInfo, _: &[f32]) {}

    fn reopened(&mut self, _: u64, _: ReopenReason, _: ReopenOutcome<'_>) {}

    fn call_output_refused(&mut self, _: u64, wanted: &str, used: Option<&str>, error: &str) {
        self.refused
            .push((wanted.into(), used.map(str::to_owned), error.into()));
    }
}

/// A reading with a call app sounding on the output `id`.
fn call_on(id: &str) -> SessionReading {
    Ok(vec![crate::session::SessionPeak {
        endpoint: "Invented headset".into(),
        endpoint_id: id.into(),
        pid: 100,
        process_name: "ms-teams.exe".into(),
        active: true,
        peak: Some(0.2),
    }])
}

#[test]
fn capture_loop_a_refused_call_output_logs_the_error_as_a_number_only() {
    let platform = Rc::new(Platform {
        faults: vec![Fault::from(
            Call::Open(TrackId::Reference),
            1,
            AUDCLNT_E_CPUUSAGE_EXCEEDED,
        )],
        ..Platform::default()
    });
    let mut driver = FakeDriver {
        platform: Rc::clone(&platform),
    };
    let stream = FakeStream::new(&platform, Endpoint::Loopback, 2);
    stream.start().unwrap();
    let mut lb = Track::new(
        stream,
        Endpoint::Loopback,
        DeviceChoice::Default,
        StreamFollower::new(Flow::Render, Role::Console, true),
        RefusalEar::default(),
    );
    let mut rule = CallOutputFollower::new();
    for n in 1..=crate::call_output::POLLS_TO_MOVE {
        follow_call_reading(
            &mut rule,
            Some(call_on("headset")),
            &mut driver,
            &mut lb,
            u64::from(n),
        );
    }
    assert_eq!(platform.count(Call::Open(TrackId::Reference)), 1);
    assert_eq!(
        lb.observer.refused,
        [(
            "Invented headset".to_owned(),
            Some("Fake output".to_owned()),
            "fake: error 0x88890017".to_owned()
        )]
    );
    assert!(lb.stream.is_some(), "still recording where it was");
}

/// Hears which chosen endpoint each successful re-open says it gave up.
#[derive(Default)]
struct GiveUpEar {
    opened: Vec<Option<String>>,
}

impl TrackObserver for GiveUpEar {
    fn packet(&mut self, _: PacketInfo, _: &[f32]) {}

    fn reopened(&mut self, _: u64, _: ReopenReason, outcome: ReopenOutcome<'_>) {
        if let ReopenOutcome::Opened { gave_up, .. } = outcome {
            self.opened.push(gave_up.map(str::to_owned));
        }
    }
}

/// A loopback on `device` (moved onto the call's output first when `follow_call`) whose output
/// is lost on the third pass and cannot be opened for 15 s, so it is given up for the default.
/// What each successful re-open said it gave up.
fn lose_output(device: DeviceChoice, follow_call: bool) -> Vec<Option<String>> {
    let lost_until = 15 * QPC_UNITS_PER_SEC / follow::RETRY_INTERVAL_QPC;
    let platform = Rc::new(Platform {
        faults: vec![
            Fault::on(
                Call::Drain(TrackId::Reference),
                4,
                follow::AUDCLNT_E_DEVICE_INVALIDATED,
            ),
            Fault {
                call: Call::Open(TrackId::Reference),
                first: usize::from(follow_call) + 1,
                last: usize::from(follow_call) + lost_until as usize,
                hresult: follow::AUDCLNT_E_DEVICE_INVALIDATED,
            },
        ],
        ..Platform::default()
    });
    let mut driver = FakeDriver {
        platform: Rc::clone(&platform),
    };
    let stream = FakeStream::new(&platform, Endpoint::Loopback, 2);
    stream.start().unwrap();
    let follower = match device {
        DeviceChoice::Default => StreamFollower::new(Flow::Render, Role::Console, true),
        DeviceChoice::Endpoint { .. } => StreamFollower::chosen(Flow::Render, Role::Console),
    };
    let mut lb = Track::new(
        stream,
        Endpoint::Loopback,
        device,
        follower,
        GiveUpEar::default(),
    );
    if follow_call {
        let mut rule = CallOutputFollower::new();
        for n in 1..=crate::call_output::POLLS_TO_MOVE {
            follow_call_reading(
                &mut rule,
                Some(call_on("headset")),
                &mut driver,
                &mut lb,
                u64::from(n),
            );
        }
        assert_eq!(platform.count(Call::Open(TrackId::Reference)), 1, "moved");
    }
    let mic_stream = FakeStream::new(&platform, Endpoint::Microphone(MicRole::Console), 1);
    mic_stream.start().unwrap();
    let mut mic = Track::new(
        mic_stream,
        Endpoint::Microphone(MicRole::Console),
        DeviceChoice::Default,
        StreamFollower::new(Flow::Capture, Role::Console, true),
        Ear::default(),
    );
    let end = 30 * QPC_UNITS_PER_SEC;
    run_capture(&mut driver, &mut mic, Some(&mut lb), None, |now| now >= end).unwrap();
    assert!(lb.stream.is_some(), "recording the default at the end");
    assert!(lb.follower.follows_default());
    lb.observer.opened
}

#[test]
fn capture_loop_a_lost_call_output_is_not_reported_as_a_chosen_device() {
    // The control: an output chosen on the Audio page is named when it is given up.
    let chosen = DeviceChoice::Endpoint {
        id: "headset".into(),
        name: "Invented headset".into(),
    };
    assert_eq!(
        lose_output(chosen, false),
        [Some("Invented headset".to_owned())]
    );
    // An output the recording followed a call onto was never the user's choice: its loss
    // moves the track to the default without saying a choice was given up.
    let followed = lose_output(DeviceChoice::Default, true);
    assert_eq!(followed.len(), 2, "{followed:?}");
    assert_eq!(followed[0], None, "the move itself");
    assert_eq!(followed[1], None, "the default after the loss");
}

#[test]
fn capture_loop_a_refused_keep_awake_is_kept_as_a_number() {
    const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;
    let refused: Result<KeepAwake, CaptureError> = Err(CaptureError::Windows {
        context: "PowerSetRequest",
        source: windows::core::Error::from_hresult(HRESULT(E_ACCESSDENIED)),
    });
    assert_eq!(
        sleep_refusal(&refused).as_deref(),
        Some("PowerSetRequest: error 0x80070005")
    );
    // Both capture paths keep what a refusal says instead of dropping it.
    let source = include_str!("../wasapi.rs");
    assert!(!source.contains("let _awake = KeepAwake::hold()"));
    assert_eq!(source.matches("sleep_refusal(&awake)").count(), 2);
}

/// The places in `src` (shipped code of `wasapi.rs`) that may turn an error into text other
/// than through [`CaptureError::detail`]: any `.to_string()` except the two `PWSTR`
/// conversions (which return a `Result`, so their `.unwrap_or_default()` / `.ok()` would not
/// compile on an error's `String`), any `ToString::to_string`, and any format argument that
/// names an error binding, inline (`{e}`, `{source:?}`) or positional (`"{}", e`). The one
/// place allowed to format the Windows error is `CaptureError`'s own `Display` (what `detail`
/// avoids), which is cut out first, with `detail` itself.
fn windows_text_leaks(src: &str) -> Vec<String> {
    const ERROR_NAMES: [&str; 5] = ["e", "err", "error", "source", "why"];
    const ALLOWED_TO_STRING: [&str; 2] =
        ["p.to_string().unwrap_or_default()", "id.to_string() }.ok()"];
    let cut = |text: &str, from: &str, to: &str| -> String {
        match text.find(from) {
            Some(start) => {
                let end = text[start..]
                    .find(to)
                    .map_or(text.len(), |n| start + n + to.len());
                format!("{}{}", &text[..start], &text[end..])
            }
            None => text.to_owned(),
        }
    };
    let text = cut(src, "impl fmt::Display for CaptureError", "\n}\n");
    let text = cut(&text, "    fn detail(&self) -> String {", "\n    }\n");
    let mut leaks = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let code = line.split("//").next().unwrap_or_default();
        let mut rest = code;
        while let Some(at) = rest.find(".to_string()") {
            let around = &rest[at.saturating_sub(3)..];
            if !ALLOWED_TO_STRING.iter().any(|ok| around.contains(ok)) {
                leaks.push(format!("{n}: {line}"));
            }
            rest = &rest[at + 1..];
        }
        if code.contains("ToString::to_string") {
            leaks.push(format!("{n}: {line}"));
        }
        let formats = [
            "format!(",
            "write!(",
            "writeln!(",
            "print!(",
            "println!(",
            "panic!(",
        ]
        .iter()
        .any(|m| code.contains(m));
        for name in ERROR_NAMES {
            let inline = [format!("{{{name}}}"), format!("{{{name}:")];
            let positional = [format!(", {name})"), format!(", {name},")];
            if inline.iter().any(|p| code.contains(p.as_str()))
                || (formats && positional.iter().any(|p| code.contains(p.as_str())))
            {
                leaks.push(format!("{n}: {line}"));
            }
        }
    }
    leaks
}

#[test]
fn capture_loop_logs_no_windows_message_text() {
    // Every error the capture crate keeps as text goes through `CaptureError::detail`.
    let source = include_str!("../wasapi.rs");
    let shipped = source
        .split_once("\n#[cfg(test)]")
        .map_or(source, |(code, _)| code);
    assert!(
        shipped.contains("impl fmt::Display for CaptureError")
            && shipped.contains("    fn detail(&self) -> String {"),
        "the allowed places are where the scan expects them"
    );
    let leaks = windows_text_leaks(shipped);
    assert!(leaks.is_empty(), "errors kept as message text: {leaks:#?}");

    // The negative control: each way an error becomes its message text is caught.
    for leak in [
        "        Err(e) => Err(e.to_string()),",
        "        .map_err(|err| err.to_string())",
        "        let s = ToString::to_string(&e);",
        "        observer.note(&format!(\"open failed: {e}\"));",
        "        observer.note(&format!(\"open failed: {source:?}\"));",
        "        observer.note(&format!(\"{}\", e));",
        "        observer.note(&format!(\"{} {}\", what, error));",
        "        let text = failure.to_string();",
    ] {
        let found = windows_text_leaks(&format!("fn f() {{\n{leak}\n}}\n"));
        assert_eq!(found.len(), 1, "missed: {leak}");
    }
    // And the allowed forms pass.
    for ok in [
        "        let s = p.to_string().unwrap_or_default();",
        "            unsafe { id.to_string() }.ok()",
        "        Err(e) => Err(e.detail()),",
    ] {
        assert!(
            windows_text_leaks(&format!("fn f() {{\n{ok}\n}}\n")).is_empty(),
            "{ok}"
        );
    }
}

#[test]
fn capture_loop_every_programming_fault_on_a_drain_ends_the_capture() {
    for code in follow::PROGRAMMING_FAULTS {
        let run = run(&[Fault::on(Call::Drain(TrackId::Reference), 3, code)], 2);
        let err = run.result.expect_err("a programming fault fails fast");
        assert_eq!(hresult_of(&err), Some(code), "{code:#x}");
        assert_eq!(run.platform.count(Call::Open(TrackId::Reference)), 0);
    }
}

#[test]
fn capture_loop_both_tracks_ended_ends_the_capture() {
    let run = run(
        &[
            Fault::on(Call::Drain(TrackId::Mic), 3, AUDCLNT_E_CPUUSAGE_EXCEEDED),
            Fault::from(Call::Open(TrackId::Mic), 1, AUDCLNT_E_CPUUSAGE_EXCEEDED),
            Fault::on(
                Call::Drain(TrackId::Reference),
                5,
                AUDCLNT_E_CPUUSAGE_EXCEEDED,
            ),
            Fault::from(
                Call::Open(TrackId::Reference),
                1,
                AUDCLNT_E_CPUUSAGE_EXCEEDED,
            ),
        ],
        60,
    );
    let err = run.result.expect_err("nothing is left to record");
    assert_eq!(hresult_of(&err), Some(AUDCLNT_E_CPUUSAGE_EXCEEDED));
    assert!(matches!(run.mic.heard.last(), Some(Heard::Ended { .. })));
    assert!(matches!(
        run.reference.heard.last(),
        Some(Heard::Ended { .. })
    ));
    // It stopped when the second track ended, not at the deadline.
    assert!(run.platform.clock.get() < 60 * QPC_UNITS_PER_SEC);
}

#[test]
fn capture_loop_hresults_match_the_windows_bindings() {
    use windows::Win32::Foundation::{
        E_INVALIDARG as WIN_E_INVALIDARG, E_POINTER as WIN_E_POINTER,
    };
    use windows::Win32::Media::Audio as a;
    let pairs = [
        (follow::E_POINTER, WIN_E_POINTER),
        (follow::E_INVALIDARG, WIN_E_INVALIDARG),
        (
            follow::AUDCLNT_E_NOT_INITIALIZED,
            a::AUDCLNT_E_NOT_INITIALIZED,
        ),
        (
            follow::AUDCLNT_E_ALREADY_INITIALIZED,
            a::AUDCLNT_E_ALREADY_INITIALIZED,
        ),
        (
            follow::AUDCLNT_E_WRONG_ENDPOINT_TYPE,
            a::AUDCLNT_E_WRONG_ENDPOINT_TYPE,
        ),
        (follow::AUDCLNT_E_NOT_STOPPED, a::AUDCLNT_E_NOT_STOPPED),
        (follow::AUDCLNT_E_OUT_OF_ORDER, a::AUDCLNT_E_OUT_OF_ORDER),
        (follow::AUDCLNT_E_INVALID_SIZE, a::AUDCLNT_E_INVALID_SIZE),
        (
            follow::AUDCLNT_E_BUFFER_OPERATION_PENDING,
            a::AUDCLNT_E_BUFFER_OPERATION_PENDING,
        ),
        (
            follow::AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED,
            a::AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED,
        ),
        (
            follow::AUDCLNT_E_EVENTHANDLE_NOT_SET,
            a::AUDCLNT_E_EVENTHANDLE_NOT_SET,
        ),
        (
            follow::AUDCLNT_E_DEVICE_INVALIDATED,
            a::AUDCLNT_E_DEVICE_INVALIDATED,
        ),
        (
            follow::AUDCLNT_E_SERVICE_NOT_RUNNING,
            a::AUDCLNT_E_SERVICE_NOT_RUNNING,
        ),
        (
            follow::AUDCLNT_E_RESOURCES_INVALIDATED,
            a::AUDCLNT_E_RESOURCES_INVALIDATED,
        ),
        (AUDCLNT_E_CPUUSAGE_EXCEEDED, a::AUDCLNT_E_CPUUSAGE_EXCEEDED),
    ];
    for (ours, theirs) in pairs {
        assert_eq!(ours, theirs.0, "{ours:#x}");
    }
}

/// Runs the loop for `seconds` on a fake mic opened with `processing` and a default-device
/// loopback, with the default mic and the default output both changing on the first pass.
/// `mic_without_raw_until` is how long (QPC) the new default mic lacks raw support.
fn run_with_default_changes(
    processing: MicProcessing,
    mic_without_raw_until: u64,
    seconds: u64,
) -> Run {
    let (result, mic, reference, platform) = drive_default_changes(
        processing,
        mic_without_raw_until,
        seconds,
        &[],
        Ear::default(),
    );
    Run {
        result,
        mic,
        reference,
        platform,
    }
}

/// [`run_with_default_changes`] with `faults` and `mic_observer` on the mic track.
fn drive_default_changes<O: TrackObserver>(
    processing: MicProcessing,
    mic_without_raw_until: u64,
    seconds: u64,
    faults: &[Fault],
    mic_observer: O,
) -> (Result<u64, CaptureError>, O, Ear, Rc<Platform>) {
    drive_mic(
        processing,
        false,
        None,
        mic_without_raw_until,
        seconds,
        faults,
        mic_observer,
    )
}

/// [`run_with_default_changes`] on the live source's mic (T12.36): a raw re-open that is
/// refused falls back to the default stream.
fn run_live_with_default_changes(
    processing: MicProcessing,
    mic_without_raw_until: u64,
    seconds: u64,
    faults: &[Fault],
) -> Run {
    run_live_mic(processing, None, mic_without_raw_until, seconds, faults)
}

/// [`run_live_with_default_changes`] where, when `raw_error` is set, raw opens fail with that
/// call and HRESULT (until `mic_without_raw_until`) instead of the mic refusing raw.
fn run_live_mic(
    processing: MicProcessing,
    raw_error: Option<(&'static str, i32)>,
    mic_without_raw_until: u64,
    seconds: u64,
    faults: &[Fault],
) -> Run {
    let (result, mic, reference, platform) = drive_mic(
        processing,
        true,
        raw_error,
        mic_without_raw_until,
        seconds,
        faults,
        Ear::default(),
    );
    Run {
        result,
        mic,
        reference,
        platform,
    }
}

/// [`drive_default_changes`], the mic track built as the live source builds it when
/// `raw_fallback` is set (`Track::with_raw_fallback`).
fn drive_mic<O: TrackObserver>(
    processing: MicProcessing,
    raw_fallback: bool,
    raw_error: Option<(&'static str, i32)>,
    mic_without_raw_until: u64,
    seconds: u64,
    faults: &[Fault],
    mic_observer: O,
) -> (Result<u64, CaptureError>, O, Ear, Rc<Platform>) {
    let platform = Rc::new(Platform {
        faults: faults.to_vec(),
        ..Platform::default()
    });
    if raw_error.is_some() {
        platform.raw_open_error.set(raw_error);
    } else {
        platform.mic_without_raw.set(mic_without_raw_until > 0);
    }
    for (flow, id) in [
        (Flow::Capture, "another-mic"),
        (Flow::Render, "another-output"),
    ] {
        platform.changes.borrow_mut().push(DefaultDeviceChange {
            flow,
            role: Role::Console,
            device_id: Some(id.into()),
        });
    }
    let mut driver = FakeDriver {
        platform: Rc::clone(&platform),
    };
    let mic_endpoint = Endpoint::Microphone(MicRole::Console);
    let mic_stream = FakeStream::new(&platform, mic_endpoint, 1);
    let lb_stream = FakeStream::new(&platform, Endpoint::Loopback, 2);
    mic_stream.start().unwrap();
    lb_stream.start().unwrap();
    let mut mic = Track::new(
        mic_stream,
        mic_endpoint,
        DeviceChoice::Default,
        StreamFollower::new(Flow::Capture, Role::Console, true),
        mic_observer,
    )
    .with_processing(processing);
    if raw_fallback {
        mic = mic.with_raw_fallback(false);
    }
    let mut lb = Track::new(
        lb_stream,
        Endpoint::Loopback,
        DeviceChoice::Default,
        StreamFollower::new(Flow::Render, Role::Console, true),
        Ear::default(),
    );
    let end = seconds * QPC_UNITS_PER_SEC;
    let result = run_capture(&mut driver, &mut mic, Some(&mut lb), None, |now| {
        if now >= mic_without_raw_until {
            platform.mic_without_raw.set(false);
            platform.raw_open_error.set(None);
        }
        now >= end
    });
    (result, mic.observer, lb.observer, platform)
}

/// What the opens of `track` were asked for.
fn asked_for(platform: &Platform, track: TrackId) -> Vec<MicProcessing> {
    platform
        .opened_with
        .borrow()
        .iter()
        .filter(|(t, _)| *t == track)
        .map(|(_, p)| *p)
        .collect()
}

#[test]
fn capture_loop_a_raw_mic_asks_for_raw_again_when_it_is_reopened() {
    let run = run_with_default_changes(MicProcessing::Raw, 0, 2);
    assert!(run.result.is_ok());
    assert_eq!(
        run.mic.reopens(),
        [(ReopenReason::DefaultDeviceChanged, true)]
    );
    assert_eq!(
        asked_for(&run.platform, TrackId::Mic),
        [MicProcessing::Raw],
        "the re-open keeps the request"
    );
    // Raw is the mic's alone: the loopback re-opened beside it declares nothing.
    assert_eq!(
        run.reference.reopens(),
        [(ReopenReason::DefaultDeviceChanged, true)]
    );
    assert_eq!(
        asked_for(&run.platform, TrackId::Reference),
        [MicProcessing::Default]
    );
}

#[test]
fn capture_loop_tracks_ask_for_the_default_processing_unless_told_otherwise() {
    // Every track but the raw recorder's mic: what the app's live sessions run.
    let changed = run_with_default_changes(MicProcessing::Default, 0, 2);
    assert!(changed.result.is_ok());
    assert_eq!(
        asked_for(&changed.platform, TrackId::Mic),
        [MicProcessing::Default]
    );
    assert_eq!(
        asked_for(&changed.platform, TrackId::Reference),
        [MicProcessing::Default]
    );
    // And a track built without a word about processing, after a device loss.
    let lost = run(
        &[Fault::on(
            Call::Drain(TrackId::Mic),
            3,
            follow::AUDCLNT_E_DEVICE_INVALIDATED,
        )],
        2,
    );
    assert_eq!(
        asked_for(&lost.platform, TrackId::Mic),
        [MicProcessing::Default]
    );
}

#[test]
fn capture_loop_a_raw_mic_reopened_on_a_mic_without_raw_records_no_processed_audio() {
    // The new default mic has no raw support for the first 3 s, then one that has it returns.
    let back = 3 * QPC_UNITS_PER_SEC;
    let run = run_with_default_changes(MicProcessing::Raw, back, 5);
    assert!(run.result.is_ok(), "the recording goes on");
    // The failed re-open is reported once, its retries are not, and the one that works is.
    assert_eq!(
        run.mic.reopens(),
        [
            (ReopenReason::DefaultDeviceChanged, false),
            (ReopenReason::Retry, true)
        ]
    );
    let asked = asked_for(&run.platform, TrackId::Mic);
    assert!(asked.len() >= 3, "retried every interval: {asked:?}");
    assert!(
        asked.iter().all(|p| *p == MicProcessing::Raw),
        "never asked for the processed stream: {asked:?}"
    );
    // The track did not end, and nothing was recorded while no raw mic was open: only the
    // first pass before the change, and what followed the mic's return.
    assert!(
        !run.mic
            .heard
            .iter()
            .any(|h| matches!(h, Heard::Ended { .. }))
    );
    let audio: Vec<(u64, u64)> = run
        .mic
        .heard
        .iter()
        .filter_map(|h| match h {
            Heard::Audio { from, to } => Some((*from, *to)),
            Heard::Reopened { .. } | Heard::Ended { .. } => None,
        })
        .collect();
    assert_eq!(audio.len(), 2, "{audio:?}");
    assert_eq!(audio[0], (0, WAIT_QPC));
    assert!(audio[1].0 >= back, "{audio:?}");
    assert_eq!(audio[1].1, 5 * QPC_UNITS_PER_SEC);
    // The reference re-opened once and recorded throughout.
    assert_eq!(
        run.reference.reopens(),
        [(ReopenReason::DefaultDeviceChanged, true)]
    );
}

/// Records the mic of [`drive_default_changes`] as `record_raw` does (a [`RecordingTrack`]
/// inside a [`NotifyingTrack`]) and finishes it at the end of the capture.
fn record_default_changes(
    processing: MicProcessing,
    mic_without_raw_until: u64,
    seconds: u64,
    faults: &[Fault],
) -> crate::recording::TrackRecording {
    let first = DeviceDescription {
        name: "Fake mic".into(),
        format: MixFormat::from_waveformat_bytes(&float_waveformat_bytes(RATE, 1)).unwrap(),
    };
    let track = RecordingTrack::new(first, crate::timeline::TrackTimeline::new(RATE, 0));
    let observer = NotifyingTrack::new(track, |_: &HandsFreeMic| {});
    let (result, mic, _, _) =
        drive_default_changes(processing, mic_without_raw_until, seconds, faults, observer);
    mic.into_track()
        .finish(result.expect("the recording goes on"))
}

fn qpc_frames(qpc: u64) -> u64 {
    qpc * u64::from(RATE) / QPC_UNITS_PER_SEC
}

#[test]
fn capture_loop_a_raw_mic_reopen_refused_counts_the_silence_it_leaves() {
    // The new default mic has no raw support for the first 3 s of a 5 s take.
    let rec = record_default_changes(MicProcessing::Raw, 3 * QPC_UNITS_PER_SEC, 5, &[]);
    let back = rec
        .device_changes
        .iter()
        .find(|c| c.reason == ReopenReason::Retry && c.device_name.is_some())
        .expect("the raw mic came back")
        .qpc;
    assert!(back >= 3 * QPC_UNITS_PER_SEC, "{back}");
    // From the refused re-open (the first pass) to the first packet of the returned mic.
    let silent = qpc_frames(back - WAIT_QPC);
    assert_eq!(rec.raw_refused_frames, silent);
    assert!(silent > 2 * u64::from(RATE), "{silent}");
    // It is the track's silence, not something counted beside it.
    assert_eq!(rec.timeline.packets()[1].silence_before, silent);
    assert!(
        rec.timeline.samples()[qpc_frames(WAIT_QPC) as usize..][..silent as usize]
            .iter()
            .all(|&s| s == 0.0)
    );

    // A raw mic that never comes back: silence to the end of the take.
    let rec = record_default_changes(MicProcessing::Raw, 10 * QPC_UNITS_PER_SEC, 5, &[]);
    assert_eq!(
        rec.raw_refused_frames,
        qpc_frames(5 * QPC_UNITS_PER_SEC - WAIT_QPC)
    );
    assert_eq!(
        rec.raw_refused_frames,
        rec.timeline.frames() - qpc_frames(WAIT_QPC)
    );
}

#[test]
fn capture_loop_only_a_refused_raw_reopen_counts_as_raw_silence() {
    // A raw mic re-opened raw at once.
    let rec = record_default_changes(MicProcessing::Raw, 0, 3, &[]);
    assert_eq!(rec.raw_refused_frames, 0);
    // A mic that cannot be opened for a second for another reason (device gone) leaves
    // silence too, but none of it is a raw refusal: raw or default.
    for processing in [MicProcessing::Raw, MicProcessing::Default] {
        let gone = Fault::on(
            Call::Open(TrackId::Mic),
            1,
            follow::AUDCLNT_E_DEVICE_INVALIDATED,
        );
        let rec = record_default_changes(processing, 0, 3, &[gone]);
        assert!(rec.timeline.synthesized_frames() > u64::from(RATE / 2));
        assert_eq!(rec.raw_refused_frames, 0, "{processing:?}");
    }
}

#[test]
fn capture_loop_a_live_raw_mic_reopened_on_a_mic_without_raw_records_the_default_stream() {
    // The new default mic never supports raw in this 5 s take.
    let run = run_live_with_default_changes(MicProcessing::Raw, 10 * QPC_UNITS_PER_SEC, 5, &[]);
    assert!(run.result.is_ok(), "the recording goes on");
    // One re-open, which worked: raw was refused and the default stream opened at once.
    assert_eq!(
        run.mic.reopens(),
        [(ReopenReason::DefaultDeviceChanged, true)]
    );
    assert_eq!(
        asked_for(&run.platform, TrackId::Mic),
        [MicProcessing::Raw, MicProcessing::Default]
    );
    // Said once, at the re-open, after its report.
    let reopened_at = run
        .mic
        .heard
        .iter()
        .find_map(|h| match h {
            Heard::Reopened { at, .. } => Some(*at),
            Heard::Audio { .. } | Heard::Ended { .. } => None,
        })
        .expect("a re-open");
    assert_eq!(run.mic.fell_back, [reopened_at]);
    // No silence: the old stream up to the change, then the default stream to the end.
    assert_eq!(run.mic.audio(), 5 * QPC_UNITS_PER_SEC);
    assert!(
        !run.mic
            .heard
            .iter()
            .any(|h| matches!(h, Heard::Ended { .. }))
    );
}

#[test]
fn capture_loop_a_live_mic_that_fell_back_keeps_the_default_stream() {
    // After the fallback the mic is lost twice more; even once a raw mic is back (from 1 s)
    // its re-opens keep the default stream it fell back to: only the timed try for raw
    // (T12.36a, first one 10 s after the fallback, past this 4 s take) goes back to raw.
    let losses = [
        Fault::on(
            Call::Drain(TrackId::Mic),
            60,
            follow::AUDCLNT_E_DEVICE_INVALIDATED,
        ),
        Fault::on(
            Call::Drain(TrackId::Mic),
            120,
            follow::AUDCLNT_E_DEVICE_INVALIDATED,
        ),
    ];
    let run = run_live_with_default_changes(MicProcessing::Raw, QPC_UNITS_PER_SEC, 4, &losses);
    assert!(run.result.is_ok());
    assert_eq!(
        asked_for(&run.platform, TrackId::Mic),
        [
            MicProcessing::Raw,
            MicProcessing::Default,
            MicProcessing::Default,
            MicProcessing::Default
        ]
    );
    assert_eq!(run.mic.fell_back.len(), 1, "{:?}", run.mic.fell_back);
}

#[test]
fn capture_loop_a_live_raw_mic_falls_back_only_when_raw_is_refused() {
    // A raw mic re-opened raw at once: no fallback, no warning.
    let run = run_live_with_default_changes(MicProcessing::Raw, 0, 2, &[]);
    assert_eq!(asked_for(&run.platform, TrackId::Mic), [MicProcessing::Raw]);
    assert!(run.mic.fell_back.is_empty());
    // A raw mic lost for another reason (its re-open fails as a gone device) keeps asking for
    // raw: losing a device is not a refusal of raw.
    let gone = Fault::on(
        Call::Open(TrackId::Mic),
        1,
        follow::AUDCLNT_E_DEVICE_INVALIDATED,
    );
    let run = run_live_with_default_changes(MicProcessing::Raw, 0, 3, &[gone]);
    assert!(run.result.is_ok());
    // (`asked_for` lists the opens that got past the fault.)
    assert!(run.platform.count(Call::Open(TrackId::Mic)) >= 2);
    let asked = asked_for(&run.platform, TrackId::Mic);
    assert!(!asked.is_empty());
    assert!(
        asked.iter().all(|p| *p == MicProcessing::Raw),
        "never fell back on a lost device: {asked:?}"
    );
    assert!(run.mic.fell_back.is_empty());
    // The live source's default mic never asks for raw.
    let run = run_live_with_default_changes(MicProcessing::Default, 10 * QPC_UNITS_PER_SEC, 2, &[]);
    assert_eq!(
        asked_for(&run.platform, TrackId::Mic),
        [MicProcessing::Default]
    );
    assert!(run.mic.fell_back.is_empty());
}

#[test]
fn capture_loop_a_live_raw_mic_whose_default_stream_also_fails_is_retried_on_the_default() {
    // Raw refused, and the default open right after it fails too (the device went away):
    // the fallback is said once and every retry asks for the default stream.
    let gone = Fault::on(
        Call::Open(TrackId::Mic),
        2,
        follow::AUDCLNT_E_DEVICE_INVALIDATED,
    );
    let run = run_live_with_default_changes(MicProcessing::Raw, 10 * QPC_UNITS_PER_SEC, 3, &[gone]);
    assert!(run.result.is_ok());
    // Raw, the failed default (not listed: the fault comes first), then the retry.
    assert_eq!(run.platform.count(Call::Open(TrackId::Mic)), 3);
    assert_eq!(
        asked_for(&run.platform, TrackId::Mic),
        [MicProcessing::Raw, MicProcessing::Default]
    );
    assert_eq!(run.mic.fell_back.len(), 1);
    assert_eq!(
        run.mic.reopens(),
        [
            (ReopenReason::DefaultDeviceChanged, false),
            (ReopenReason::Retry, true)
        ]
    );
}

#[test]
fn capture_loop_the_live_start_opens_the_default_stream_when_raw_fails() {
    let refused = || {
        CaptureError::RawMic(RawRefusal::NotSupported {
            device_name: "Fake mic".into(),
        })
    };
    // Raw asked and refused: the default stream, and why raw was not used.
    let mut asked = Vec::new();
    let (stream, used, why) = prefer_raw(MicProcessing::Raw, |p| {
        asked.push(p);
        match p {
            MicProcessing::Raw => Err(refused()),
            MicProcessing::Default => Ok("default stream"),
        }
    })
    .unwrap();
    assert_eq!(asked, [MicProcessing::Raw, MicProcessing::Default]);
    assert_eq!((stream, used), ("default stream", MicProcessing::Default));
    assert_eq!(why.as_deref(), Some(refused().to_string().as_str()));
    // Any failure of the raw open, not only a refusal, costs nothing but raw.
    let (_, used, why) = prefer_raw(MicProcessing::Raw, |p| match p {
        MicProcessing::Raw => Err(hresult_error(E_INVALIDARG)),
        MicProcessing::Default => Ok(()),
    })
    .unwrap();
    assert_eq!(used, MicProcessing::Default);
    assert!(why.is_some_and(|w| w.contains("0x80070057")));
    // Raw used: no reason.
    let (_, used, why) = prefer_raw(MicProcessing::Raw, |_| Ok(())).unwrap();
    assert_eq!((used, why), (MicProcessing::Raw, None));
    // The default asked: opened once, its error is the open's.
    let mut asked = Vec::new();
    let failed = prefer_raw(MicProcessing::Default, |p| -> Result<(), _> {
        asked.push(p);
        Err(hresult_error(E_INVALIDARG))
    });
    assert!(failed.is_err());
    assert_eq!(asked, [MicProcessing::Default]);
    // Raw refused and the default failing too: the default's error.
    let failed = prefer_raw(MicProcessing::Raw, |p| -> Result<(), _> {
        match p {
            MicProcessing::Raw => Err(refused()),
            MicProcessing::Default => Err(hresult_error(follow::AUDCLNT_E_DEVICE_INVALIDATED)),
        }
    });
    assert!(failed.is_err_and(|e| e.is_device_loss()));
    // A raw open that fails because the device is lost is the open's error: no fallback.
    let mut asked = Vec::new();
    let lost = prefer_raw(MicProcessing::Raw, |p| -> Result<(), _> {
        asked.push(p);
        Err(hresult_error(follow::AUDCLNT_E_DEVICE_INVALIDATED))
    });
    assert!(lost.is_err_and(|e| e.is_device_loss()));
    assert_eq!(asked, [MicProcessing::Raw]);
}

#[test]
fn capture_loop_any_raw_failure_but_a_lost_device_falls_back() {
    // The one rule of the live start and the live re-open (T12.36).
    assert!(
        CaptureError::RawMic(RawRefusal::NotSupported {
            device_name: "Fake mic".into()
        })
        .raw_falls_back()
    );
    let windows = |context, hresult| CaptureError::Windows {
        context,
        source: windows::core::Error::from_hresult(HRESULT(hresult)),
    };
    for context in [
        RAW_PROPERTIES_CONTEXT,
        RAW_CLIENT2_CONTEXT,
        "IAudioClient::Initialize",
    ] {
        assert!(windows(context, E_INVALIDARG).raw_falls_back(), "{context}");
        // A device lost during the raw calls is a lost device, not a refusal of raw.
        assert!(
            !windows(context, follow::AUDCLNT_E_DEVICE_INVALIDATED).raw_falls_back(),
            "{context}"
        );
    }
    assert!(!CaptureError::DeviceGone { what: "microphone" }.raw_falls_back());
    let format = CaptureError::Format(FormatError::Unsupported("24-bit packed".into()));
    assert!(format.raw_falls_back());
}

#[test]
fn capture_loop_a_live_raw_mic_whose_raw_open_fails_later_records_the_default_stream() {
    // Raw is accepted but the open fails afterwards (here Initialize) on every raw open: the
    // live mic falls back to the default stream at once, never loses the mic, and says so once.
    let run = run_live_mic(
        MicProcessing::Raw,
        Some(("IAudioClient::Initialize", E_INVALIDARG)),
        10 * QPC_UNITS_PER_SEC,
        5,
        &[],
    );
    assert!(run.result.is_ok());
    assert_eq!(
        asked_for(&run.platform, TrackId::Mic),
        [MicProcessing::Raw, MicProcessing::Default]
    );
    assert_eq!(run.mic.fell_back.len(), 1);
    assert!(
        !run.mic
            .heard
            .iter()
            .any(|h| matches!(h, Heard::Ended { .. })),
        "the mic track never ends"
    );
    assert_eq!(run.mic.audio(), 5 * QPC_UNITS_PER_SEC, "no silence");
    // The raw recorder keeps refusing on the same failure: no default stream is ever asked for.
    let (result, mic, _, platform) = drive_mic(
        MicProcessing::Raw,
        false,
        Some(("IAudioClient::Initialize", E_INVALIDARG)),
        10 * QPC_UNITS_PER_SEC,
        3,
        &[],
        Ear::default(),
    );
    assert!(result.is_ok());
    assert!(
        asked_for(&platform, TrackId::Mic)
            .iter()
            .all(|p| *p == MicProcessing::Raw)
    );
    assert!(mic.fell_back.is_empty());
}

#[test]
fn capture_loop_a_device_lost_inside_the_raw_calls_is_retried_raw() {
    // The raw calls fail with a lost device for the first second: a lost device, retried raw,
    // never a fallback to the processed stream.
    let run = run_live_mic(
        MicProcessing::Raw,
        Some((RAW_PROPERTIES_CONTEXT, follow::AUDCLNT_E_DEVICE_INVALIDATED)),
        QPC_UNITS_PER_SEC,
        3,
        &[],
    );
    assert!(run.result.is_ok());
    let asked = asked_for(&run.platform, TrackId::Mic);
    assert!(asked.len() >= 2, "retried: {asked:?}");
    assert!(asked.iter().all(|p| *p == MicProcessing::Raw), "{asked:?}");
    assert!(run.mic.fell_back.is_empty());
    assert!(
        !run.mic
            .heard
            .iter()
            .any(|h| matches!(h, Heard::Ended { .. }))
    );
}

/// Runs the loop for `seconds` on a fake live mic (raw fallback on) opened with `processing`
/// and a default-device loopback; `fell_back_at_start` as `Track::with_raw_fallback` takes it.
/// `each(now, platform)` runs on every pass, before the loop decides, to script default-device
/// changes and raw support. Raw is supported unless `each` says otherwise.
fn run_live_scripted(
    processing: MicProcessing,
    fell_back_at_start: bool,
    seconds: u64,
    faults: &[Fault],
    mut each: impl FnMut(u64, &Platform),
) -> Run {
    let platform = Rc::new(Platform {
        faults: faults.to_vec(),
        ..Platform::default()
    });
    let mut driver = FakeDriver {
        platform: Rc::clone(&platform),
    };
    let mic_endpoint = Endpoint::Microphone(MicRole::Console);
    let mic_stream = FakeStream::new(&platform, mic_endpoint, 1);
    let lb_stream = FakeStream::new(&platform, Endpoint::Loopback, 2);
    mic_stream.start().unwrap();
    lb_stream.start().unwrap();
    let mut mic = Track::new(
        mic_stream,
        mic_endpoint,
        DeviceChoice::Default,
        StreamFollower::new(Flow::Capture, Role::Console, true),
        Ear::default(),
    )
    .with_processing(processing)
    .with_raw_fallback(fell_back_at_start);
    let mut lb = Track::new(
        lb_stream,
        Endpoint::Loopback,
        DeviceChoice::Default,
        StreamFollower::new(Flow::Render, Role::Console, true),
        Ear::default(),
    );
    let end = seconds * QPC_UNITS_PER_SEC;
    let result = run_capture(&mut driver, &mut mic, Some(&mut lb), None, |now| {
        each(now, &platform);
        now >= end
    });
    Run {
        result,
        mic: mic.observer,
        reference: lb.observer,
        platform,
    }
}

/// A new default mic, as Windows announces it.
fn new_default_mic(platform: &Platform, id: &str) {
    platform.changes.borrow_mut().push(DefaultDeviceChange {
        flow: Flow::Capture,
        role: Role::Console,
        device_id: Some(id.into()),
    });
}

/// When each reported re-open of `ear` happened, with its reason and whether it opened.
fn reopens_at(ear: &Ear) -> Vec<(u64, ReopenReason, bool)> {
    ear.heard
        .iter()
        .filter_map(|h| match h {
            Heard::Reopened { at, reason, opened }
                if crate::observer::reopen_is_reported(*reason, *opened) =>
            {
                Some((*at, *reason, *opened))
            }
            Heard::Reopened { .. } | Heard::Audio { .. } | Heard::Ended { .. } => None,
        })
        .collect()
}

#[test]
fn capture_loop_a_live_raw_mic_reopened_raw_after_a_stream_error_stays_raw() {
    // T12.36a, the observed sequence: a raw mic, a stream error that is not a device loss,
    // then a raw re-open that is accepted. The track stays raw and nothing is said.
    let error = Fault::on(Call::Drain(TrackId::Mic), 50, AUDCLNT_E_CPUUSAGE_EXCEEDED);
    let run = run_live_scripted(MicProcessing::Raw, false, 3, &[error], |_, _| {});
    assert!(run.result.is_ok());
    assert_eq!(asked_for(&run.platform, TrackId::Mic), [MicProcessing::Raw]);
    assert_eq!(run.mic.reopens(), [(ReopenReason::StreamError, true)]);
    assert!(run.mic.fell_back.is_empty());
    assert!(run.mic.restored.is_empty());
}

#[test]
fn capture_loop_a_live_mic_that_fell_back_tries_raw_again_until_it_opens() {
    // T12.36a: raw is refused at a default-device change (announced at 1 s, re-opened on the
    // next pass) and stays refused until 15 s. The fallback is said once; the try 10 s later is
    // refused and changes nothing (no re-open, no note); the try 20 s after that opens raw: one
    // re-open onto it, one note, and the track is raw again with no gap in the recording.
    let refused_until = 15 * QPC_UNITS_PER_SEC;
    let run = run_live_scripted(MicProcessing::Raw, false, 40, &[], |now, p| {
        if now == QPC_UNITS_PER_SEC {
            p.mic_without_raw.set(true);
            new_default_mic(p, "another-mic");
        }
        if now >= refused_until {
            p.mic_without_raw.set(false);
        }
    });
    assert!(run.result.is_ok());
    let first = QPC_UNITS_PER_SEC + WAIT_QPC;
    assert_eq!(run.mic.fell_back, [first]);
    let tried = first + follow::RAW_RETRY_INTERVAL_QPC;
    let back = tried + 2 * follow::RAW_RETRY_INTERVAL_QPC;
    assert_eq!(
        asked_for(&run.platform, TrackId::Mic),
        [
            MicProcessing::Raw,
            MicProcessing::Default,
            MicProcessing::Raw,
            MicProcessing::Raw,
        ]
    );
    assert_eq!(
        reopens_at(&run.mic),
        [
            (first, ReopenReason::DefaultDeviceChanged, true),
            (back, ReopenReason::RawRestored, true),
        ]
    );
    assert_eq!(run.mic.restored, [back]);
    assert_eq!(
        run.mic.audio(),
        40 * QPC_UNITS_PER_SEC,
        "no gap: each new stream opens before the old one closes"
    );
    // Raw again: no more tries.
    assert_eq!(run.platform.count(Call::Open(TrackId::Mic)), 4);
}

#[test]
fn capture_loop_a_live_mic_back_to_raw_falls_back_again_and_says_so_again() {
    // T12.36a: after raw is back, a later refusal at a device change falls back and is said
    // again, and the tries start over from the first wait.
    let run = run_live_scripted(MicProcessing::Raw, false, 40, &[], |now, p| {
        let s = QPC_UNITS_PER_SEC;
        if now == s || now == 20 * s {
            p.mic_without_raw.set(true);
            new_default_mic(p, if now == s { "mic-b" } else { "mic-c" });
        }
        if now == 5 * s || now == 25 * s {
            p.mic_without_raw.set(false);
        }
    });
    assert!(run.result.is_ok());
    let s = QPC_UNITS_PER_SEC;
    let r = follow::RAW_RETRY_INTERVAL_QPC;
    let w = WAIT_QPC;
    assert_eq!(run.mic.fell_back, [s + w, 20 * s + w]);
    assert_eq!(run.mic.restored, [s + w + r, 20 * s + w + r]);
    assert_eq!(run.mic.audio(), 40 * s);
}

#[test]
fn capture_loop_a_live_mic_that_fell_back_at_the_start_tries_raw_too() {
    // T12.36a: the start opened the default stream because raw failed; the track tries raw on
    // the same schedule, from its first pass.
    let run = run_live_scripted(MicProcessing::Default, true, 12, &[], |_, _| {});
    assert!(run.result.is_ok());
    let first_pass = WAIT_QPC;
    let back = first_pass + follow::RAW_RETRY_INTERVAL_QPC;
    assert_eq!(asked_for(&run.platform, TrackId::Mic), [MicProcessing::Raw]);
    assert_eq!(
        reopens_at(&run.mic),
        [(back, ReopenReason::RawRestored, true)]
    );
    assert_eq!(run.mic.restored, [back]);
    assert!(run.mic.fell_back.is_empty(), "the start said it already");
}

#[test]
fn capture_loop_a_live_mic_tries_raw_only_when_raw_was_asked_for() {
    // The live source's processed mic (raw never asked for) and a raw mic that never fell
    // back open nothing on their own.
    for (processing, fell_back_at_start) in
        [(MicProcessing::Default, false), (MicProcessing::Raw, false)]
    {
        let run = run_live_scripted(processing, fell_back_at_start, 30, &[], |_, _| {});
        assert!(run.result.is_ok());
        assert_eq!(
            run.platform.count(Call::Open(TrackId::Mic)),
            0,
            "{processing:?}"
        );
        assert!(run.mic.restored.is_empty());
    }
}

#[test]
fn capture_loop_a_raw_try_waits_while_the_mic_has_no_stream() {
    // T12.36a: a mic that fell back and then lost its device (every open fails as a gone
    // device from 2 s on) tries no raw while it has no stream: only the follower's re-opens,
    // all asking for the default stream it fell back to.
    let gone = Fault::from(
        Call::Open(TrackId::Mic),
        3,
        follow::AUDCLNT_E_DEVICE_INVALIDATED,
    );
    let lost = Fault::from(
        Call::Drain(TrackId::Mic),
        100,
        follow::AUDCLNT_E_DEVICE_INVALIDATED,
    );
    let run = run_live_scripted(MicProcessing::Raw, false, 15, &[gone, lost], |now, p| {
        if now == QPC_UNITS_PER_SEC {
            p.mic_without_raw.set(true);
            new_default_mic(p, "mic-b");
        }
    });
    assert!(run.result.is_ok());
    assert_eq!(run.mic.fell_back, [QPC_UNITS_PER_SEC + WAIT_QPC]);
    let asked = asked_for(&run.platform, TrackId::Mic);
    assert_eq!(
        asked,
        [MicProcessing::Raw, MicProcessing::Default],
        "{asked:?}"
    );
    assert!(
        run.platform.count(Call::Open(TrackId::Mic)) > 3,
        "the follower kept re-opening"
    );
    assert!(run.mic.restored.is_empty());
}

#[test]
fn capture_loop_the_raw_recorder_never_tries_raw_on_its_own() {
    // The raw recorder refuses and never falls back, so it has nothing to try again: a mic
    // without raw for 15 s of a 30 s take asks only for raw, every attempt a follower re-open.
    let (result, mic, _, platform) = drive_default_changes(
        MicProcessing::Raw,
        15 * QPC_UNITS_PER_SEC,
        30,
        &[],
        Ear::default(),
    );
    assert!(result.is_ok());
    let asked = asked_for(&platform, TrackId::Mic);
    assert!(asked.iter().all(|p| *p == MicProcessing::Raw), "{asked:?}");
    assert!(mic.restored.is_empty() && mic.fell_back.is_empty());
    assert!(
        !mic.reopens()
            .iter()
            .any(|(reason, _)| *reason == ReopenReason::RawRestored)
    );
}

#[test]
fn capture_loop_the_live_start_keeps_a_chosen_mic_that_refuses_raw() {
    // What each open was asked for: (device, processing).
    let opened = std::cell::RefCell::new(Vec::new());
    let refused = || {
        CaptureError::RawMic(RawRefusal::NotSupported {
            device_name: "Chosen mic".into(),
        })
    };
    // The chosen mic refuses raw: it is opened again on the same device with the default
    // processing, never replaced by the default device.
    let (chosen, used, why) = open_live_mic_with(Some("chosen"), MicProcessing::Raw, |id, p| {
        opened.borrow_mut().push((id.map(str::to_owned), p));
        match (id, p) {
            (Some("chosen"), MicProcessing::Raw) => Err(refused()),
            _ => Ok(id.map(str::to_owned)),
        }
    })
    .unwrap();
    assert!(
        matches!(chosen, Chosen::Asked(Some(ref d)) if d == "chosen"),
        "{chosen:?}"
    );
    assert_eq!(used, MicProcessing::Default);
    assert!(why.is_some());
    assert_eq!(
        *opened.borrow(),
        [
            (Some("chosen".to_owned()), MicProcessing::Raw),
            (Some("chosen".to_owned()), MicProcessing::Default)
        ]
    );
    // A chosen mic that is gone: the default device, raw if it allows it.
    opened.borrow_mut().clear();
    let (chosen, used, why) = open_live_mic_with(Some("gone"), MicProcessing::Raw, |id, p| {
        opened.borrow_mut().push((id.map(str::to_owned), p));
        match id {
            Some(_) => Err(CaptureError::DeviceGone { what: "microphone" }),
            None => Ok(None::<String>),
        }
    })
    .unwrap();
    assert!(
        matches!(chosen, Chosen::Default { stream: None, ref error } if error.is_device_gone()),
        "{chosen:?}"
    );
    assert_eq!((used, why), (MicProcessing::Raw, None));
    // Gone, and the default device refuses raw: the default device's processed stream, the
    // gone choice still reported.
    let (chosen, used, why) =
        open_live_mic_with(Some("gone"), MicProcessing::Raw, |id, p| match (id, p) {
            (Some(_), _) => Err(CaptureError::DeviceGone { what: "microphone" }),
            (None, MicProcessing::Raw) => Err(refused()),
            (None, MicProcessing::Default) => Ok(None::<String>),
        })
        .unwrap();
    assert!(
        matches!(chosen, Chosen::Default { stream: None, .. }),
        "{chosen:?}"
    );
    assert_eq!(used, MicProcessing::Default);
    assert!(why.is_some());
    // No choice and the default processing: one open, no fallback.
    let (chosen, used, why) =
        open_live_mic_with(None, MicProcessing::Default, |_, _| Ok(7)).unwrap();
    assert!(matches!(chosen, Chosen::Asked(7)));
    assert_eq!((used, why), (MicProcessing::Default, None));
}

#[test]
fn capture_loop_a_raw_refusal_is_an_open_failure_that_is_not_a_lost_device() {
    let refused = CaptureError::RawMic(RawRefusal::NotSupported {
        device_name: "Fake mic".into(),
    });
    assert!(!refused.is_device_gone());
    assert!(!refused.is_device_loss());
    // What the re-open's log entry holds: the refusal's own text.
    assert_eq!(refused.detail(), refused.to_string());
    assert!(refused.detail().contains("cannot be opened raw"));
}

/// Every `.rs` file under `dir`, build output left out. Fails closed: a folder or entry that
/// cannot be read panics rather than leave files unscanned.
fn rust_files(dir: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|e| panic!("cannot read an entry of {}: {e}", dir.display()));
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && name != "node_modules" && name != ".git" {
                rust_files(&path, found);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
}

/// The text of a scanned source file. Fails closed: an unreadable file panics, never reads as
/// a clean one.
fn read_source(file: &std::path::Path) -> String {
    std::fs::read_to_string(file).unwrap_or_else(|e| panic!("cannot read {}: {e}", file.display()))
}

#[test]
fn raw_mic_source_scan_fails_closed_on_what_it_cannot_read() {
    let missing = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-folder-t12-32");
    assert!(!missing.exists());
    let listed = std::panic::catch_unwind(|| rust_files(&missing, &mut Vec::new()));
    assert!(
        listed.is_err(),
        "a folder that cannot be listed fails the scan"
    );
    let read = std::panic::catch_unwind(|| read_source(&missing.join("lib.rs")));
    assert!(read.is_err(), "a file that cannot be read fails the scan");
}

/// T12.32: the raw mic is a measurement switch of the raw recorder and the developer CLI.
/// T12.36 adds one path: the Audio page's setting, which only the runtime's audio settings
/// module turns into the capture selection's request. Nothing else in the workspace (the rest
/// of the runtime, the app, xtask) names the request, so no other session, check or sample can
/// ask for it, and that module names nothing of the raw recorder's refusals.
#[test]
fn raw_mic_is_named_only_by_the_capture_crate_and_the_cli() {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = here.parent().and_then(|p| p.parent()).expect("repo root");
    let cli = root.join("crates").join("earshot-cli");
    let mut files = Vec::new();
    for dir in ["crates", "apps", "xtask"] {
        rust_files(&root.join(dir), &mut files);
    }
    assert!(
        files.iter().any(|f| f.starts_with(&cli)),
        "the scan found the workspace"
    );
    // A raw request cannot be written without naming its type (`MicProcessing::Raw`; the
    // default is all `Default::default()` gives), so these cover every way to ask.
    let names = [
        "MicProcessing",
        "mic_processing",
        "RawRefusal",
        "raw_refusal",
        "STREAMOPTIONS_RAW",
    ];
    // The one path of T12.36: the setting's module and its tests, which may name the request
    // and the selection's field, nothing else.
    let settings_dir = root.join("crates").join("earshot-runtime").join("src");
    let settings = settings_dir.join("audio_settings.rs");
    let settings_tests = settings_dir.join("audio_settings_tests.rs");
    let path_names = ["MicProcessing", "mic_processing"];
    let mut naming = Vec::new();
    let mut seen_settings = 0;
    for file in files
        .iter()
        .filter(|f| !f.starts_with(here) && !f.starts_with(&cli))
    {
        let text = read_source(file);
        let allowed: &[&str] = if *file == settings || *file == settings_tests {
            seen_settings += 1;
            &path_names
        } else {
            &[]
        };
        if let Some(name) = names
            .iter()
            .find(|n| text.contains(**n) && !allowed.contains(n))
        {
            naming.push(format!("{}: {name}", file.display()));
        }
    }
    assert!(naming.is_empty(), "outside the recorder: {naming:?}");
    assert_eq!(seen_settings, 2, "the scan found the settings module");
    // The setting is turned into a raw request in exactly one place.
    let text = read_source(&settings);
    assert_eq!(text.matches("MicProcessing::Raw").count(), 1);
    assert_eq!(text.matches("mic_processing:").count(), 1);
}

/// T12.32 and T12.36: inside this crate a raw mic is asked for only by [`record_raw`] (its
/// argument, refused rather than replaced) and by the live source from its selection's
/// request (`DeviceSelection::mic_processing`, falling back to the default stream). The live
/// source never asks for raw on its own, and only its mic falls back.
#[test]
fn raw_mic_is_made_only_by_the_raw_recorder() {
    let source = include_str!("../wasapi.rs");
    let shipped = source
        .split_once("\n#[cfg(test)]")
        .map_or(source, |(code, _)| code);
    let between = |from: &str, to: &str| {
        let start = shipped.find(from).unwrap_or_else(|| panic!("no {from}"));
        let end = start
            + shipped[start..]
                .find(to)
                .unwrap_or_else(|| panic!("no {to}"));
        &shipped[start..end]
    };
    // Everything a live session runs: the source, its thread and its capture.
    let live = between("pub struct WasapiSource", "pub struct CtrlC");
    // Its mic's processing comes only from the selection, and it never writes raw itself
    // (the one mention is the comparison that reports what was opened).
    assert_eq!(live.matches("devices.mic_processing").count(), 1);
    assert_eq!(live.matches("MicProcessing::Raw").count(), 1);
    assert_eq!(live.matches("== MicProcessing::Raw").count(), 1);
    assert!(!live.contains("open_with("));
    assert_eq!(live.matches(".with_processing(").count(), 1);
    assert_eq!(live.matches(".with_raw_fallback(").count(), 1);
    // The shared default openers pass the default processing.
    let openers = between("fn open_chosen", "fn open_chosen_as");
    assert!(openers.contains("MicProcessing::Default"));
    assert!(!openers.contains("MicProcessing::Raw"));
    assert_eq!(
        shipped.matches(".with_processing(").count(),
        2,
        "two callers: record_raw and the live source"
    );
    assert_eq!(
        shipped.matches(".with_raw_fallback(").count(),
        1,
        "only the live source falls back"
    );
    // One caller each of the shared opener (the default openers) and of the live mic's opener
    // (the live source); a definition, `fn name<'com>(`, does not match.
    assert_eq!(shipped.matches("open_chosen_as(").count(), 1);
    assert_eq!(
        between("fn open_chosen<", "fn open_chosen_as")
            .matches("open_chosen_as(")
            .count(),
        1
    );
    assert_eq!(shipped.matches("open_live_mic(").count(), 1);
    assert_eq!(live.matches("open_live_mic(").count(), 1);
    // Streams are opened with a processing in four places: the loop's re-open, which passes
    // the track's own, the chosen-endpoint opener, the live mic's opener and record_raw.
    assert_eq!(shipped.matches("CaptureStream::open_with(").count(), 4);
    let recorder = between("pub fn record_raw", "struct Started");
    assert_eq!(recorder.matches("CaptureStream::open_with(").count(), 1);
    assert_eq!(recorder.matches(".with_processing(").count(), 1);
    assert!(!recorder.contains("with_raw_fallback"));
    assert!(!recorder.contains("open_live_mic"));
}
