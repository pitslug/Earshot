//! `earshot-cli devices` and `earshot-cli record` on real audio devices: informational, run by
//! hand on a host with a microphone and speakers (GATE-01), with
//! `cargo test -p earshot-cli --test record -- --ignored`. `record` also needs the `ci`
//! model set. Ctrl+C is checked by hand at GATE-01 (a console control event would reach
//! the test runner too).

#![cfg(windows)]

use std::path::PathBuf;
use std::process::{Command, Output};

use earshot_core::model::{AudioFileRepo, MeetingRepo};
use earshot_store::{DataDir, Store};

fn earshot(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_earshot-cli"))
        .args(args)
        .output()
        .expect("run earshot")
}

fn describe(out: &Output) -> String {
    format!(
        "status {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
#[ignore = "needs audio devices (GATE-01): cargo test -p earshot-cli --test record -- --ignored"]
fn record_devices_lists_the_default_endpoints() {
    let out = earshot(&["devices"]);
    assert!(out.status.success(), "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for label in [
        "mic (console):",
        "mic (communications):",
        "loopback (default render):",
    ] {
        assert!(stdout.contains(label), "{label}\n{}", describe(&out));
    }
    println!("{}", describe(&out));
}

#[test]
#[ignore = "needs audio devices (GATE-01): cargo test -p earshot-cli --test record -- --ignored"]
fn record_a_short_meeting_finalises_both_tracks() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("earshot-cli-record-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let data = dir.join("data");
    let out = earshot(&[
        "record",
        "--seconds",
        "3",
        "--out",
        data.to_str().unwrap(),
        "--model",
        "base.en",
        "--loopback-process",
        "default-device",
        "--no-post",
        "--no-summary",
    ]);
    assert!(out.status.success(), "{}", describe(&out));
    println!("{}", describe(&out));
    let store = Store::open(&DataDir::new(&data).db_path()).unwrap();
    let meetings = store.meetings().unwrap();
    assert_eq!(meetings.len(), 1);
    let files = store.audio_files(meetings[0].id).unwrap();
    assert_eq!(files.len(), 2, "{}", describe(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("live asr mic:"), "{}", describe(&out));
    assert!(stdout.contains("aec "), "{}", describe(&out));
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
