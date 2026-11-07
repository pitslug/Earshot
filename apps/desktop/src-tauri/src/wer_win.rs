//! Keeps the app out of Windows Error Reporting (T9.1t, PLAN.md 3.9). A crash report can hold
//! part of a process's memory, so transcript text, and depending on the machine's settings
//! Windows sends it to Microsoft. [`opt_out`] puts the running exe's name on the current user's
//! list of excluded applications (`WerAddExcludedApplication`, `HKCU`, no elevation). The list is
//! by file name, so the child modes (the model check, the trials, summaries on the graphics card)
//! are covered too; each also calls it, as the first thing they do. WebView2's own processes are
//! not covered, and excluding them by name would change every WebView2 app of this user
//! (PRIVACY.md says so). Local dumps are a separate Windows setting this does not stop:
//! `crash::remove_old_dumps` deletes ours at the next start.

use std::ffi::OsStr;
use std::path::Path;

use windows::Win32::System::ErrorReporting::WerAddExcludedApplication;
use windows::core::HSTRING;

/// The name the list wants: the file name with its extension.
fn listed_name(exe: &Path) -> Option<&OsStr> {
    exe.file_name()
}

/// Adds `name` to the current user's list.
fn exclude(name: &OsStr) -> windows::core::Result<()> {
    let name = HSTRING::from(name);
    // SAFETY: `name` is a valid NUL-terminated wide string that outlives the call, which reads it
    // and keeps no pointer to it. `false` means the current user's list.
    unsafe { WerAddExcludedApplication(&name, false) }
}

/// Opts the running exe out, best effort: a report nobody may ever need must not cost a meeting,
/// so a failure changes nothing else. Returns whether the name is now on the list.
pub fn opt_out() -> bool {
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(listed_name)
        .is_some_and(|name| exclude(name).is_ok())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant, SystemTime};

    use windows::Win32::System::ErrorReporting::WerRemoveExcludedApplication;
    use windows::core::HSTRING;

    use super::{exclude, listed_name, opt_out};

    const LIST_KEY: &str =
        r"HKCU\Software\Microsoft\Windows\Windows Error Reporting\ExcludedApplications";
    /// How long Windows gets to write a report after a crash.
    const REPORT_WAIT: Duration = Duration::from_secs(30);
    /// Asks the helper test to crash; the helper does nothing without it.
    const CRASH_ENV: &str = "EARSHOT_TEST_WER_CRASH";

    /// Takes `name` off the list however the test ends.
    struct Listed(OsString);

    impl Drop for Listed {
        fn drop(&mut self) {
            let name = HSTRING::from(self.0.as_os_str());
            // SAFETY: as `exclude`.
            let _ = unsafe { WerRemoveExcludedApplication(&name, false) };
        }
    }

    fn on_the_list(name: &str) -> bool {
        Command::new("reg")
            .args(["query", LIST_KEY, "/v", name])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn wer_listed_name_is_the_file_name_with_its_extension() {
        assert_eq!(
            listed_name(&PathBuf::from(
                r"C:\Users\x\AppData\Local\Earshot\earshot.exe"
            )),
            Some(std::ffi::OsStr::new("earshot.exe"))
        );
        assert_eq!(listed_name(&PathBuf::from(r"C:\")), None);
    }

    #[test]
    fn wer_exclusion_lands_on_the_current_users_list() {
        // A name no program has, so a real exe's entry is never touched.
        let name = format!("earshot-wer-test-{}.exe", std::process::id());
        assert!(!on_the_list(&name), "a leftover from an earlier run");
        let listed = Listed(OsString::from(&name));
        exclude(&listed.0).unwrap();
        assert!(on_the_list(&name), "{name} is not under {LIST_KEY}");
        drop(listed);
        assert!(!on_the_list(&name), "the test left {name} behind");
    }

    /// Reports of this test exe queued or archived since `since`: what can go to Microsoft.
    fn reports(exe: &str, since: SystemTime) -> Vec<String> {
        let stem = exe.trim_end_matches(".exe");
        // Windows cuts the exe's name to 16 characters in a report folder's name.
        let short: String = stem.chars().take(16).collect();
        let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap());
        let data = PathBuf::from(std::env::var_os("ProgramData").unwrap());
        let mut found = Vec::new();
        for root in [&local, &data] {
            for folder in ["ReportArchive", "ReportQueue"] {
                let place = root.join(r"Microsoft\Windows\WER").join(folder);
                let Ok(entries) = std::fs::read_dir(&place) else {
                    continue;
                };
                for e in entries.filter_map(Result::ok) {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let new = e
                        .metadata()
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t >= since);
                    if new && name.contains(&short) {
                        found.push(name);
                    }
                }
            }
        }
        found
    }

    /// Runs the helper, which crashes this exe.
    fn crash_once() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "wer_win::tests::wer_crash_helper", "--ignored"])
            .env(CRASH_ENV, "1")
            .status()
            .unwrap();
        assert!(!status.success(), "the helper did not crash");
    }

    /// Not a test of its own: `wer_excluded_exe_leaves_no_report` runs it in a child.
    #[test]
    #[ignore = "crashes on purpose; run only by wer_excluded_exe_leaves_no_report"]
    fn wer_crash_helper() {
        if std::env::var_os(CRASH_ENV).is_some() {
            std::process::abort();
        }
    }

    #[test]
    #[ignore = "crashes this test exe twice and may leave one report of it, so run it from your own \
                shell, not an agent's (virtualised registry): cargo test -p earshot-desktop \
                wer_excluded_exe_leaves_no_report -- --ignored"]
    fn wer_excluded_exe_leaves_no_report() {
        let exe_path = std::env::current_exe().unwrap();
        let exe = exe_path.file_name().unwrap().to_string_lossy().into_owned();
        let listed = Listed(OsString::from(&exe));

        // Negative control: without the exclusion this machine does queue a report, otherwise
        // the second half would prove nothing.
        let start = SystemTime::now();
        crash_once();
        let waited = Instant::now();
        while reports(&exe, start).is_empty() {
            assert!(
                waited.elapsed() < REPORT_WAIT,
                "no report of a crash without the exclusion, so this machine cannot show the \
                 difference"
            );
            std::thread::sleep(Duration::from_millis(500));
        }
        // Lets Windows finish with the first crash, so its files do not count for the second.
        std::thread::sleep(REPORT_WAIT);

        assert!(opt_out(), "the running exe could not be listed");
        let start = SystemTime::now();
        crash_once();
        std::thread::sleep(REPORT_WAIT);
        assert_eq!(
            reports(&exe, start),
            Vec::<String>::new(),
            "a crash queued a report with the exclusion on"
        );

        // `LocalDumps` is apart from reporting and may still have written a dump: the start-up
        // sweep's rule takes whatever this test's crashes left.
        crate::crash::remove_dumps_for_test(&exe);
        let left = std::fs::read_dir(
            PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("CrashDumps"),
        )
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(&format!("{exe}.")))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        assert_eq!(left, Vec::<String>::new(), "a dump survived the sweep");
        drop(listed);
    }
}
