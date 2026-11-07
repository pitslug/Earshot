//! Tests of the Audio page settings (T7.4i): stored values, validation and the device
//! selection the meeting start uses.

use super::*;
use crate::testutil::TempDir;

#[test]
fn audio_settings_device_available_follows_the_shared_case_table() {
    let text = include_str!("../../../fixtures/audio/device_available_cases.json");
    let table: serde_json::Value = serde_json::from_str(text).unwrap();
    let cases = table["cases"].as_array().unwrap();
    assert!(cases.len() >= 9, "the table lost cases");
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let listed: Vec<(&str, bool)> = case["listed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_str().unwrap(), p[1].as_bool().unwrap()))
            .collect();
        let chosen = case["chosen"].as_str();
        assert_eq!(
            device_available(listed.iter().copied(), chosen),
            case["available"].as_bool().unwrap(),
            "{name}"
        );
    }
}

fn store(dir: &TempDir) -> Store {
    Store::open(&dir.0.join("earshot.db")).unwrap()
}

const MIC_ID: &str = "{0.0.1.00000000}.{5b862d27-476a-4b77-8ac5-a4af31878474}";
const OUT_ID: &str = "{0.0.0.00000000}.{23e79b98-1e59-4d5b-bce3-76fa15461197}";

fn chosen(id: &str, name: &str) -> DeviceSetting {
    DeviceSetting::Chosen {
        id: id.into(),
        name: name.into(),
    }
}

#[test]
fn audio_settings_round_trip_in_the_store() {
    let dir = TempDir::new("audio-settings");
    let mut db = store(&dir);
    assert_eq!(
        audio_settings(&db).unwrap(),
        (AudioSettings::default(), Vec::new())
    );
    assert_eq!(
        AudioSettings::default().capture_mode,
        DefaultCaptureMode::Call
    );
    let want = AudioSettings {
        microphone: chosen(MIC_ID, "Acme Evolve"),
        output: chosen(OUT_ID, "Speakers"),
        capture_mode: DefaultCaptureMode::InRoom,
    };
    assert_eq!(set_audio_settings(&mut db, &want).unwrap(), want);
    assert_eq!(audio_settings(&db).unwrap(), (want.clone(), Vec::new()));
    // Kept across a reopen, as text under the registry keys.
    drop(db);
    let mut db = store(&dir);
    assert_eq!(audio_settings(&db).unwrap().0, want);
    assert_eq!(
        db.local_value(MICROPHONE_KEY).unwrap().unwrap(),
        MIC_ID.as_bytes()
    );
    assert_eq!(
        db.local_value(MICROPHONE_NAME_KEY).unwrap().unwrap(),
        b"Acme Evolve"
    );
    assert_eq!(
        db.local_value(CAPTURE_MODE_KEY).unwrap().unwrap(),
        b"in-room"
    );
    // Back to the defaults: the names are cleared with the ids.
    let defaults = AudioSettings::default();
    assert_eq!(set_audio_settings(&mut db, &defaults).unwrap(), defaults);
    assert_eq!(audio_settings(&db).unwrap(), (defaults, Vec::new()));
    assert_eq!(db.local_value(OUTPUT_KEY).unwrap().unwrap(), b"default");
    assert_eq!(db.local_value(OUTPUT_NAME_KEY).unwrap().unwrap(), b"");
    assert_eq!(db.local_value(CAPTURE_MODE_KEY).unwrap().unwrap(), b"call");
}

#[test]
fn audio_settings_unusable_stored_values_read_as_defaults_and_are_reported() {
    let dir = TempDir::new("audio-unknown");
    let mut db = store(&dir);
    db.set_local_value(MICROPHONE_KEY, b"bad\nid").unwrap();
    db.set_local_value(MICROPHONE_NAME_KEY, b"Headset").unwrap();
    db.set_local_value(OUTPUT_KEY, OUT_ID.as_bytes()).unwrap();
    // A name that cannot be shown is dropped, the choice kept.
    db.set_local_value(OUTPUT_NAME_KEY, b"two\nlines").unwrap();
    db.set_local_value(CAPTURE_MODE_KEY, b"podcast").unwrap();
    let (s, unknown) = audio_settings(&db).unwrap();
    assert_eq!(s.microphone, DeviceSetting::Default);
    assert_eq!(s.output, chosen(OUT_ID, ""));
    assert_eq!(s.capture_mode, DefaultCaptureMode::Call);
    let keys: Vec<&str> = unknown.iter().map(|u| u.key).collect();
    assert_eq!(keys, [MICROPHONE_KEY, CAPTURE_MODE_KEY]);
    assert_eq!(unknown[1].stored, "podcast");
    assert!(unknown[1].to_string().contains("the default is used"));
    // A chosen id with no name key at all is still the choice.
    db.set_local_value(MICROPHONE_KEY, MIC_ID.as_bytes())
        .unwrap();
    db.delete_local_value(MICROPHONE_NAME_KEY).unwrap();
    assert_eq!(
        audio_settings(&db).unwrap().0.microphone,
        chosen(MIC_ID, "")
    );
}

#[test]
fn audio_settings_refusals_store_nothing() {
    let dir = TempDir::new("audio-refused");
    let mut db = store(&dir);
    let base = AudioSettings {
        capture_mode: DefaultCaptureMode::InRoom,
        ..AudioSettings::default()
    };
    for (microphone, why) in [
        (chosen("", "Mic"), "empty id"),
        (chosen("   ", "Mic"), "blank id"),
        (chosen("default", "Mic"), "the default's word"),
        (chosen("a\u{0}b", "Mic"), "control character"),
        (chosen(&"x".repeat(MAX_DEVICE_ID + 1), "Mic"), "long id"),
    ] {
        let e = set_audio_settings(
            &mut db,
            &AudioSettings {
                microphone,
                ..base.clone()
            },
        )
        .unwrap_err();
        assert!(matches!(e, AudioSettingsError::BadDeviceId), "{why}: {e}");
    }
    for name in ["a\nb".to_owned(), "n".repeat(MAX_DEVICE_NAME + 1)] {
        let e = set_audio_settings(
            &mut db,
            &AudioSettings {
                output: chosen(OUT_ID, &name),
                ..base.clone()
            },
        )
        .unwrap_err();
        assert!(matches!(e, AudioSettingsError::BadDeviceName), "{e}");
        assert!(e.to_string().contains("cannot be chosen"));
    }
    assert_eq!(db.local_value(CAPTURE_MODE_KEY).unwrap(), None);
    // The longest id and name are kept; ids and names are trimmed.
    let ok = AudioSettings {
        microphone: chosen(&format!(" {MIC_ID} "), " Mic "),
        output: chosen(&"x".repeat(MAX_DEVICE_ID), &"n".repeat(MAX_DEVICE_NAME)),
        ..base
    };
    let stored = set_audio_settings(&mut db, &ok).unwrap();
    assert_eq!(stored.microphone, chosen(MIC_ID, "Mic"));
    assert_eq!(audio_settings(&db).unwrap().0, stored);
}

#[test]
fn audio_settings_capture_mode_names() {
    for m in [DefaultCaptureMode::Call, DefaultCaptureMode::InRoom] {
        assert_eq!(DefaultCaptureMode::parse(m.as_str()), Some(m));
    }
    assert_eq!(DefaultCaptureMode::parse("Call"), None);
    assert_eq!(DeviceSetting::Default.id(), None);
    assert_eq!(chosen(MIC_ID, "").id(), Some(MIC_ID));
}

#[cfg(windows)]
#[test]
fn audio_settings_give_the_capture_layer_the_chosen_endpoints() {
    use earshot_capture_wasapi::DeviceChoice;
    let s = AudioSettings {
        microphone: chosen(MIC_ID, "Jabra"),
        output: DeviceSetting::Default,
        capture_mode: DefaultCaptureMode::Call,
    };
    let d = s.devices();
    assert_eq!(
        d.microphone,
        DeviceChoice::Endpoint {
            id: MIC_ID.into(),
            name: "Jabra".into()
        }
    );
    assert_eq!(d.output, DeviceChoice::Default);
}
