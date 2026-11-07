//! Local crash logs (T9.1e, PLAN.md 3.9): a panic anywhere in the app, on any thread and under
//! `catch_unwind` too (the hook runs before the unwinding), leaves one text file in the
//! `crash-logs` folder of the app's data folder.
//!
//! A file holds the app version, the UTC time, the thread's name, the source location and the
//! function names of the backtrace. It never holds the panic's message or payload, which could
//! quote meeting text: [`CrashReport`] has no field for them and the hook never reads them. Nor
//! does it hold a backtrace's file paths ([`function_names`]) or anything else about the machine.
//! Nothing is uploaded; this module opens no connection.
//!
//! The hook runs inside a panic, where a second panic aborts the process, so it only uses calls
//! that report failure as a value and ignores every failure. It then calls the hook it replaced:
//! that one prints to stderr, which is how a developer sees a panic; the release app has no
//! console and the model check child's stderr is null. Only the newest [`KEEP_FILES`] files stay.

use std::backtrace::Backtrace;
use std::fmt::Write as _;
use std::io::Write as _;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use earshot_store::DataDir;

/// The folder in the app's data folder.
pub const LOGS_FOLDER: &str = "crash-logs";
/// How many files stay (a judgement: enough to show a pattern, small enough to never matter).
pub const KEEP_FILES: usize = 20;
/// How many names one second can give before a report is given up.
const MAX_NAMES_PER_SECOND: u32 = 1000;
/// The most backtrace lines in a file (a panic deep in a recursion has very many frames).
const MAX_BACKTRACE_LINES: usize = 256;
/// The longest line of a file, in characters.
const MAX_LINE_CHARS: usize = 512;

/// Everything a crash file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashReport<'a> {
    /// The app version.
    pub version: &'a str,
    /// When, in seconds since the Unix epoch.
    pub unix_secs: u64,
    /// The thread's name (all of the app's are fixed text); `None` when it has none.
    pub thread: Option<&'a str>,
    /// The source location the panic hook gives, `file:line:column`.
    pub location: Option<String>,
    /// The output of [`function_names`].
    pub functions: Vec<&'a str>,
}

/// `crash-logs` in the data folder `root`.
#[must_use]
pub fn logs_dir(root: &Path) -> PathBuf {
    root.join(LOGS_FOLDER)
}

/// The folder of the app's own data folder, `None` when `LOCALAPPDATA` is not set.
#[must_use]
pub fn default_dir() -> Option<PathBuf> {
    DataDir::from_local_app_data().map(|d| logs_dir(d.root()))
}

/// Installs the hook for the app's own data folder; without one the panic behaviour stays as
/// it is.
pub fn install_default() {
    if let Some(dir) = default_dir() {
        install(dir);
    }
}

/// Installs the hook: a panic writes a file in `dir` (created when needed), then runs the hook
/// this one replaced. Call it once per process, before any other thread starts.
pub fn install(dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record_panic(&dir, info);
        previous(info);
    }));
}

/// Writes the file for this panic. Reads `info` only for its location.
fn record_panic(dir: &Path, info: &PanicHookInfo<'_>) {
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // `force_capture` ignores RUST_BACKTRACE. `write!` to a String cannot fail, where `to_string`
    // would panic on an error.
    let mut backtrace = String::new();
    let _ = write!(backtrace, "{}", Backtrace::force_capture());
    let current = std::thread::current();
    let report = CrashReport {
        version: env!("CARGO_PKG_VERSION"),
        unix_secs,
        thread: current.name(),
        location: info
            .location()
            .map(|l| format!("{}:{}:{}", source_file(l.file()), l.line(), l.column())),
        functions: function_names(&backtrace),
    };
    // A log that cannot be written must not change what the panic does.
    let _ = save(dir, unix_secs, &format_report(&report));
}

/// A panic location's file without anything about the machine that built it. Workspace files are
/// already relative. A dependency's path is kept from its `<crate>-<version>` folder, because the
/// registry's own folders above it say nothing; any other absolute path keeps only its file name.
/// Shipped builds have neutral paths anyway (`xtask::build::NeutralPaths`), so this is what keeps a
/// hand-made build's file to the promise in the module docs.
#[must_use]
pub fn source_file(file: &str) -> &str {
    let is_separator = |c: char| c == '/' || c == '\\';
    let absolute = file.starts_with(is_separator) || file.as_bytes().get(1) == Some(&b':');
    if !absolute {
        return file;
    }
    let parts: Vec<(usize, &str)> = file
        .split_inclusive(is_separator)
        .scan(0, |at, part| {
            let start = *at;
            *at += part.len();
            Some((start, part))
        })
        .collect();
    let registry = parts.windows(4).find_map(|w| {
        let name = |i: usize| w[i].1.trim_end_matches(is_separator);
        (name(0) == "registry" && name(1) == "src").then_some(w[3].0)
    });
    match registry {
        Some(start) => &file[start..],
        None => parts.last().map_or(file, |(_, name)| name),
    }
}

/// Writes one file and removes the oldest beyond [`KEEP_FILES`].
fn save(dir: &Path, unix_secs: u64, text: &str) -> std::io::Result<()> {
    let written = write_new(dir, unix_secs, text)?;
    prune(dir, KEEP_FILES, &written);
    Ok(())
}

/// The function names of a `std::backtrace::Backtrace` printed with `{}`: a frame's first line
/// (`  3: name`) and the inlined names under it. Its `at <path>:<line>:<column>` lines go, and so
/// does any other line naming a path: symbol names have no slashes.
#[must_use]
pub fn function_names(backtrace: &str) -> Vec<&str> {
    backtrace
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("at ") && !l.contains(['/', '\\']))
        .take(MAX_BACKTRACE_LINES)
        .collect()
}

/// `text` as one bounded line, with control characters replaced.
fn one_line(text: &str) -> String {
    text.chars()
        .take(MAX_LINE_CHARS)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The text of a file.
#[must_use]
pub fn format_report(report: &CrashReport<'_>) -> String {
    let mut out = String::from("Earshot crash log\n");
    let _ = writeln!(out, "version: {}", one_line(report.version));
    let _ = writeln!(out, "time: {}", iso_time(report.unix_secs));
    let _ = writeln!(
        out,
        "thread: {}",
        one_line(report.thread.unwrap_or("unnamed"))
    );
    let _ = writeln!(
        out,
        "location: {}",
        one_line(report.location.as_deref().unwrap_or("unknown"))
    );
    out.push_str("backtrace:\n");
    for line in &report.functions {
        let _ = writeln!(out, "  {}", one_line(line));
    }
    out
}

/// UTC civil time of a Unix time: `(year, month, day, hour, minute, second)`. Days to civil after
/// H. Hinnant, "chrono-Compatible Low-Level Date Algorithms", `civil_from_days`, for days from
/// the epoch on; no input overflows.
fn civil(unix_secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let secs_of_day = unix_secs % 86_400;
    let z = unix_secs / 86_400 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (
        year,
        month,
        day,
        secs_of_day / 3600,
        secs_of_day / 60 % 60,
        secs_of_day % 60,
    )
}

/// `2026-11-05T12:34:56Z`.
fn iso_time(unix_secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(unix_secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// `crash-20261105T123456Z-0000.txt`: names sort by time, and `n` keeps two panics in one second
/// (or in the app and its check child) apart.
#[must_use]
pub fn file_name(unix_secs: u64, n: u32) -> String {
    let (y, mo, d, h, mi, s) = civil(unix_secs);
    format!("crash-{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z-{n:04}.txt")
}

/// Whether `name` has exactly the form of [`file_name`]. Pruning touches nothing else.
#[must_use]
pub fn is_crash_file_name(name: &str) -> bool {
    let b = name.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    b.len() == 31
        && name.is_ascii()
        && name.starts_with("crash-")
        && digits(6..14)
        && b[14] == b'T'
        && digits(15..21)
        && &b[21..23] == b"Z-"
        && digits(23..27)
        && name.ends_with(".txt")
}

/// Writes `text` to a new file in `dir` (created when needed), never over an existing file.
fn write_new(dir: &Path, unix_secs: u64, text: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for n in 0..MAX_NAMES_PER_SECOND {
        let path = dir.join(file_name(unix_secs, n));
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match opened {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
}

/// Removes the oldest crash files in `dir` so that at most `keep` remain, by name (the time in
/// it). `written` always stays, even when the clock went back; other files are never touched;
/// a file that cannot be removed stays.
pub fn prune(dir: &Path, keep: usize, written: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let written = written.file_name();
    let mut names: Vec<std::ffi::OsString> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name())
        .filter(|n| n.to_str().is_some_and(is_crash_file_name))
        .filter(|n| Some(n.as_os_str()) != written)
        .collect();
    names.sort_unstable_by(|a, b| b.cmp(a));
    for name in names.iter().skip(keep.saturating_sub(1)) {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// Where the crash logs are, shown on the General page (`null` when the app has no data folder).
#[tauri::command]
#[specta::specta]
pub fn crash_logs_folder() -> Option<String> {
    default_dir().map(|d| d.display().to_string())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::testutil::TempDir;

    /// What the panics in the child say; no file may hold it.
    const MARKER: &str = "MEETING-TEXT-MARKER-7f3a";
    /// Names the folder the child logs to.
    const CHILD_DIR_ENV: &str = "EARSHOT_CRASH_CHILD_DIR";
    const CHILD_TEST: &str = "crash::tests::crash_child_panics_under_catch_unwind_and_on_a_thread";
    const WORKER_THREAD: &str = "earshot-crash-worker";
    const PLAIN_THREAD: &str = "earshot-crash-plain";

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The test binary runs [`CHILD_TEST`] alone in its own process, with its own panic hook.
    fn run_child(dir: &Path) -> std::process::Output {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
            .env(CHILD_DIR_ENV, dir)
            .output()
            .unwrap()
    }

    /// Not a test of its own: [`crash_real_panics_leave_files_without_their_message`] and
    /// [`crash_a_folder_that_cannot_be_written_changes_nothing_about_a_panic`] run it as a
    /// child process, because a panic hook is process-wide.
    #[test]
    #[ignore = "helper: the crash tests run it in a child process"]
    fn crash_child_panics_under_catch_unwind_and_on_a_thread() {
        let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
            return;
        };
        install(PathBuf::from(dir));
        // The way every app worker runs (`jobs`, `meeting`, `model_jobs`).
        let worker = std::thread::Builder::new()
            .name(WORKER_THREAD.into())
            .spawn(|| {
                let caught = std::panic::catch_unwind(|| panic!("{MARKER} said in the meeting"));
                assert!(caught.is_err());
            })
            .unwrap();
        worker.join().unwrap();
        // A plain thread, and a payload that is not a formatted message.
        let plain = std::thread::Builder::new()
            .name(PLAIN_THREAD.into())
            .spawn(|| std::panic::panic_any(format!("{MARKER} as a payload")))
            .unwrap();
        assert!(plain.join().is_err());
    }

    #[test]
    fn crash_real_panics_leave_files_without_their_message() {
        let dir = TempDir::new("crash-real");
        let out = run_child(&dir.0);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "child failed: {stderr}");
        // The message really was there to leak, and the previous hook still ran.
        assert!(stderr.contains(MARKER), "no chained hook output: {stderr}");

        let names = names_in(&dir.0);
        assert_eq!(names.len(), 2, "{names:?}");
        let mut threads = Vec::new();
        for name in &names {
            assert!(is_crash_file_name(name), "{name}");
            let raw = std::fs::read(dir.0.join(name)).unwrap();
            let text = String::from_utf8(raw).unwrap();
            assert!(!text.contains(MARKER), "the message leaked: {text}");
            assert!(!text.contains("said in the meeting"), "{text}");
            assert!(!text.contains("as a payload"), "{text}");

            let (head, frames) = text.split_once("backtrace:\n").unwrap();
            let head: Vec<&str> = head.lines().collect();
            assert_eq!(head.len(), 5, "{text}");
            assert_eq!(head[0], "Earshot crash log");
            assert_eq!(
                head[1],
                format!("version: {}", env!("CARGO_PKG_VERSION")),
                "{text}"
            );
            assert!(head[2].starts_with("time: 20"), "{text}");
            threads.push(head[3].to_owned());
            assert!(head[4].starts_with("location: "), "{text}");
            assert!(head[4].contains("crash.rs:"), "{text}");
            // Function names and nothing else: this frame's name, and no path of any frame.
            assert!(frames.contains("crash"), "no function names: {text}");
            for line in frames.lines() {
                assert!(!line.contains(['/', '\\']), "a path: {line}");
                assert!(!line.trim().starts_with("at "), "a path: {line}");
            }
        }
        threads.sort();
        assert_eq!(
            threads,
            [
                format!("thread: {PLAIN_THREAD}"),
                format!("thread: {WORKER_THREAD}")
            ]
        );
    }

    #[test]
    fn crash_a_folder_that_cannot_be_written_changes_nothing_about_a_panic() {
        let dir = TempDir::new("crash-unwritable");
        // A file where the folder should be.
        let blocked = dir.0.join("blocked");
        std::fs::write(&blocked, b"not a folder").unwrap();
        let out = run_child(&blocked);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "child failed: {stderr}");
        assert!(stderr.contains(MARKER), "no chained hook output: {stderr}");
        assert_eq!(std::fs::read(&blocked).unwrap(), b"not a folder");
    }

    #[test]
    fn crash_backtrace_keeps_function_names_only() {
        let text = "   0: std::backtrace::Backtrace::force_capture\n             at /rustc/abc/library/std/src/backtrace.rs:312:9\n   1: earshot_desktop::jobs::run::{{closure}}\n             core::ops::function::FnOnce::call_once\n             at C:\\Users\\someone\\earshot\\jobs.rs:12:3\n   2: <unknown>\n   3: C:\\Users\\someone\\earshot.exe\n\n";
        assert_eq!(
            function_names(text),
            [
                "0: std::backtrace::Backtrace::force_capture",
                "1: earshot_desktop::jobs::run::{{closure}}",
                "core::ops::function::FnOnce::call_once",
                "2: <unknown>",
            ]
        );
        let long = "1: f\n".repeat(MAX_BACKTRACE_LINES + 5);
        assert_eq!(function_names(&long).len(), MAX_BACKTRACE_LINES);
        assert!(function_names("").is_empty());
    }

    #[test]
    fn crash_location_holds_nothing_of_the_machine_that_built_it() {
        for (file, kept) in [
            // A workspace file, as cargo passes it.
            (
                r"crates\earshot-runtime\src\live.rs",
                r"crates\earshot-runtime\src\live.rs",
            ),
            ("src/main.rs", "src/main.rs"),
            // A dependency, from a build by hand and a shipped one.
            (
                r"C:\Users\someone\.cargo\registry\src\index.crates.io-1949cf\tokio-1.40.0\src\rt.rs",
                r"tokio-1.40.0\src\rt.rs",
            ),
            (
                "/cargo/registry/src/index.crates.io-1949cf/tokio-1.40.0/src/rt.rs",
                "tokio-1.40.0/src/rt.rs",
            ),
            // What rustc's remap really leaves: its prefix, then the path's own separators.
            (
                r"/cargo\registry\src\index.crates.io-1949cf\tokio-1.40.0\src\rt.rs",
                r"tokio-1.40.0\src\rt.rs",
            ),
            // Anything else absolute: a toolchain, a git checkout, a generated file.
            (
                r"C:\Users\someone\.rustup\toolchains\1.99\lib\rustlib\src\rust\library\core\src\option.rs",
                "option.rs",
            ),
            ("/rustc/abc123/library/core/src/option.rs", "option.rs"),
            (r"C:\eb\host\build\out\gen.rs", "gen.rs"),
            // A registry folder with nothing below it is no dependency.
            (r"C:\x\registry\src\idx", "idx"),
        ] {
            assert_eq!(source_file(file), kept, "{file}");
            assert!(!source_file(file).contains("someone"), "{file}");
        }
    }

    #[test]
    fn crash_report_holds_the_fields_on_their_own_lines() {
        let report = CrashReport {
            version: "1.2.3",
            unix_secs: 1_793_881_496,
            thread: Some("earshot-job"),
            location: Some(r"crates\earshot-runtime\src\live.rs:12:3".into()),
            functions: vec!["0: f", "g"],
        };
        assert_eq!(
            format_report(&report),
            "Earshot crash log\nversion: 1.2.3\ntime: 2026-11-05T12:24:56Z\nthread: earshot-job\nlocation: crates\\earshot-runtime\\src\\live.rs:12:3\nbacktrace:\n  0: f\n  g\n"
        );
        let unknown = CrashReport {
            thread: None,
            location: None,
            functions: Vec::new(),
            ..report.clone()
        };
        let text = format_report(&unknown);
        assert!(text.contains("thread: unnamed\nlocation: unknown\nbacktrace:\n"));
        assert!(text.ends_with("backtrace:\n"));
        // A strange name cannot add lines or grow a file.
        let long = "x".repeat(MAX_LINE_CHARS + 10);
        let odd = CrashReport {
            thread: Some("a\nversion: 9"),
            location: Some(long),
            ..report
        };
        let text = format_report(&odd);
        assert!(text.contains("thread: a?version: 9\n"));
        assert!(
            text.lines()
                .all(|l| l.chars().count() <= MAX_LINE_CHARS + 10)
        );
    }

    #[test]
    fn crash_times_are_utc_and_never_overflow() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_time(951_782_399), "2000-02-28T23:59:59Z");
        assert_eq!(iso_time(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso_time(1_793_881_496), "2026-11-05T12:24:56Z");
        assert_eq!(iso_time(4_102_444_799), "2099-12-31T23:59:59Z");
        assert!(iso_time(u64::MAX).ends_with('Z'));
    }

    #[test]
    fn crash_file_names_sort_by_time_and_are_recognised_exactly() {
        let name = file_name(1_793_881_496, 7);
        assert_eq!(name, "crash-20261105T122456Z-0007.txt");
        assert!(is_crash_file_name(&name));
        assert!(file_name(1_793_881_496, 0) < file_name(1_793_881_496, 1));
        assert!(file_name(1_793_881_496, 999) < file_name(1_793_881_497, 0));
        for other in [
            "",
            "crash-.txt",
            "notes.txt",
            "crash-20261105T122456Z-0007.txt.bak",
            "crash-20261105T122456Z-007.txt",
            "crash-2026110xT122456Z-0007.txt",
            "crash-20261105-122456Z-0007.txt",
            "Crash-20261105T122456Z-0007.txt",
            "crash-20261105T122456Z-0007.log",
            "crash-20261105T122456é-0007.tx",
        ] {
            assert!(!is_crash_file_name(other), "{other}");
        }
    }

    #[test]
    fn crash_write_creates_the_folder_and_never_overwrites() {
        let dir = TempDir::new("crash-write");
        let logs = logs_dir(&dir.0);
        let first = write_new(&logs, 100, "one").unwrap();
        let second = write_new(&logs, 100, "two").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(second).unwrap(), "two");
    }

    #[test]
    fn crash_write_failure_is_an_error_for_the_hook_to_ignore() {
        let dir = TempDir::new("crash-fail");
        let blocked = dir.0.join("blocked");
        std::fs::write(&blocked, b"x").unwrap();
        assert!(save(&blocked, 100, "text").is_err());
    }

    #[test]
    fn crash_prune_keeps_the_newest_and_touches_nothing_else() {
        let dir = TempDir::new("crash-prune");
        let mut all = Vec::new();
        for secs in 0..(KEEP_FILES as u64 + 5) {
            all.push(write_new(&dir.0, 1_000_000 + secs, "x").unwrap());
        }
        std::fs::write(dir.0.join("notes.txt"), b"mine").unwrap();
        std::fs::create_dir(dir.0.join("crash-20000101T000000Z-0000.txt")).unwrap();
        prune(&dir.0, KEEP_FILES, all.last().unwrap());
        for (i, path) in all.iter().enumerate() {
            assert_eq!(path.exists(), i >= 5, "{path:?}");
        }
        assert_eq!(names_in(&dir.0).len(), KEEP_FILES + 2);
        assert!(dir.0.join("notes.txt").exists());
        assert!(dir.0.join("crash-20000101T000000Z-0000.txt").is_dir());
    }

    #[test]
    fn crash_prune_keeps_the_file_just_written_when_the_clock_went_back() {
        let dir = TempDir::new("crash-clock");
        for secs in 0..4 {
            write_new(&dir.0, 2_000_000 + secs, "x").unwrap();
        }
        let written = write_new(&dir.0, 1_000_000, "x").unwrap();
        prune(&dir.0, 3, &written);
        assert!(written.exists());
        assert_eq!(names_in(&dir.0).len(), 3);
        assert!(dir.0.join(file_name(2_000_003, 0)).exists());
        assert!(dir.0.join(file_name(2_000_002, 0)).exists());
        // A missing folder is nothing to do.
        prune(&dir.0.join("missing"), 3, &written);
    }

    #[test]
    fn crash_saving_past_the_limit_leaves_the_limit() {
        let dir = TempDir::new("crash-save");
        for secs in 0..(KEEP_FILES as u64 + 3) {
            save(&dir.0, 1_000_000 + secs, "x").unwrap();
        }
        assert_eq!(names_in(&dir.0).len(), KEEP_FILES);
        assert!(!dir.0.join(file_name(1_000_000, 0)).exists());
    }

    #[test]
    fn crash_logs_live_in_the_app_data_folder() {
        assert_eq!(
            logs_dir(Path::new(r"C:\data\Earshot")),
            Path::new(r"C:\data\Earshot").join("crash-logs")
        );
        let data = DataDir::from_local_app_data().map(|d| logs_dir(d.root()));
        assert_eq!(default_dir(), data);
        assert_eq!(crash_logs_folder(), data.map(|d| d.display().to_string()));
    }
}
