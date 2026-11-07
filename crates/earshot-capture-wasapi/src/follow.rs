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
    }

    /// After [`open_failed`](Self::open_failed) at `now_qpc`: whether the chosen endpoint
    /// is given up. It is once it has failed every re-open for [`CHOSEN_DEVICE_GRACE_QPC`]
    /// since its first failure (retried every [`RETRY_INTERVAL_QPC`] until then). From then
    /// on the stream is on the default endpoint of its flow and role and follows it like any
    /// default stream, and a re-open onto it is due at once. Always false for a stream that
    /// is not (or no longer) on a chosen endpoint.
    pub fn give_up_chosen(&mut self, now_qpc: u64) -> bool {
        if !self.chosen {
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
        if !self.follows_default || change.flow != self.flow || change.role != self.role {
            return;
        }
        if self.lost || change.device_id != self.current_id {
            self.pending = Some(ReopenReason::DefaultDeviceChanged);
        }
    }

    /// Takes the pending re-open decision at `now_qpc`, if any. A lost stream with no
    /// pending event is retried every [`RETRY_INTERVAL_QPC`].
    pub fn take_reopen(&mut self, now_qpc: u64) -> Option<ReopenReason> {
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
