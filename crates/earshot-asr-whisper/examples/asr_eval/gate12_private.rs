//! Settings of the private echo-leak recordings (T9.1kb): where they are and which stretches
//! of them the evaluation tools look at. These were read off the recordings, so they live
//! outside the repo, in `%USERPROFILE%\.earshot-private\gate12.toml`. A tool whose settings are
//! missing says so and skips its private part; a file that cannot be read is an error.
//!
//! Compiled by path into `gate12_diag`, `model_eval`, `xtask`, earshot-cli's
//! `final_pass_pieces` test, earshot-runtime's test build and earshot-core's `aec_eval` example
//! and `capture_raw_recording` test (never core's library): std, serde and toml only.

#![allow(
    dead_code,
    reason = "each tool that includes this file reads only its own fields"
)]

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The settings file's place under the home folder.
const IN_HOME: &[&str] = &[".earshot-private", "gate12.toml"];

/// Why a tool skips its private part when the file is missing.
pub const NOT_HERE: &str =
    "its private settings (.earshot-private/gate12.toml in the home folder) are not here";

/// One stretch a tool reports on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    /// Name in the report.
    pub label: String,
    /// Start (ms from the start of the recording).
    pub from_ms: u64,
    /// End (ms, exclusive).
    pub to_ms: u64,
}

/// One recording `xtask t613 quality` replays.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Take {
    /// Name in the report.
    pub name: String,
    /// Folder under the private fixtures folder.
    pub dir: String,
}

/// Everything the tools need to know about the private recordings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate12Private {
    /// `model_eval`'s default recording folder, relative to the repo root.
    pub model_eval_dir: String,
    /// `t613 quality`'s recordings.
    pub takes: Vec<Take>,
    /// `gate12_diag`'s windows; the first is the one its tables use.
    pub windows: Vec<Window>,
    /// Script lines reported, 1-based: first and one past the last.
    pub lines: [usize; 2],
    /// Script words counted by name.
    pub key_words: Vec<String>,
    /// Stretches `[from_ms, to_ms)` where the near end was silent and the far end played.
    pub residue_ms: Vec<[u64; 2]>,
    /// Script words `final_pass_pieces` expects in the stored mic transcript.
    pub script_words: Vec<String>,
    /// The a private recording's folder, relative to the repo root (`aec_rumble`'s ignored
    /// test).
    pub take3_dir: String,
    /// In that recording, stretches `[from_ms, to_ms)` with only the far end.
    pub take3_far_ms: Vec<[u64; 2]>,
    /// In that recording, a stretch `[from_ms, to_ms)` with only the near end.
    pub take3_near_ms: [u64; 2],
    /// The GATE-10 recording.
    pub gate10: Gate10,
}

/// The GATE-10 recording: where it is and what `echo_nl`'s ignored test expects of it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate10 {
    /// Its folder, relative to the repo root (`aec_eval`, so `accept m1`, and the ignored
    /// tests that replay it). The stereo recording sits beside it with a `-stereo` suffix.
    pub dir: String,
    /// The bulk delay's range (ms), low and one past high.
    pub delay_ms: [f64; 2],
}

impl Gate12Private {
    /// Parses and checks the file's text. The message never quotes the text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let s: Self = toml::from_str(text).map_err(|e| {
            format!(
                "not valid ({})",
                e.message().chars().take(120).collect::<String>()
            )
        })?;
        if s.windows.is_empty() {
            return Err("it needs at least one window".into());
        }
        if s.windows.iter().any(|w| w.from_ms >= w.to_ms) {
            return Err("a window ends before it starts".into());
        }
        if s.residue_ms.iter().any(|r| r[0] >= r[1]) {
            return Err("a residue stretch ends before it starts".into());
        }
        if s.gate10.delay_ms[0].partial_cmp(&s.gate10.delay_ms[1]) != Some(std::cmp::Ordering::Less)
        {
            return Err("`gate10.delay_ms` must be low then high".into());
        }
        if s.lines[0] == 0 || s.lines[0] >= s.lines[1] {
            return Err("`lines` must be 1-based, first then one past the last".into());
        }
        if s.gate10.dir.trim().is_empty() || s.take3_dir.trim().is_empty() {
            return Err("a recording's folder is empty".into());
        }
        Ok(s)
    }

    /// Whether the reported lines exist in a script of `script_lines` lines.
    pub fn check_lines(&self, script_lines: usize) -> Result<(), String> {
        if self.lines[1] - 1 > script_lines {
            return Err(format!(
                "`lines` goes past the script's {script_lines} lines; the settings file is damaged"
            ));
        }
        Ok(())
    }

    /// The script lines reported, 1-based.
    pub fn line_range(&self) -> std::ops::Range<usize> {
        self.lines[0]..self.lines[1]
    }
}

/// The file's default place: under the first home folder known.
pub fn path_from(
    userprofile: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let non_empty = |v: Option<std::ffi::OsString>| v.filter(|s| !s.is_empty());
    let mut path = PathBuf::from(non_empty(userprofile).or(non_empty(home))?);
    path.extend(IN_HOME);
    Some(path)
}

/// The file's place for this process.
pub fn default_path() -> Option<PathBuf> {
    path_from(std::env::var_os("USERPROFILE"), std::env::var_os("HOME"))
}

/// `Ok(None)` when there is no file, so the caller says so and skips its private part.
pub fn load(path: Option<&Path>) -> Result<Option<Gate12Private>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    match std::fs::read_to_string(path) {
        Ok(text) => Gate12Private::parse(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// An ignored test's recording folder: the variable `var` when set, else `folder` of the
/// settings, under `root`.
pub fn recording_dir_from(
    var: &str,
    value: Option<std::ffi::OsString>,
    settings: Result<Option<Gate12Private>, String>,
    root: &Path,
    folder: fn(&Gate12Private) -> &str,
) -> Result<PathBuf, String> {
    if let Some(value) = value {
        return Ok(PathBuf::from(value));
    }
    match settings? {
        Some(s) => Ok(root.join(folder(&s))),
        None => Err(format!(
            "this test needs a private recording: {NOT_HERE}, and {var} is not set"
        )),
    }
}

/// [`recording_dir_from`] for this process; fails the test with the reason.
pub fn recording_dir(var: &str, root: &Path, folder: fn(&Gate12Private) -> &str) -> PathBuf {
    let settings = load(default_path().as_deref());
    recording_dir_from(var, std::env::var_os(var), settings, root, folder)
        .unwrap_or_else(|e| panic!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Invented values.
    const GOOD: &str = r#"
model_eval_dir = "fixtures/private/one"
take3_dir = "fixtures/private/three"
take3_far_ms = [[0, 1000], [5000, 6000]]
take3_near_ms = [1000, 2000]
lines = [2, 4]
key_words = ["valve"]
residue_ms = [[0, 1000], [5000, 6000]]
script_words = ["gasket"]

[gate10]
dir = "fixtures/private/two"
delay_ms = [1.5, 2.5]

[[takes]]
name = "a"
dir = "one"

[[windows]]
label = "first"
from_ms = 100
to_ms = 200
"#;

    #[test]
    fn gate12_private_parses_every_field() {
        let s = Gate12Private::parse(GOOD).unwrap();
        assert_eq!(s.model_eval_dir, "fixtures/private/one");
        assert_eq!(s.line_range(), 2..4);
        assert_eq!(s.key_words, ["valve"]);
        assert_eq!(s.residue_ms, [[0, 1000], [5000, 6000]]);
        assert_eq!(s.script_words, ["gasket"]);
        assert_eq!(s.gate10.delay_ms, [1.5, 2.5]);
        assert_eq!(s.gate10.dir, "fixtures/private/two");
        assert_eq!(s.take3_dir, "fixtures/private/three");
        assert_eq!(
            s.takes,
            [Take {
                name: "a".into(),
                dir: "one".into()
            }]
        );
        assert_eq!(
            s.windows,
            [Window {
                label: "first".into(),
                from_ms: 100,
                to_ms: 200
            }]
        );
    }

    #[test]
    fn gate12_private_refuses_what_it_cannot_use() {
        let no_window = GOOD.split("[[windows]]").next().unwrap();
        for bad in [
            "",
            no_window,
            &format!("windows = []\n{no_window}"),
            &GOOD.replace("delay_ms = [1.5, 2.5]", "delay_ms = [2.5, 1.5]"),
            &GOOD.replace("to_ms = 200", "to_ms = 100"),
            &GOOD.replace("[[0, 1000]", "[[1000, 1000]"),
            &GOOD.replace("lines = [2, 4]", "lines = [0, 4]"),
            &GOOD.replace("lines = [2, 4]", "lines = [4, 4]"),
            &GOOD.replace(r#"dir = "fixtures/private/two""#, r#"dir = " ""#),
            &GOOD.replace(
                r#"take3_dir = "fixtures/private/three""#,
                r#"take3_dir = """#,
            ),
            &format!("unknown = 1\n{GOOD}"),
        ] {
            assert!(Gate12Private::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn gate12_private_recording_dir_rule() {
        let root = Path::new("repo");
        let settings = || Ok(Some(Gate12Private::parse(GOOD).unwrap()));
        let gate10: fn(&Gate12Private) -> &str = |s| &s.gate10.dir;
        let take3: fn(&Gate12Private) -> &str = |s| &s.take3_dir;
        // The settings file, under the repo root.
        assert_eq!(
            recording_dir_from("VAR", None, settings(), root, gate10),
            Ok(root.join("fixtures/private/two"))
        );
        assert_eq!(
            recording_dir_from("VAR", None, settings(), root, take3),
            Ok(root.join("fixtures/private/three"))
        );
        // The variable wins and needs no file.
        assert_eq!(
            recording_dir_from("VAR", Some("elsewhere".into()), Ok(None), root, gate10),
            Ok(PathBuf::from("elsewhere"))
        );
        // No file and no variable says what is missing.
        let missing = recording_dir_from("VAR", None, Ok(None), root, gate10).unwrap_err();
        assert!(missing.contains("gate12.toml"), "{missing}");
        assert!(missing.contains("VAR"), "{missing}");
        // A damaged file stays an error, with its own message.
        let damaged =
            recording_dir_from("VAR", None, Err("damaged".into()), root, gate10).unwrap_err();
        assert!(damaged.contains("damaged"), "{damaged}");
    }

    #[test]
    fn gate12_private_lines_must_fit_the_script() {
        let s = Gate12Private::parse(GOOD).unwrap();
        assert_eq!(s.check_lines(3), Ok(()));
        assert!(s.check_lines(2).is_err());
    }

    #[test]
    fn gate12_private_missing_is_none_and_damaged_is_an_error() {
        let dir = std::env::temp_dir().join(format!("gate12-private-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gate12.toml");
        assert_eq!(load(None), Ok(None));
        assert_eq!(load(Some(&file)), Ok(None));
        std::fs::write(&file, GOOD).unwrap();
        assert!(load(Some(&file)).unwrap().is_some());
        std::fs::write(&file, "windows = 1").unwrap();
        assert!(load(Some(&file)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gate12_private_path_rule() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        let want = PathBuf::from("h")
            .join(".earshot-private")
            .join("gate12.toml");
        assert_eq!(path_from(os("h"), os("x")), Some(want.clone()));
        assert_eq!(path_from(os(""), os("h")), Some(want));
        assert_eq!(path_from(None, None), None);
    }

    /// Run with `-- --ignored` on the host that has the file.
    #[test]
    #[ignore = "needs the private settings file"]
    fn gate12_private_the_host_file_parses() {
        let s = load(default_path().as_deref()).unwrap().expect("the file");
        assert!(!s.takes.is_empty() && !s.key_words.is_empty() && !s.residue_ms.is_empty());
        assert!(!s.script_words.is_empty());
    }
}
