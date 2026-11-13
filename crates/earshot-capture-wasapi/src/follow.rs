//! Device following: decides when a capture stream must be re-opened, from
//! default-device notifications (`IMMNotificationClient::OnDefaultDeviceChanged`)
//! and stream errors. Platform independent; the WASAPI layer feeds it events and
//! performs the re-open.

use serde::Serialize;

use crate::timeline::QPC_UNITS_PER_SEC;

/// While a stream has no device, a re-open is retried at most this often (QPC units).
pub const RETRY_INTERVAL_QPC: u64 = QPC_UNITS_PER_SEC;

/// How long a chosen endpoint (T7.4i) that keeps failing to open during a capture is
/// retried (every [`RETRY_INTERVAL_QPC`], silence recorded meanwhile) before the stream
/// moves to the default endpoint for the rest of the capture: 5 s. Long enough for Windows
/// to bring back an endpoint after a format change in Sound settings, a Bluetooth switch
/// between its listening and hands-free profiles, or another app briefly holding it
/// exclusively; short enough that a device that was unplugged costs only a few seconds. A
/// judgement, not measured.
pub const CHOSEN_DEVICE_GRACE_QPC: u64 = 5 * QPC_UNITS_PER_SEC;

/// `AUDCLNT_E_DEVICE_INVALIDATED`: the endpoint was removed or reconfigured.
pub const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;
/// `AUDCLNT_E_SERVICE_NOT_RUNNING`: the audio service stopped.
pub const AUDCLNT_E_SERVICE_NOT_RUNNING: i32 = 0x8889_0010_u32 as i32;
/// `AUDCLNT_E_RESOURCES_INVALIDATED`: the stream's resources were invalidated.
pub const AUDCLNT_E_RESOURCES_INVALIDATED: i32 = 0x8889_0026_u32 as i32;

/// True for stream errors that mean "this device went away; re-open", as opposed to
/// programming or format errors that should end the capture.
pub fn is_device_loss(hresult: i32) -> bool {
    matches!(
        hresult,
        AUDCLNT_E_DEVICE_INVALIDATED
            | AUDCLNT_E_SERVICE_NOT_RUNNING
            | AUDCLNT_E_RESOURCES_INVALIDATED
    )
}

/// How long a track whose stream failed with an error that is neither a device loss nor a
/// programming fault (T9.21) is re-opened, every [`RETRY_INTERVAL_QPC`] with silence recorded
/// meanwhile, before that track ends: 10 s since the first such error. Twice
/// [`CHOSEN_DEVICE_GRACE_QPC`], so a chosen endpoint is given up for the default first and the
/// default still gets its own 5 s. A stream that runs this long without an error ends the run of
/// errors. Which error codes occur on real machines is not known; a judgement, not measured.
pub const STREAM_ERROR_GRACE_QPC: u64 = 2 * CHOSEN_DEVICE_GRACE_QPC;

/// `E_POINTER`.
pub const E_POINTER: i32 = 0x8000_4003_u32 as i32;
/// `E_INVALIDARG`.
pub const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
/// `AUDCLNT_E_NOT_INITIALIZED`: the stream was used before `Initialize`.
pub const AUDCLNT_E_NOT_INITIALIZED: i32 = 0x8889_0001_u32 as i32;
/// `AUDCLNT_E_ALREADY_INITIALIZED`.
pub const AUDCLNT_E_ALREADY_INITIALIZED: i32 = 0x8889_0002_u32 as i32;
/// `AUDCLNT_E_WRONG_ENDPOINT_TYPE`: a capture call on a render stream or the reverse.
pub const AUDCLNT_E_WRONG_ENDPOINT_TYPE: i32 = 0x8889_0003_u32 as i32;
/// `AUDCLNT_E_NOT_STOPPED`: a call that needs a stopped stream.
pub const AUDCLNT_E_NOT_STOPPED: i32 = 0x8889_0005_u32 as i32;
/// `AUDCLNT_E_OUT_OF_ORDER`: `GetBuffer` before the previous `ReleaseBuffer`.
pub const AUDCLNT_E_OUT_OF_ORDER: i32 = 0x8889_0007_u32 as i32;
/// `AUDCLNT_E_INVALID_SIZE`: `ReleaseBuffer` with another frame count than `GetBuffer` gave.
pub const AUDCLNT_E_INVALID_SIZE: i32 = 0x8889_0009_u32 as i32;
/// `AUDCLNT_E_BUFFER_OPERATION_PENDING`: a buffer call while another is outstanding.
pub const AUDCLNT_E_BUFFER_OPERATION_PENDING: i32 = 0x8889_000B_u32 as i32;
/// `AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED`.
pub const AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED: i32 = 0x8889_0011_u32 as i32;
/// `AUDCLNT_E_EVENTHANDLE_NOT_SET`: an event-driven stream started without its event.
pub const AUDCLNT_E_EVENTHANDLE_NOT_SET: i32 = 0x8889_0014_u32 as i32;

/// Stream errors that mean the capture code itself is wrong (an invalid argument, or a call
/// in the wrong state): they end the capture at once instead of being re-opened (T9.21),
/// because a re-open would only repeat the mistake.
pub const PROGRAMMING_FAULTS: [i32; 11] = [
    E_POINTER,
    E_INVALIDARG,
    AUDCLNT_E_NOT_INITIALIZED,
    AUDCLNT_E_ALREADY_INITIALIZED,
    AUDCLNT_E_WRONG_ENDPOINT_TYPE,
    AUDCLNT_E_NOT_STOPPED,
    AUDCLNT_E_OUT_OF_ORDER,
    AUDCLNT_E_INVALID_SIZE,
    AUDCLNT_E_BUFFER_OPERATION_PENDING,
    AUDCLNT_E_EVENTHANDLE_NOT_EXPECTED,
    AUDCLNT_E_EVENTHANDLE_NOT_SET,
];

/// True for a stream error in [`PROGRAMMING_FAULTS`].
pub fn is_programming_fault(hresult: i32) -> bool {
    PROGRAMMING_FAULTS.contains(&hresult)
}

/// The detail logged when a track ends after [`STREAM_ERROR_GRACE_QPC`] of stream errors: the
/// error only as a number, never Windows' message text.
pub fn track_ended_detail(hresult: i32) -> String {
    format!("stream error {:#010x}; the track ended", hresult as u32)
}

/// Data-flow direction of an endpoint (`EDataFlow`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// `eRender`.
    Render,
    /// `eCapture`.
    Capture,
}

/// Endpoint role (`ERole`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `eConsole`.
    Console,
    /// `eMultimedia`.
    Multimedia,
    /// `eCommunications`.
    Communications,
}

/// One `OnDefaultDeviceChanged` notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultDeviceChange {
    /// Direction of the changed default.
    pub flow: Flow,
    /// Role of the changed default.
    pub role: Role,
    /// New default endpoint id, `None` when no endpoint is left for this role.
    pub device_id: Option<String>,
}

/// Why a stream was re-opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReopenReason {
    /// The default endpoint for the stream's flow and role changed.
    DefaultDeviceChanged,
    /// The stream reported that its device was lost.
    StreamError,
    /// Periodic retry while the stream has no device.
    Retry,
    /// A known call app plays on another output, and the track moved there (T6.15).
    CallOutputMoved,
}

/// Re-open decisions for one capture stream.
#[derive(Clone, Debug)]
pub struct StreamFollower {
    flow: Flow,
    role: Role,
    follows_default: bool,
    /// On an endpoint the user chose (T7.4i), until it is given up.
    chosen: bool,
    /// When the chosen endpoint first failed to open since it was last open.
    chosen_failing_since: Option<u64>,
    current_id: Option<String>,
    lost: bool,
    pending: Option<ReopenReason>,
    last_attempt_qpc: Option<u64>,
    /// When the current run of stream errors (T9.21) began.
    errors_since: Option<u64>,
    /// The last stream error or failed re-open of that run.
    last_failure_qpc: u64,
    /// The run of stream errors outlasted [`STREAM_ERROR_GRACE_QPC`]: the track has ended.
    ended: bool,
}

impl StreamFollower {
    /// Follower for a stream on the default endpoint of `flow` / `role`. With
    /// `follows_default == false` (process loopback, which is device independent)
    /// default-device changes are ignored and only stream errors cause a re-open.
    pub fn new(flow: Flow, role: Role, follows_default: bool) -> Self {
        Self {
            flow,
            role,
            follows_default,
            chosen: false,
            chosen_failing_since: None,
            current_id: None,
            lost: false,
            pending: None,
            last_attempt_qpc: None,
            errors_since: None,
            last_failure_qpc: 0,
            ended: false,
        }
    }

    /// Follower for a stream on an endpoint the user chose (T7.4i): default-device changes
    /// are ignored, and a re-open that keeps failing for [`CHOSEN_DEVICE_GRACE_QPC`] gives
    /// the endpoint up for the default ([`give_up_chosen`](Self::give_up_chosen)).
    pub fn chosen(flow: Flow, role: Role) -> Self {
        Self {
            chosen: true,
            ..Self::new(flow, role, false)
        }
    }

    /// The stream is open on `device_id` (`None` when the endpoint has no id).
    pub fn opened(&mut self, device_id: Option<String>) {
        self.chosen_failing_since = None;
        self.current_id = device_id;
        self.lost = false;
        self.pending = None;
    }

    /// A re-open attempt at `now_qpc` failed: the stream has no device until a later
    /// notification or retry.
    pub fn open_failed(&mut self, now_qpc: u64) {
        self.lost = true;
        self.pending = None;
        self.last_attempt_qpc = Some(now_qpc);
        if self.errors_since.is_some() {
            self.failed_in_run(now_qpc);
        }
    }

    /// The running stream failed at `now_qpc` with an error that is neither a device loss nor
    /// a programming fault (T9.21): treated as a lost stream and re-opened, but only for
    /// [`STREAM_ERROR_GRACE_QPC`] from the first such error. Errors, and re-opens failing for
    /// any reason, until then belong to the same run; once it has lasted that long the track
    /// has [`ended`](Self::ended).
    pub fn on_unknown_stream_error(&mut self, now_qpc: u64) {
        self.errors_since.get_or_insert(now_qpc);
        self.on_stream_error();
        self.failed_in_run(now_qpc);
    }

    /// A drain of the open stream succeeded at `now_qpc`: once it has run for
    /// [`STREAM_ERROR_GRACE_QPC`] since the run's last failure, the run of stream errors is
    /// over and a later error starts a new one.
    pub fn stream_ran(&mut self, now_qpc: u64) {
        if self.errors_since.is_some()
            && !self.lost
            && now_qpc.saturating_sub(self.last_failure_qpc) >= STREAM_ERROR_GRACE_QPC
        {
            self.errors_since = None;
        }
    }

    fn failed_in_run(&mut self, now_qpc: u64) {
        self.last_failure_qpc = now_qpc;
        let since = self.errors_since.unwrap_or(now_qpc);
        if now_qpc.saturating_sub(since) >= STREAM_ERROR_GRACE_QPC {
            self.ended = true;
            self.lost = true;
            self.pending = None;
        }
    }

    /// The track has ended after a run of stream errors (T9.21): it is never re-opened again,
    /// whatever happens to the default device.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// After [`open_failed`](Self::open_failed) at `now_qpc`: whether the chosen endpoint
    /// is given up. It is once it has failed every re-open for [`CHOSEN_DEVICE_GRACE_QPC`]
    /// since its first failure (retried every [`RETRY_INTERVAL_QPC`] until then). From then
    /// on the stream is on the default endpoint of its flow and role and follows it like any
    /// default stream, and a re-open onto it is due at once. Always false for a stream that
    /// is not (or no longer) on a chosen endpoint.
    pub fn give_up_chosen(&mut self, now_qpc: u64) -> bool {
        if !self.chosen || self.ended {
            return false;
        }
        let since = *self.chosen_failing_since.get_or_insert(now_qpc);
        if now_qpc.saturating_sub(since) < CHOSEN_DEVICE_GRACE_QPC {
            return false;
        }
        self.chosen = false;
        self.chosen_failing_since = None;
        self.follows_default = true;
        self.pending = Some(ReopenReason::Retry);
        true
    }

    /// Whether default-device changes re-open this stream.
    pub fn follows_default(&self) -> bool {
        self.follows_default
    }

    /// The running stream reported a device loss.
    pub fn on_stream_error(&mut self) {
        self.lost = true;
        self.pending = Some(ReopenReason::StreamError);
    }

    /// Feeds one notification. Only changes for this stream's flow and role that name
    /// a different endpoint (or a lost stream) request a re-open; Windows sends one
    /// notification per role, and repeats are ignored.
    pub fn on_default_changed(&mut self, change: &DefaultDeviceChange) {
        if self.ended
            || !self.follows_default
            || change.flow != self.flow
            || change.role != self.role
        {
            return;
        }
        if self.lost || change.device_id != self.current_id {
            self.pending = Some(ReopenReason::DefaultDeviceChanged);
        }
    }

    /// Takes the pending re-open decision at `now_qpc`, if any. A lost stream with no
    /// pending event is retried every [`RETRY_INTERVAL_QPC`].
    pub fn take_reopen(&mut self, now_qpc: u64) -> Option<ReopenReason> {
        if self.ended {
            return None;
        }
        if let Some(r) = self.pending.take() {
            return Some(r);
        }
        let due = self
            .last_attempt_qpc
            .is_none_or(|t| now_qpc.saturating_sub(t) >= RETRY_INTERVAL_QPC);
        (self.lost && due).then_some(ReopenReason::Retry)
    }

    /// The stream currently has no device.
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    /// Endpoint id the stream is open on.
    pub fn current_id(&self) -> Option<&str> {
        self.current_id.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(flow: Flow, role: Role, id: Option<&str>) -> DefaultDeviceChange {
        DefaultDeviceChange {
            flow,
            role,
            device_id: id.map(str::to_owned),
        }
    }

    fn mic() -> StreamFollower {
        let mut f = StreamFollower::new(Flow::Capture, Role::Console, true);
        f.opened(Some("mic-a".into()));
        f
    }

    #[test]
    fn steady_state_needs_no_reopen() {
        let mut f = mic();
        assert_eq!(f.take_reopen(0), None);
        assert_eq!(f.take_reopen(100 * RETRY_INTERVAL_QPC), None);
        assert!(!f.is_lost());
        assert_eq!(f.current_id(), Some("mic-a"));
    }

    #[test]
    fn default_change_for_own_flow_and_role_reopens_once() {
        let mut f = mic();
        f.on_default_changed(&change(Flow::Capture, Role::Console, Some("mic-b")));
        assert_eq!(f.take_reopen(0), Some(ReopenReason::DefaultDeviceChanged));
        assert_eq!(f.take_reopen(0), None, "decision is taken once");
        f.opened(Some("mic-b".into()));
        // Windows repeats the notification for the same endpoint: no second re-open.
        f.on_default_changed(&change(Flow::Capture, Role::Console, Some("mic-b")));
        assert_eq!(f.take_reopen(1), None);
    }

    #[test]
    fn other_flow_role_or_same_device_is_ignored() {
        let mut f = mic();
        f.on_default_changed(&change(Flow::Render, Role::Console, Some("spk")));
        f.on_default_changed(&change(Flow::Capture, Role::Communications, Some("mic-b")));
        f.on_default_changed(&change(Flow::Capture, Role::Multimedia, Some("mic-b")));
        f.on_default_changed(&change(Flow::Capture, Role::Console, Some("mic-a")));
        assert_eq!(f.take_reopen(0), None);
    }

    #[test]
    fn communications_role_follows_communications_default() {
        let mut f = StreamFollower::new(Flow::Capture, Role::Communications, true);
        f.opened(Some("headset".into()));
        f.on_default_changed(&change(Flow::Capture, Role::Console, Some("mic-b")));
        assert_eq!(f.take_reopen(0), None);
        f.on_default_changed(&change(Flow::Capture, Role::Communications, Some("mic-b")));
        assert_eq!(f.take_reopen(0), Some(ReopenReason::DefaultDeviceChanged));
    }

    #[test]
    fn process_loopback_ignores_render_changes_but_recovers_from_errors() {
        let mut f = StreamFollower::new(Flow::Render, Role::Console, false);
        f.opened(None);
        f.on_default_changed(&change(Flow::Render, Role::Console, Some("headphones")));
        assert_eq!(f.take_reopen(0), None);
        f.on_stream_error();
        assert!(f.is_lost());
        assert_eq!(f.take_reopen(0), Some(ReopenReason::StreamError));
    }

    #[test]
    fn default_render_loopback_follows_render_changes() {
        let mut f = StreamFollower::new(Flow::Render, Role::Console, true);
        f.opened(Some("speakers".into()));
        f.on_default_changed(&change(Flow::Render, Role::Console, Some("headphones")));
        assert_eq!(f.take_reopen(0), Some(ReopenReason::DefaultDeviceChanged));
    }

    #[test]
    fn lost_stream_retries_at_interval_until_reopened() {
        let mut f = mic();
        f.on_stream_error();
        assert_eq!(f.take_reopen(10), Some(ReopenReason::StreamError));
        f.open_failed(10);
        assert!(f.is_lost());
        assert_eq!(f.take_reopen(10 + RETRY_INTERVAL_QPC - 1), None);
        assert_eq!(
            f.take_reopen(10 + RETRY_INTERVAL_QPC),
            Some(ReopenReason::Retry)
        );
        f.open_failed(10 + RETRY_INTERVAL_QPC);
        // A notification while lost re-opens immediately, even for the same id.
        f.on_default_changed(&change(Flow::Capture, Role::Console, Some("mic-a")));
        assert_eq!(
            f.take_reopen(11 + RETRY_INTERVAL_QPC),
            Some(ReopenReason::DefaultDeviceChanged)
        );
        f.opened(Some("mic-a".into()));
        assert!(!f.is_lost());
        assert_eq!(f.take_reopen(100 * RETRY_INTERVAL_QPC), None);
    }

    #[test]
    fn default_removed_requests_reopen_which_then_fails() {
        let mut f = mic();
        f.on_default_changed(&change(Flow::Capture, Role::Console, None));
        assert_eq!(f.take_reopen(0), Some(ReopenReason::DefaultDeviceChanged));
        f.open_failed(0);
        assert!(f.is_lost());
    }

    /// Drives a lost chosen stream: every due re-open fails, until `until`; returns the
    /// times of the attempts and whether the last one gave the endpoint up.
    fn fail_chosen_until(f: &mut StreamFollower, start: u64, until: u64) -> (Vec<u64>, bool) {
        let mut attempts = Vec::new();
        let mut now = start;
        while now <= until {
            if f.take_reopen(now).is_some() {
                attempts.push(now);
                f.open_failed(now);
                if f.give_up_chosen(now) {
                    return (attempts, true);
                }
            }
            now += RETRY_INTERVAL_QPC / 4;
        }
        (attempts, false)
    }

    #[test]
    fn chosen_endpoint_ignores_default_changes() {
        // A chosen mic (T7.4i) stays put when the Windows default changes.
        let mut f = StreamFollower::chosen(Flow::Capture, Role::Console);
        f.opened(Some("chosen".into()));
        assert!(!f.follows_default());
        f.on_default_changed(&change(Flow::Capture, Role::Console, Some("mic-b")));
        assert_eq!(f.take_reopen(0), None);
        // A stream that is not on a chosen endpoint never gives anything up.
        let mut d = mic();
        d.open_failed(0);
        assert!(!d.give_up_chosen(100 * CHOSEN_DEVICE_GRACE_QPC));
        let mut p = StreamFollower::new(Flow::Render, Role::Console, false);
        p.open_failed(0);
        assert!(!p.give_up_chosen(100 * CHOSEN_DEVICE_GRACE_QPC));
    }

    #[test]
    fn chosen_endpoint_is_retried_for_the_grace_period_then_given_up() {
        let t0 = 7 * QPC_UNITS_PER_SEC;
        let mut f = StreamFollower::chosen(Flow::Capture, Role::Console);
        f.opened(Some("chosen".into()));
        f.on_stream_error();
        // Retried every second from the first failure; not given up before the grace ends.
        let (attempts, gave_up) = fail_chosen_until(&mut f, t0, t0 + CHOSEN_DEVICE_GRACE_QPC - 1);
        assert!(!gave_up);
        let expected: Vec<u64> = (0..CHOSEN_DEVICE_GRACE_QPC / RETRY_INTERVAL_QPC)
            .map(|k| t0 + k * RETRY_INTERVAL_QPC)
            .collect();
        assert_eq!(attempts, expected);
        assert!(!f.follows_default());
        // The exact boundary: a failure one tick before the grace ends keeps it ...
        let mut edge = f.clone();
        edge.open_failed(t0 + CHOSEN_DEVICE_GRACE_QPC - 1);
        assert!(!edge.give_up_chosen(t0 + CHOSEN_DEVICE_GRACE_QPC - 1));
        // ... and one at the end gives it up: the default is opened at once and followed.
        f.open_failed(t0 + CHOSEN_DEVICE_GRACE_QPC);
        assert!(f.give_up_chosen(t0 + CHOSEN_DEVICE_GRACE_QPC));
        assert!(f.follows_default());
        assert_eq!(
            f.take_reopen(t0 + CHOSEN_DEVICE_GRACE_QPC),
            Some(ReopenReason::Retry)
        );
        // Given up once: a later failure (of the default) gives nothing up again.
        f.open_failed(t0 + CHOSEN_DEVICE_GRACE_QPC);
        assert!(!f.give_up_chosen(t0 + 10 * CHOSEN_DEVICE_GRACE_QPC));
        f.opened(Some("mic-b".into()));
        f.on_default_changed(&change(Flow::Capture, Role::Console, Some("mic-c")));
        assert_eq!(f.take_reopen(t0), Some(ReopenReason::DefaultDeviceChanged));
    }

    #[test]
    fn chosen_endpoint_back_within_the_grace_period_stays_chosen() {
        let mut f = StreamFollower::chosen(Flow::Capture, Role::Console);
        f.opened(Some("chosen".into()));
        f.on_stream_error();
        let (_, gave_up) = fail_chosen_until(&mut f, 0, 3 * RETRY_INTERVAL_QPC);
        assert!(!gave_up);
        // It opens again: the grace starts over at its next loss.
        f.opened(Some("chosen".into()));
        assert!(!f.follows_default());
        let later = 100 * CHOSEN_DEVICE_GRACE_QPC;
        f.on_stream_error();
        assert_eq!(f.take_reopen(later), Some(ReopenReason::StreamError));
        f.open_failed(later);
        assert!(
            !f.give_up_chosen(later),
            "a new loss starts a new grace period"
        );
    }

    /// Fails every due re-open of a stream that hit an unknown error at `start`, until it
    /// ends or `until` passes; returns the attempt times and whether it ended.
    fn fail_errors_until(f: &mut StreamFollower, start: u64, until: u64) -> (Vec<u64>, bool) {
        f.on_unknown_stream_error(start);
        let mut attempts = Vec::new();
        let mut now = start;
        while now <= until {
            if f.take_reopen(now).is_some() {
                attempts.push(now);
                f.open_failed(now);
            }
            if f.ended() {
                return (attempts, true);
            }
            now += RETRY_INTERVAL_QPC / 4;
        }
        (attempts, false)
    }

    #[test]
    fn unknown_stream_errors_are_retried_for_the_grace_then_the_track_ends() {
        let t0 = 3 * QPC_UNITS_PER_SEC;
        let mut f = mic();
        let (attempts, ended) = fail_errors_until(&mut f, t0, t0 + STREAM_ERROR_GRACE_QPC - 1);
        assert!(!ended);
        let expected: Vec<u64> = (0..STREAM_ERROR_GRACE_QPC / RETRY_INTERVAL_QPC)
            .map(|k| t0 + k * RETRY_INTERVAL_QPC)
            .collect();
        assert_eq!(attempts, expected, "re-opened every retry interval");
        // The retry due at the end of the grace fails too: the track ends.
        let end = t0 + STREAM_ERROR_GRACE_QPC;
        assert_eq!(f.take_reopen(end), Some(ReopenReason::Retry));
        f.open_failed(end);
        assert!(f.ended());
        assert!(f.is_lost());
        // Never re-opened again, whatever the default does.
        f.on_default_changed(&change(Flow::Capture, Role::Console, Some("mic-b")));
        assert_eq!(f.take_reopen(end + 100 * RETRY_INTERVAL_QPC), None);
    }

    #[test]
    fn unknown_stream_errors_that_reopen_and_fail_again_are_one_run() {
        // Each re-open works, but the new stream fails again a second later.
        let mut f = mic();
        let mut now = 0;
        loop {
            f.on_unknown_stream_error(now);
            if f.ended() {
                break;
            }
            assert_eq!(f.take_reopen(now), Some(ReopenReason::StreamError));
            f.opened(Some("mic-a".into()));
            f.stream_ran(now + RETRY_INTERVAL_QPC / 2);
            now += RETRY_INTERVAL_QPC;
            assert!(now <= STREAM_ERROR_GRACE_QPC, "never ended");
        }
        assert_eq!(now, STREAM_ERROR_GRACE_QPC);
    }

    #[test]
    fn a_stream_that_runs_clean_for_the_grace_starts_a_new_run() {
        let mut f = mic();
        f.on_unknown_stream_error(0);
        f.take_reopen(0);
        f.opened(Some("mic-a".into()));
        f.stream_ran(STREAM_ERROR_GRACE_QPC - 1);
        // An error just inside the grace still belongs to the first run ...
        let mut inside = f.clone();
        inside.on_unknown_stream_error(STREAM_ERROR_GRACE_QPC);
        assert!(inside.ended());
        // ... but after a clean run of the whole grace it starts over.
        f.stream_ran(STREAM_ERROR_GRACE_QPC);
        f.on_unknown_stream_error(STREAM_ERROR_GRACE_QPC + 1);
        assert!(!f.ended());
        assert_eq!(
            f.take_reopen(STREAM_ERROR_GRACE_QPC + 1),
            Some(ReopenReason::StreamError)
        );
    }

    #[test]
    fn device_losses_alone_never_end_a_track() {
        let mut f = mic();
        f.on_stream_error();
        let mut now = 0;
        while now < 100 * STREAM_ERROR_GRACE_QPC {
            if f.take_reopen(now).is_some() {
                f.open_failed(now);
            }
            now += RETRY_INTERVAL_QPC;
        }
        assert!(!f.ended());
        assert!(f.take_reopen(now).is_some());
    }

    #[test]
    fn a_chosen_endpoint_is_given_up_before_its_stream_errors_end_the_track() {
        let mut f = StreamFollower::chosen(Flow::Capture, Role::Console);
        f.opened(Some("chosen".into()));
        f.on_unknown_stream_error(0);
        let mut now = 0;
        let mut gave_up_at = None;
        while !f.ended() {
            if f.take_reopen(now).is_some() {
                f.open_failed(now);
                if f.give_up_chosen(now) {
                    gave_up_at.get_or_insert(now);
                }
            }
            now += RETRY_INTERVAL_QPC / 4;
        }
        assert_eq!(gave_up_at, Some(CHOSEN_DEVICE_GRACE_QPC));
        assert!(f.follows_default());
        // The default kept failing too: the track ended at the first failure past the grace.
        let ended_at = now - RETRY_INTERVAL_QPC / 4;
        assert!(
            (STREAM_ERROR_GRACE_QPC..STREAM_ERROR_GRACE_QPC + RETRY_INTERVAL_QPC)
                .contains(&ended_at),
            "{ended_at}"
        );
    }

    #[test]
    fn programming_faults_are_exactly_the_listed_codes() {
        for code in PROGRAMMING_FAULTS {
            assert!(is_programming_fault(code), "{code:#x}");
            assert!(!is_device_loss(code), "{code:#x}");
        }
        for other in [
            AUDCLNT_E_DEVICE_INVALIDATED,
            AUDCLNT_E_SERVICE_NOT_RUNNING,
            AUDCLNT_E_RESOURCES_INVALIDATED,
            0x8000_4005_u32 as i32, // E_FAIL
            0x8007_000E_u32 as i32, // E_OUTOFMEMORY
            0x8889_0017_u32 as i32, // AUDCLNT_E_CPUUSAGE_EXCEEDED
            0x8889_0018_u32 as i32, // AUDCLNT_E_BUFFER_ERROR
            0,
        ] {
            assert!(!is_programming_fault(other), "{other:#x}");
        }
        assert_eq!(
            track_ended_detail(0x8889_0017_u32 as i32),
            "stream error 0x88890017; the track ended"
        );
    }

    #[test]
    fn classifies_device_loss_hresults() {
        assert!(is_device_loss(AUDCLNT_E_DEVICE_INVALIDATED));
        assert!(is_device_loss(AUDCLNT_E_SERVICE_NOT_RUNNING));
        assert!(is_device_loss(AUDCLNT_E_RESOURCES_INVALIDATED));
        assert!(!is_device_loss(0));
        assert!(!is_device_loss(0x8007_0057_u32 as i32)); // E_INVALIDARG
        assert!(!is_device_loss(0x8889_0008_u32 as i32)); // AUDCLNT_E_UNSUPPORTED_FORMAT
    }
}
