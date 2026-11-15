//! The worker's panic hook (as the desktop's T9.1e hook): a panic prints where it happened and
//! nothing else. Its message (the payload) is never read, because a panic's message can hold
//! whatever the code was handling, transcript text included, and the container's log is kept by
//! Docker.

/// The line printed for a panic at `location` (`file:line`, or nothing when it is unknown).
#[must_use]
pub fn panic_line(location: Option<(&str, u32)>) -> String {
    match location {
        Some((file, line)) => format!("earshot-worker: panicked at {file}:{line}"),
        None => "earshot-worker: panicked".to_owned(),
    }
}

/// Installs the hook for the whole process; call it first in `main`.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let location = info.location().map(|l| (l.file(), l.line()));
        eprintln!("{}", panic_line(location));
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the child panics with: it must never reach the output.
    const PAYLOAD: &str = "an-invented-transcript-line-that-must-not-be-printed";

    #[test]
    fn crash_line_names_only_the_place() {
        assert_eq!(
            panic_line(Some(("src/worker.rs", 12))),
            "earshot-worker: panicked at src/worker.rs:12"
        );
        assert_eq!(panic_line(None), "earshot-worker: panicked");
    }

    /// Run only as the child of [`crash_hook_prints_the_place_never_the_message`]: the hook is
    /// process-wide.
    #[test]
    #[ignore = "a helper: run by crash_hook_prints_the_place_never_the_message in a child process"]
    fn crash_hook_child_panics() {
        install_panic_hook();
        panic!("{PAYLOAD}");
    }

    #[test]
    fn crash_hook_prints_the_place_never_the_message() {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "crash::tests::crash_hook_child_panics",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.status.success(), "the child panicked: {text}");
        assert!(
            text.contains("earshot-worker: panicked at ") && text.contains("crash.rs:"),
            "{text}"
        );
        assert!(!text.contains(PAYLOAD), "{text}");
    }
}
