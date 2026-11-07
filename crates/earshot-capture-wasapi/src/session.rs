//! Loopback source selection: which process tree (if any) to capture with process
//! loopback, chosen from the audio sessions active on render endpoints.
//! Platform independent; the WASAPI layer supplies the session list.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::recording::{LoopbackMode, LoopbackSource};

/// What the user asked to capture as the loopback ("them") track.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LoopbackRequest {
    /// Pick a known call app with an active render session; fall back to the default
    /// render endpoint when none is found or process loopback is unavailable.
    #[default]
    Auto,
    /// Loopback of the default console render endpoint (the T1.0 behaviour).
    DefaultDevice,
    /// Process loopback of the process with this executable name (`zoom`, `zoom.exe`).
    ProcessName(String),
    /// Process loopback of this process id (and its child processes).
    Pid(u32),
    /// The default render endpoint at the start, then the output a known call app plays on
    /// ([`crate::call_output`], T6.15). A chosen output always wins and is not followed. Opt-in
    /// (`follow-call`) until a real call has proven it.
    FollowCallOutput,
}

/// What a live capture records (T6.27).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    /// A call: the microphone plus a loopback of the far end chosen by the request, the
    /// echo canceller's reference (two tracks, two-track speaker policy).
    Call(LoopbackRequest),
    /// A meeting in the room with no call audio: the microphone only (no loopback, so no
    /// echo canceller; one track, in-person speaker policy).
    InRoom,
}

impl CaptureMode {
    /// The loopback request of a call; `None` in the room.
    #[must_use]
    pub fn loopback(&self) -> Option<&LoopbackRequest> {
        match self {
            Self::Call(request) => Some(request),
            Self::InRoom => None,
        }
    }
}

/// `--loopback-process` value that is neither a keyword, a pid nor a usable name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseLoopbackRequestError(String);

impl fmt::Display for ParseLoopbackRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid loopback source {:?}: expected auto, default-device, a process name or a pid",
            self.0
        )
    }
}

impl std::error::Error for ParseLoopbackRequestError {}

impl FromStr for LoopbackRequest {
    type Err = ParseLoopbackRequestError;

    /// `auto`, `default-device` (case-insensitive), a decimal pid (> 0), or any other
    /// non-empty string as a process name.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        let err = || ParseLoopbackRequestError(s.to_owned());
        if t.is_empty() {
            return Err(err());
        }
        if t.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        if t.eq_ignore_ascii_case("default-device") {
            return Ok(Self::DefaultDevice);
        }
        if t.eq_ignore_ascii_case("follow-call") {
            return Ok(Self::FollowCallOutput);
        }
        if t.bytes().all(|b| b.is_ascii_digit()) {
            return match t.parse::<u32>() {
                Ok(pid) if pid > 0 => Ok(Self::Pid(pid)),
                _ => Err(err()),
            };
        }
        Ok(Self::ProcessName(t.to_owned()))
    }
}

impl fmt::Display for LoopbackRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::DefaultDevice => f.write_str("default-device"),
            Self::FollowCallOutput => f.write_str("follow-call"),
            Self::ProcessName(n) => f.write_str(n),
            Self::Pid(p) => write!(f, "{p}"),
        }
    }
}

/// One audio session on a render endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioSession {
    /// Owning process id (0 for the system sounds session).
    pub pid: u32,
    /// Executable name or full image path of the process; empty when unknown.
    pub process_name: String,
    /// `AudioSessionStateActive`: the session currently has a playing stream.
    pub active: bool,
}

/// One render session with its meter.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionPeak {
    /// The endpoint's friendly name.
    pub endpoint: String,
    /// The endpoint's id (`IMMDevice::GetId`); empty when unknown.
    pub endpoint_id: String,
    /// Owning process id.
    pub pid: u32,
    /// Executable name; empty when unknown.
    pub process_name: String,
    /// The session has a playing stream.
    pub active: bool,
    /// `IAudioMeterInformation::GetPeakValue` (0 to 1); `None` when it cannot be read.
    pub peak: Option<f32>,
}

/// How often a meeting reads the render sessions: 2 s, a judgement (not a measurement). A reading
/// walks every output's sessions, so often enough to notice a call but no tighter.
pub const CALL_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Whether a known call app has an active render session (T9.4).
#[must_use]
pub fn call_app_active(sessions: &[SessionPeak]) -> bool {
    sessions
        .iter()
        .any(|s| s.pid != 0 && s.active && known_call_app(&s.process_name).is_some())
}

/// A call application recognised by its executable names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallApp {
    /// Display label.
    pub label: &'static str,
    /// Lower-case executable names without `.exe`.
    pub exe_names: &'static [&'static str],
}

/// Known call apps in ranking order: dedicated call apps first, browsers (web
/// clients) last. Earlier entries win when several have an active session.
pub const KNOWN_CALL_APPS: &[CallApp] = &[
    CallApp {
        label: "Microsoft Teams",
        exe_names: &["ms-teams", "msteams", "teams"],
    },
    CallApp {
        label: "Zoom",
        exe_names: &["zoom"],
    },
    CallApp {
        label: "Webex",
        exe_names: &["webex", "ciscocollabhost", "atmgr"],
    },
    CallApp {
        label: "Slack",
        exe_names: &["slack"],
    },
    CallApp {
        label: "Discord",
        exe_names: &["discord"],
    },
    CallApp {
        label: "Skype",
        exe_names: &["skype", "skypeapp"],
    },
    CallApp {
        label: "Microsoft Edge",
        exe_names: &["msedge"],
    },
    CallApp {
        label: "Google Chrome",
        exe_names: &["chrome"],
    },
    CallApp {
        label: "Firefox",
        exe_names: &["firefox"],
    },
    CallApp {
        label: "Brave",
        exe_names: &["brave"],
    },
    CallApp {
        label: "Opera",
        exe_names: &["opera"],
    },
    CallApp {
        label: "Vivaldi",
        exe_names: &["vivaldi"],
    },
];

/// File name of `name` (after the last `\` or `/`), lower-case, without `.exe`.
pub fn normalize_process_name(name: &str) -> String {
    let base = name.rsplit(['\\', '/']).next().unwrap_or(name).trim();
    let lower = base.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_owned()
}

/// Rank (index in [`KNOWN_CALL_APPS`]) and entry of a known call app.
pub fn known_call_app(process_name: &str) -> Option<(usize, &'static CallApp)> {
    let n = normalize_process_name(process_name);
    KNOWN_CALL_APPS
        .iter()
        .enumerate()
        .find(|(_, app)| app.exe_names.contains(&n.as_str()))
}

/// Process tree chosen for process loopback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessTarget {
    /// Target process id (its child processes are included).
    pub pid: u32,
    /// Executable name as reported by the session, if known.
    pub process_name: Option<String>,
    /// Known call app label, if the process is one.
    pub app: Option<&'static str>,
}

/// Why the default render endpoint is captured instead of a process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    /// `default-device` was requested.
    Requested,
    /// `auto` found no known call app with an active render session.
    NoCallApp,
    /// Listing audio sessions failed.
    SessionQueryFailed(String),
    /// Process loopback could not be activated (needs Windows 10 2004 or later).
    ProcessLoopbackUnavailable(String),
}

impl fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Requested => f.write_str("requested"),
            Self::NoCallApp => f.write_str("no call app with an active audio session"),
            Self::SessionQueryFailed(e) => write!(f, "audio session query failed: {e}"),
            Self::ProcessLoopbackUnavailable(e) => write!(f, "process loopback unavailable: {e}"),
        }
    }
}

/// Loopback source decided from a request and the session list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopbackChoice {
    /// Process loopback of a process tree.
    Process(ProcessTarget),
    /// Loopback of the default console render endpoint.
    DefaultDevice(FallbackReason),
}

/// An explicit request that cannot be satisfied (never falls back silently).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectError {
    /// No audio session belongs to a process with this name.
    NoSessionForName(String),
    /// The target process does not exist (or has exited).
    NoSuchProcess(u32),
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSessionForName(n) => {
                write!(f, "no audio session belongs to a process named {n:?}")
            }
            Self::NoSuchProcess(pid) => write!(f, "no running process with pid {pid}"),
        }
    }
}

impl std::error::Error for SelectError {}

fn target(s: &AudioSession) -> ProcessTarget {
    ProcessTarget {
        pid: s.pid,
        process_name: (!s.process_name.is_empty()).then(|| s.process_name.clone()),
        app: known_call_app(&s.process_name).map(|(_, app)| app.label),
    }
}

/// Chooses the loopback source.
///
/// - `Auto`: among sessions that are active and belong to a known call app, the best
///   ranked app ([`KNOWN_CALL_APPS`] order) wins, ties broken by the lowest pid.
///   Nothing found -> default device ([`FallbackReason::NoCallApp`]).
/// - `ProcessName`: sessions whose normalised process name matches; active before
///   inactive, then lowest pid. Nothing found -> error.
/// - `Pid`: that pid, named from its session when one exists.
/// - `DefaultDevice` and `FollowCallOutput`: the default render endpoint (what follows the call
///   comes later, from the capture loop).
///
/// The system sounds session (pid 0) is never chosen.
pub fn choose_loopback(
    request: &LoopbackRequest,
    sessions: &[AudioSession],
) -> Result<LoopbackChoice, SelectError> {
    let real = || sessions.iter().filter(|s| s.pid != 0);
    match request {
        LoopbackRequest::DefaultDevice | LoopbackRequest::FollowCallOutput => {
            Ok(LoopbackChoice::DefaultDevice(FallbackReason::Requested))
        }
        LoopbackRequest::Auto => Ok(real()
            .filter(|s| s.active)
            .filter_map(|s| known_call_app(&s.process_name).map(|(rank, _)| (rank, s.pid, s)))
            .min_by_key(|&(rank, pid, _)| (rank, pid))
            .map(|(_, _, s)| LoopbackChoice::Process(target(s)))
            .unwrap_or(LoopbackChoice::DefaultDevice(FallbackReason::NoCallApp))),
        LoopbackRequest::ProcessName(name) => {
            let wanted = normalize_process_name(name);
            real()
                .filter(|s| normalize_process_name(&s.process_name) == wanted)
                .min_by_key(|s| (!s.active, s.pid))
                .map(|s| LoopbackChoice::Process(target(s)))
                .ok_or_else(|| SelectError::NoSessionForName(name.clone()))
        }
        LoopbackRequest::Pid(pid) => Ok(LoopbackChoice::Process(
            real()
                .filter(|s| s.pid == *pid)
                .find(|s| !s.process_name.is_empty())
                .map(target)
                .unwrap_or(ProcessTarget {
                    pid: *pid,
                    process_name: None,
                    app: None,
                }),
        )),
    }
}

impl LoopbackChoice {
    /// The `timestamps.json` description of this choice for `request`.
    pub fn source(&self, request: &LoopbackRequest) -> LoopbackSource {
        match self {
            Self::Process(t) => LoopbackSource {
                requested: request.to_string(),
                mode: LoopbackMode::Process,
                pid: Some(t.pid),
                process_name: t.process_name.clone(),
                app: t.app.map(str::to_owned),
                fallback_reason: None,
            },
            Self::DefaultDevice(reason) => LoopbackSource {
                requested: request.to_string(),
                mode: LoopbackMode::DefaultDevice,
                pid: None,
                process_name: None,
                app: None,
                fallback_reason: (*reason != FallbackReason::Requested).then(|| reason.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(pid: u32, name: &str, active: bool) -> SessionPeak {
        SessionPeak {
            endpoint: "Speakers".into(),
            endpoint_id: "{speakers}".into(),
            pid,
            process_name: name.into(),
            active,
            peak: Some(0.0),
        }
    }

    #[test]
    fn follow_call_output_starts_on_the_default_device_as_requested_even_with_a_call_app() {
        let sessions = [s(1, "zoom.exe", true)];
        let choice = choose_loopback(&LoopbackRequest::FollowCallOutput, &sessions).unwrap();
        assert_eq!(
            choice,
            LoopbackChoice::DefaultDevice(FallbackReason::Requested)
        );
        let source = choice.source(&LoopbackRequest::FollowCallOutput);
        assert_eq!(source.requested, "follow-call");
        assert_eq!(source.fallback_reason, None);
    }

    #[test]
    fn call_app_active_needs_a_known_app_with_an_active_session() {
        assert!(!call_app_active(&[]));
        assert!(!call_app_active(&[peak(10, "ms-teams.exe", false)]));
        assert!(!call_app_active(&[peak(10, "spotify.exe", true)]));
        assert!(!call_app_active(&[peak(0, "ms-teams.exe", true)]));
        assert!(call_app_active(&[
            peak(10, "spotify.exe", true),
            peak(11, "Zoom.exe", true)
        ]));
    }

    fn s(pid: u32, name: &str, active: bool) -> AudioSession {
        AudioSession {
            pid,
            process_name: name.into(),
            active,
        }
    }

    fn pid_of(c: &LoopbackChoice) -> Option<u32> {
        match c {
            LoopbackChoice::Process(t) => Some(t.pid),
            LoopbackChoice::DefaultDevice(_) => None,
        }
    }

    #[test]
    fn parses_requests() {
        assert_eq!("auto".parse(), Ok(LoopbackRequest::Auto));
        assert_eq!("AUTO".parse(), Ok(LoopbackRequest::Auto));
        assert_eq!("default-device".parse(), Ok(LoopbackRequest::DefaultDevice));
        assert_eq!("follow-call".parse(), Ok(LoopbackRequest::FollowCallOutput));
        assert_eq!("FOLLOW-CALL".parse(), Ok(LoopbackRequest::FollowCallOutput));
        assert_eq!("4242".parse(), Ok(LoopbackRequest::Pid(4242)));
        assert_eq!(
            "Zoom.exe".parse(),
            Ok(LoopbackRequest::ProcessName("Zoom.exe".into()))
        );
        assert_eq!(
            " ms-teams ".parse(),
            Ok(LoopbackRequest::ProcessName("ms-teams".into()))
        );
        assert!("".parse::<LoopbackRequest>().is_err());
        assert!("  ".parse::<LoopbackRequest>().is_err());
        assert!("0".parse::<LoopbackRequest>().is_err());
        assert!("99999999999".parse::<LoopbackRequest>().is_err());
    }

    #[test]
    fn capture_mode_has_a_loopback_only_for_a_call() {
        let call = CaptureMode::Call(LoopbackRequest::DefaultDevice);
        assert_eq!(call.loopback(), Some(&LoopbackRequest::DefaultDevice));
        assert_eq!(CaptureMode::InRoom.loopback(), None);
    }

    #[test]
    fn request_display_round_trips() {
        for r in [
            LoopbackRequest::Auto,
            LoopbackRequest::DefaultDevice,
            LoopbackRequest::FollowCallOutput,
            LoopbackRequest::Pid(7),
            LoopbackRequest::ProcessName("zoom".into()),
        ] {
            assert_eq!(r.to_string().parse(), Ok(r));
        }
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize_process_name("Zoom.exe"), "zoom");
        assert_eq!(
            normalize_process_name(r"C:\Program Files\WindowsApps\MSTeams_x\ms-teams.exe"),
            "ms-teams"
        );
        assert_eq!(normalize_process_name("/usr/bin/firefox"), "firefox");
        assert_eq!(normalize_process_name("chrome"), "chrome");
        assert_eq!(normalize_process_name(""), "");
    }

    #[test]
    fn recognises_known_apps_including_new_teams() {
        assert_eq!(
            known_call_app("ms-teams.exe").unwrap().1.label,
            "Microsoft Teams"
        );
        assert_eq!(
            known_call_app("Teams.exe").unwrap().1.label,
            "Microsoft Teams"
        );
        assert_eq!(known_call_app("Zoom.exe").unwrap().1.label, "Zoom");
        assert_eq!(known_call_app("Discord.exe").unwrap().1.label, "Discord");
        assert!(known_call_app("spotify.exe").is_none());
        assert!(known_call_app("").is_none());
        // Call apps outrank browsers.
        let rank = |n| known_call_app(n).unwrap().0;
        assert!(rank("slack.exe") < rank("msedge.exe"));
        assert!(rank("ms-teams.exe") < rank("zoom.exe"));
    }

    #[test]
    fn auto_prefers_call_app_over_browser_and_ignores_inactive_and_unknown() {
        let sessions = [
            s(0, "", true),
            s(100, "spotify.exe", true),
            s(200, "chrome.exe", true),
            s(300, "Zoom.exe", true),
            s(50, "ms-teams.exe", false),
        ];
        let c = choose_loopback(&LoopbackRequest::Auto, &sessions).unwrap();
        assert_eq!(
            c,
            LoopbackChoice::Process(ProcessTarget {
                pid: 300,
                process_name: Some("Zoom.exe".into()),
                app: Some("Zoom"),
            })
        );
    }

    #[test]
    fn auto_ranking_is_deterministic_regardless_of_order() {
        let mut sessions = vec![
            s(900, "chrome.exe", true),
            s(700, "chrome.exe", true),
            s(800, "msedge.exe", true),
        ];
        let first = choose_loopback(&LoopbackRequest::Auto, &sessions).unwrap();
        sessions.reverse();
        let second = choose_loopback(&LoopbackRequest::Auto, &sessions).unwrap();
        assert_eq!(first, second);
        // Edge ranks above Chrome.
        assert_eq!(pid_of(&first), Some(800));
        // Same app twice: lowest pid.
        let c = choose_loopback(
            &LoopbackRequest::Auto,
            &[s(900, "chrome.exe", true), s(700, "chrome.exe", true)],
        )
        .unwrap();
        assert_eq!(pid_of(&c), Some(700));
    }

    #[test]
    fn auto_falls_back_when_no_active_call_app() {
        for sessions in [
            vec![],
            vec![s(1, "spotify.exe", true)],
            vec![s(2, "zoom.exe", false)],
            vec![s(0, "zoom.exe", true)],
        ] {
            assert_eq!(
                choose_loopback(&LoopbackRequest::Auto, &sessions).unwrap(),
                LoopbackChoice::DefaultDevice(FallbackReason::NoCallApp),
                "{sessions:?}"
            );
        }
    }

    #[test]
    fn name_override_matches_any_app_prefers_active_then_lowest_pid() {
        let sessions = [
            s(10, r"C:\x\Spotify.exe", false),
            s(30, "spotify.exe", true),
            s(20, "spotify.exe", true),
        ];
        let c =
            choose_loopback(&LoopbackRequest::ProcessName("Spotify".into()), &sessions).unwrap();
        assert_eq!(
            c,
            LoopbackChoice::Process(ProcessTarget {
                pid: 20,
                process_name: Some("spotify.exe".into()),
                app: None,
            })
        );
        let only_inactive = [s(10, "spotify.exe", false)];
        let c = choose_loopback(
            &LoopbackRequest::ProcessName("spotify.exe".into()),
            &only_inactive,
        )
        .unwrap();
        assert_eq!(pid_of(&c), Some(10));
    }

    #[test]
    fn name_override_without_session_is_an_error() {
        assert_eq!(
            choose_loopback(
                &LoopbackRequest::ProcessName("zoom".into()),
                &[s(1, "chrome.exe", true)]
            ),
            Err(SelectError::NoSessionForName("zoom".into()))
        );
    }

    #[test]
    fn pid_override_uses_session_name_when_known() {
        let sessions = [s(42, "Discord.exe", true)];
        assert_eq!(
            choose_loopback(&LoopbackRequest::Pid(42), &sessions).unwrap(),
            LoopbackChoice::Process(ProcessTarget {
                pid: 42,
                process_name: Some("Discord.exe".into()),
                app: Some("Discord"),
            })
        );
        assert_eq!(
            choose_loopback(&LoopbackRequest::Pid(43), &sessions).unwrap(),
            LoopbackChoice::Process(ProcessTarget {
                pid: 43,
                process_name: None,
                app: None,
            })
        );
    }

    #[test]
    fn default_device_request_ignores_sessions() {
        assert_eq!(
            choose_loopback(&LoopbackRequest::DefaultDevice, &[s(1, "zoom.exe", true)]).unwrap(),
            LoopbackChoice::DefaultDevice(FallbackReason::Requested)
        );
    }

    #[test]
    fn source_records_mode_and_reason() {
        let p = LoopbackChoice::Process(ProcessTarget {
            pid: 5,
            process_name: Some("zoom.exe".into()),
            app: Some("Zoom"),
        })
        .source(&LoopbackRequest::Auto);
        assert_eq!(p.mode, LoopbackMode::Process);
        assert_eq!(p.requested, "auto");
        assert_eq!(p.pid, Some(5));
        assert_eq!(p.app.as_deref(), Some("Zoom"));
        assert_eq!(p.fallback_reason, None);

        let d =
            LoopbackChoice::DefaultDevice(FallbackReason::NoCallApp).source(&LoopbackRequest::Auto);
        assert_eq!(d.mode, LoopbackMode::DefaultDevice);
        assert_eq!(d.pid, None);
        assert_eq!(
            d.fallback_reason.as_deref(),
            Some("no call app with an active audio session")
        );
        let r = LoopbackChoice::DefaultDevice(FallbackReason::Requested)
            .source(&LoopbackRequest::DefaultDevice);
        assert_eq!(r.requested, "default-device");
        assert_eq!(r.fallback_reason, None);
    }
}
