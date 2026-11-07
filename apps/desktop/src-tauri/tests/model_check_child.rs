//! The real hidden model check mode of the built app (T7.4e): it runs and exits before any
//! window, store or Tauri is built, and ends with exactly one final JSON line. With an empty
//! models directory the check cannot load ONNX Runtime, so the final line is an error and the
//! exit code is not zero (no model or network is needed).

use std::process::{Command, Stdio};

#[test]
fn model_check_hidden_mode_of_the_built_app_ends_with_one_error_line() {
    let dir = std::env::temp_dir().join(format!("earshot-desktop-check-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_earshot-desktop"))
        .args(["--model-check", "--languages", "en", "--models"])
        .arg(&dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!out.status.success(), "{text}");
    let last = lines.last().expect("a final line");
    assert!(last.starts_with(r#"{"line":"error","message":"#), "{text}");
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains(r#""line":"error""#))
            .count(),
        1,
        "{text}"
    );
    // Bad arguments are an error line too, never a window.
    let out = Command::new(env!("CARGO_BIN_EXE_earshot-desktop"))
        .args(["--model-check"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).contains(r#""line":"error""#));
}
