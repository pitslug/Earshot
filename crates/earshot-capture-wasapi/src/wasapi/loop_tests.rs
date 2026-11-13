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
    ) -> Result<FakeStream, CaptureError> {
        self.platform.call(Call::Open(track_of(endpoint)))?;
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
