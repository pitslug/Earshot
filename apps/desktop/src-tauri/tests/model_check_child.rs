//! The real hidden model check mode of the built app (T7.4e): it runs and exits before any
//! window, store or Tauri is built, and ends with exactly one final JSON line. With an empty
//! models directory the check cannot load ONNX Runtime, so the final line is an error and the
//! exit code is not zero (no model or network is needed). Every line of the protocol starts with
//! its prefix (T14.11), written out here so a change to it is noticed.

use std::process::{Command, Stdio};

/// What starts every line of the check's protocol (`check_child::LINE_PREFIX`).
const LINE_PREFIX: &str = "earshot-model-check ";

/// The lines of the protocol in `stdout`, without their prefix.
fn protocol_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter_map(|l| l.strip_prefix(LINE_PREFIX))
        .collect()
}

#[test]
fn model_check_hidden_mode_of_the_built_app_ends_with_one_error_line() {
    let dir = std::env::temp_dir().join(format!("earshot-desktop-check-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_earshot"))
        .args(["--model-check", "--languages", "en", "--models"])
        .arg(&dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!out.status.success(), "{text}");
    // Nothing but lines of the protocol: every line that is not empty has the prefix.
    let written: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let lines = protocol_lines(&text);
    assert_eq!(written.len(), lines.len(), "{text}");
    // Exactly one error line, and it is the last.
    let last = lines.last().expect("a final line");
    assert!(last.starts_with(r#"{"line":"error","message":"#), "{text}");
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with(r#"{"line":"error","#))
            .count(),
        1,
        "{text}"
    );
    // Bad arguments are an error line too, never a window.
    let out = Command::new(env!("CARGO_BIN_EXE_earshot"))
        .args(["--model-check"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8_lossy(&out.stdout);
    let lines = protocol_lines(&text);
    assert_eq!(lines.len(), 1, "{text}");
    assert!(
        lines[0].starts_with(r#"{"line":"error","message":"#),
        "{text}"
    );
}
