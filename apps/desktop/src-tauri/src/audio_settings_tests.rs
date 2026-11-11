//! Tests of the Audio page IPC (T7.4i): settings, the device list the page shows, and the
//! capture the meeting start builds from them. Every name contains `audio`.

use super::*;
use crate::testutil::{TempDir, services};

const MIC: &str = "{0.0.1.00000000}.{5b862d27-476a-4b77-8ac5-a4af31878474}";
const HEADSET: &str = "{0.0.1.00000000}.{11111111-2222-3333-4444-555555555555}";
const SPEAKERS: &str = "{0.0.0.00000000}.{23e79b98-1e59-4d5b-bce3-76fa15461197}";

fn item(id: &str, name: &str, is_default: bool, hands_free: bool) -> DeviceItem {
    DeviceItem {
        id: id.into(),
        name: name.into(),
        is_default,
        hands_free,
    }
}

fn surface() -> Devices {
    Devices {
        microphones: vec![
            item(MIC, "Microphone Array", true, false),
            item(HEADSET, "Headset (Acme Headset)", false, true),
        ],
        outputs: vec![item(SPEAKERS, "Speakers", true, false)],
    }
}

fn chosen(id: &str, name: &str) -> Option<ChosenDevice> {
    Some(ChosenDevice {
        id: id.into(),
        name: name.into(),
    })
}

#[test]
fn audio_page_defaults_and_round_trip() {
    let dir = TempDir::new("audio-round-trip");
    let svc = services(&dir);
    let v = audio_page(&svc, Ok(surface())).unwrap();
    assert_eq!(
        v.value,
        AudioSettingsValue {
            microphone: None,
            output: None,
            capture_mode: CaptureMode::Call,
        }
    );
    assert!(v.unknown.is_empty());
    assert_eq!(v.microphones.len(), 2);
    assert!(v.microphones[1].hands_free);
    assert_eq!(v.devices_error, None);
    assert!(!v.microphone_missing && !v.output_missing);

    let want = AudioSettingsValue {
        microphone: chosen(HEADSET, "Headset (Acme Headset)"),
        output: chosen(SPEAKERS, "Speakers"),
        capture_mode: CaptureMode::InRoom,
    };
    let stored = set_settings(&svc, &want, Ok(surface())).unwrap();
    assert_eq!(stored.value, want);
    assert_eq!(audio_page(&svc, Ok(surface())).unwrap().value, want);
}

#[test]
fn audio_page_marks_a_chosen_device_that_is_not_listed() {
    let dir = TempDir::new("audio-missing");
    let svc = services(&dir);
    let value = AudioSettingsValue {
        microphone: chosen(HEADSET, "Headset (Acme Headset)"),
        output: chosen("{gone}", "USB speakers"),
        capture_mode: CaptureMode::Call,
    };
    let v = set_settings(&svc, &value, Ok(surface())).unwrap();
    assert!(!v.microphone_missing);
    assert!(v.output_missing);
    // The chosen output is kept with its name, so the page can name it.
    assert_eq!(v.value.output, chosen("{gone}", "USB speakers"));
    // Unplug the headset.
    let mut now = surface();
    now.microphones.truncate(1);
    let v = audio_page(&svc, Ok(now)).unwrap();
    assert!(v.microphone_missing);
    // No default output at all: the default choice is missing too.
    let none = Devices {
        microphones: surface().microphones,
        outputs: Vec::new(),
    };
    let v = page_view(&AudioSettings::default(), Vec::new(), Ok(none));
    assert!(v.output_missing && !v.microphone_missing);
}

#[test]
fn audio_page_shows_a_listing_failure_and_keeps_the_settings() {
    let dir = TempDir::new("audio-list-failed");
    let svc = services(&dir);
    let value = AudioSettingsValue {
        microphone: chosen(MIC, "Microphone Array"),
        output: None,
        capture_mode: CaptureMode::InRoom,
    };
    let v = set_settings(
        &svc,
        &value,
        Err("Windows could not list the audio devices.".into()),
    )
    .unwrap();
    assert_eq!(v.value, value);
    assert!(v.microphones.is_empty() && v.outputs.is_empty());
    assert_eq!(
        v.devices_error.as_deref(),
        Some("Windows could not list the audio devices.")
    );
    // Unknown, not missing.
    assert!(!v.microphone_missing && !v.output_missing);
}

#[test]
fn audio_page_refuses_an_unusable_device_and_stores_nothing() {
    let dir = TempDir::new("audio-refused");
    let svc = services(&dir);
    let bad = AudioSettingsValue {
        microphone: chosen("bad\nid", "Mic"),
        output: None,
        capture_mode: CaptureMode::InRoom,
    };
    let e = set_settings(&svc, &bad, Ok(surface())).unwrap_err();
    assert_eq!(e.kind, "invalid_setting");
    assert_eq!(
        e.message,
        "This device cannot be chosen: its id is not usable."
    );
    assert!(!e.retryable);
    assert_eq!(
        audio_page(&svc, Ok(surface())).unwrap().value.capture_mode,
        CaptureMode::Call
    );
}

#[test]
fn audio_default_capture_mode_reads_only_the_store() {
    let dir = TempDir::new("audio-mode");
    let svc = services(&dir);
    assert_eq!(stored_capture_mode(&svc).unwrap(), CaptureMode::Call);
    let value = AudioSettingsValue {
        microphone: None,
        output: None,
        capture_mode: CaptureMode::InRoom,
    };
    set_settings(&svc, &value, Err("not listed".into())).unwrap();
    assert_eq!(stored_capture_mode(&svc).unwrap(), CaptureMode::InRoom);
    svc.store()
        .set_local_value(
            earshot_runtime::audio_settings::CAPTURE_MODE_KEY,
            b"podcast",
        )
        .unwrap();
    assert_eq!(stored_capture_mode(&svc).unwrap(), CaptureMode::Call);
}

#[test]
fn audio_page_reports_stored_values_this_version_cannot_use() {
    let dir = TempDir::new("audio-unknown");
    let svc = services(&dir);
    svc.store()
        .set_local_value(
            earshot_runtime::audio_settings::CAPTURE_MODE_KEY,
            b"podcast",
        )
        .unwrap();
    let v = audio_page(&svc, Ok(surface())).unwrap();
    assert_eq!(v.value.capture_mode, CaptureMode::Call);
    assert_eq!(v.unknown.len(), 1);
    assert!(
        v.unknown[0].contains("settings/capture-mode"),
        "{:?}",
        v.unknown
    );
}

#[cfg(windows)]
#[test]
fn audio_settings_reach_the_meeting_capture() {
    use earshot_capture_wasapi::{DeviceChoice, LoopbackRequest};

    use crate::meeting::{StartRequest, meeting_capture};

    let dir = TempDir::new("audio-meeting");
    let svc = services(&dir);
    let request = |mode| StartRequest {
        mode,
        title: String::new(),
        keep_local: false,
    };
    // Nothing chosen: the default devices, and a call records the whole output (T6.14a).
    let c = meeting_capture(&svc, &request(CaptureMode::Call)).unwrap();
    assert_eq!(
        c.mode,
        earshot_capture_wasapi::CaptureMode::Call(LoopbackRequest::DefaultDevice)
    );
    assert_eq!(
        c.devices,
        earshot_capture_wasapi::DeviceSelection::default()
    );
    assert!(c.notices.is_empty());
    // The chosen endpoints, whatever mode the meeting asks for (the stored mode is only the
    // screen's preselection).
    set_settings(
        &svc,
        &AudioSettingsValue {
            microphone: chosen(HEADSET, "Headset"),
            output: chosen(SPEAKERS, "Speakers"),
            capture_mode: CaptureMode::Call,
        },
        Ok(surface()),
    )
    .unwrap();
    let c = meeting_capture(&svc, &request(CaptureMode::InRoom)).unwrap();
    assert_eq!(c.mode, earshot_capture_wasapi::CaptureMode::InRoom);
    assert_eq!(
        c.devices.microphone,
        DeviceChoice::Endpoint {
            id: HEADSET.into(),
            name: "Headset".into()
        }
    );
    assert_eq!(
        c.devices.output,
        DeviceChoice::Endpoint {
            id: SPEAKERS.into(),
            name: "Speakers".into()
        }
    );
    // A stored value this version cannot use is a start banner, never silent.
    svc.store()
        .set_local_value(earshot_runtime::audio_settings::MICROPHONE_KEY, b"bad\nid")
        .unwrap();
    let c = meeting_capture(&svc, &request(CaptureMode::Call)).unwrap();
    assert_eq!(c.devices.microphone, DeviceChoice::Default);
    assert_eq!(c.notices.len(), 1);
    assert!(
        matches!(&c.notices[0], crate::meeting_events::MeetingEvent::Warning { key, .. } if key == "audio-settings-0"),
        "{:?}",
        c.notices
    );
}
