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
    assert_eq!(AudioSettings::default().mic_signal, MicSignal::Automatic);
    let want = AudioSettings {
        microphone: chosen(MIC_ID, "Acme Evolve"),
        output: chosen(OUT_ID, "Speakers"),
        capture_mode: DefaultCaptureMode::InRoom,
        mic_signal: MicSignal::Raw,
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
    assert_eq!(db.local_value(MIC_SIGNAL_KEY).unwrap().unwrap(), b"raw");
    // Back to the defaults: the names are cleared with the ids.
    let defaults = AudioSettings::default();
    assert_eq!(set_audio_settings(&mut db, &defaults).unwrap(), defaults);
    assert_eq!(audio_settings(&db).unwrap(), (defaults, Vec::new()));
    assert_eq!(db.local_value(OUTPUT_KEY).unwrap().unwrap(), b"default");
    assert_eq!(db.local_value(OUTPUT_NAME_KEY).unwrap().unwrap(), b"");
    assert_eq!(db.local_value(CAPTURE_MODE_KEY).unwrap().unwrap(), b"call");
    assert_eq!(db.local_value(MIC_SIGNAL_KEY).unwrap().unwrap(), b"auto");
    // Windows' processing chosen on purpose has its own word.
    let processed = AudioSettings {
        mic_signal: MicSignal::Processed,
        ..AudioSettings::default()
    };
    set_audio_settings(&mut db, &processed).unwrap();
    assert_eq!(
        db.local_value(MIC_SIGNAL_KEY).unwrap().unwrap(),
        b"processed"
    );
    assert_eq!(audio_settings(&db).unwrap(), (processed, Vec::new()));
}

#[test]
fn audio_settings_mic_signal_reads_every_stored_word() {
    // T12.36b: `auto`, `processed` and `raw` as written; T12.36's `default`, written whenever
    // any Audio setting was saved, is not a choice and reads as automatic, unreported; an
    // unknown word is reported and the default (automatic) used.
    let dir = TempDir::new("audio-signal-words");
    let mut db = store(&dir);
    for (stored, want, reported) in [
        ("auto", MicSignal::Automatic, false),
        ("default", MicSignal::Automatic, false),
        ("processed", MicSignal::Processed, false),
        ("raw", MicSignal::Raw, false),
        ("Raw", MicSignal::Automatic, true),
        ("automatic", MicSignal::Automatic, true),
    ] {
        db.set_local_value(MIC_SIGNAL_KEY, stored.as_bytes())
            .unwrap();
        let (s, unknown) = audio_settings(&db).unwrap();
        assert_eq!(s.mic_signal, want, "{stored}");
        let keys: Vec<&str> = unknown.iter().map(|u| u.key).collect();
        let expected: &[&str] = if reported { &[MIC_SIGNAL_KEY] } else { &[] };
        assert_eq!(keys, expected, "{stored}");
    }
}

#[test]
fn audio_settings_automatic_is_raw_only_in_the_room() {
    use DefaultCaptureMode::{Call, InRoom};
    assert!(MicSignal::Automatic.raw_in(InRoom));
    assert!(!MicSignal::Automatic.raw_in(Call));
    for mode in [Call, InRoom] {
        assert!(MicSignal::Raw.raw_in(mode));
        assert!(!MicSignal::Processed.raw_in(mode));
    }
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
    // A word this version does not know is never read as the explicit raw choice.
    db.set_local_value(MIC_SIGNAL_KEY, b"Raw").unwrap();
    let (s, unknown) = audio_settings(&db).unwrap();
    assert_eq!(s.microphone, DeviceSetting::Default);
    assert_eq!(s.output, chosen(OUT_ID, ""));
    assert_eq!(s.capture_mode, DefaultCaptureMode::Call);
    assert_eq!(s.mic_signal, MicSignal::Automatic);
    let keys: Vec<&str> = unknown.iter().map(|u| u.key).collect();
    assert_eq!(keys, [MICROPHONE_KEY, CAPTURE_MODE_KEY, MIC_SIGNAL_KEY]);
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
    for m in [MicSignal::Automatic, MicSignal::Processed, MicSignal::Raw] {
        assert_eq!(MicSignal::parse(m.as_str()), Some(m));
    }
    assert_eq!(MicSignal::parse("default"), Some(MicSignal::Automatic));
    assert_eq!(MicSignal::parse("Processed"), None);
    assert_eq!(DeviceSetting::Default.id(), None);
    assert_eq!(chosen(MIC_ID, "").id(), Some(MIC_ID));
}

#[cfg(windows)]
#[test]
fn audio_settings_give_the_capture_layer_the_chosen_endpoints() {
    use earshot_capture_wasapi::{DeviceChoice, MicProcessing};
    let s = AudioSettings {
        microphone: chosen(MIC_ID, "Jabra"),
        output: DeviceSetting::Default,
        capture_mode: DefaultCaptureMode::Call,
        mic_signal: MicSignal::Processed,
    };
    let d = s.devices(DefaultCaptureMode::Call);
    assert_eq!(d.mic_processing, MicProcessing::Default);
    assert!(!d.raw_fallback_quiet);
    use DefaultCaptureMode::{Call, InRoom};
    // What each choice asks of the capture layer in each kind of meeting (the meeting's mode
    // decides, not the stored mode a new meeting starts with): (processing, quiet fallback).
    for (signal, mode, processing, quiet) in [
        (MicSignal::Processed, Call, MicProcessing::Default, false),
        (MicSignal::Processed, InRoom, MicProcessing::Default, false),
        // The raw setting, in a call and in the room alike, warns when it falls back.
        (MicSignal::Raw, Call, MicProcessing::Raw, false),
        (MicSignal::Raw, InRoom, MicProcessing::Raw, false),
        // Automatic: raw in the room, falling back quietly; processed in a call.
        (MicSignal::Automatic, InRoom, MicProcessing::Raw, true),
        (MicSignal::Automatic, Call, MicProcessing::Default, false),
    ] {
        for stored_mode in [Call, InRoom] {
            let settings = AudioSettings {
                capture_mode: stored_mode,
                mic_signal: signal,
                ..s.clone()
            };
            let got = settings.devices(mode);
            assert_eq!(
                (got.mic_processing, got.raw_fallback_quiet),
                (processing, quiet),
                "{signal:?} in {mode:?}"
            );
            assert_eq!(got.microphone, d.microphone);
        }
    }
    assert_eq!(
        d.microphone,
        DeviceChoice::Endpoint {
            id: MIC_ID.into(),
            name: "Jabra".into()
        }
    );
    assert_eq!(d.output, DeviceChoice::Default);
}
