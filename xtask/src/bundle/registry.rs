//! Saving and restoring what a silent NSIS install and uninstall change in `HKCU`, for
//! `cargo xtask bundle-check`. tauri-bundler 2.10.3's `installer.nsi` (`currentUser` mode):
//!
//! - Section Uninstall deletes `...\CurrentVersion\Run\<product>` (the app's own "Start with
//!   Windows" value) and the key `...\CurrentVersion\Uninstall\<product>`.
//! - The install writes that key too, and `Software\<manufacturer>\<product>` (the default folder
//!   of the next install), creating `Software\<manufacturer>`. A silent uninstall leaves both.
//!
//! [`places`] names those four, [`restore_step`] is the rule, and [`RegistryGuard`] applies it:
//! everything is read before anything changes, with a run that cannot prove its save refused, and
//! put back on every path out, panics included, then read again and compared. A `reg.exe` that
//! cannot say "not found" in the English this code knows counts as not proven, not as absent.
//!
//! A value is saved alone (a `.reg` of that one entry), never with the rest of its key, so that
//! another program's `Run` entry changed meanwhile is left as it is. A killed process runs no drop
//! guard: its saved files stay in the guard's folder and the next run refuses to start until they
//! have been dealt with.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::proc::{CmdOutput, run_with_timeout};

/// The per-user autostart key.
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
/// The per-user Apps list.
pub const UNINSTALL_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall";
/// The first line of every `.reg` file.
const REG_HEADER: &str = "Windows Registry Editor Version 5.00";
/// How long one `reg.exe` run may take.
const REG_TIMEOUT: Duration = Duration::from_secs(30);

/// One thing in the registry that a run saves and restores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// One value of a key.
    Value {
        /// The key, `HKCU\...`.
        key: String,
        /// The value's name.
        name: String,
    },
    /// A whole key with its values and subkeys.
    Key(String),
    /// A key that may have to exist to hold another: only whether it exists is saved.
    Container(String),
}

/// What a read found at a [`Place`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// Nothing there.
    Absent,
    /// There, with the `.reg` text that holds it (empty for a [`Place::Container`]).
    Present(String),
}

/// What it takes to put a [`Place`] back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `reg delete /v`.
    DeleteValue,
    /// `reg import` of the saved value.
    ImportValue,
    /// `reg delete`.
    DeleteKey,
    /// `reg delete` (it may hold what the save did not), then `reg import`.
    ReplaceKey,
    /// `reg delete`, only when the key holds nothing.
    DeleteIfEmpty,
    /// `reg add`.
    CreateKey,
}

/// The places a silent install and uninstall of `product` change (module docs). A container comes
/// after the key inside it, so that it is empty when its turn comes.
#[must_use]
pub fn places(manufacturer: &str, product: &str) -> Vec<Place> {
    let vendor = format!(r"HKCU\Software\{manufacturer}");
    vec![
        Place::Value {
            key: RUN_KEY.to_owned(),
            name: product.to_owned(),
        },
        Place::Key(format!(r"{UNINSTALL_KEY}\{product}")),
        Place::Key(format!(r"{vendor}\{product}")),
        Place::Container(vendor),
    ]
}

/// The rule: nothing when `now` equals what was `saved`, else what puts it back.
#[must_use]
pub fn restore_step(place: &Place, saved: &Seen, now: &Seen) -> Option<Step> {
    if saved == now {
        return None;
    }
    Some(match (place, saved) {
        (Place::Value { .. }, Seen::Absent) => Step::DeleteValue,
        (Place::Value { .. }, Seen::Present(_)) => Step::ImportValue,
        (Place::Key(_), Seen::Absent) => Step::DeleteKey,
        (Place::Key(_), Seen::Present(_)) => Step::ReplaceKey,
        (Place::Container(_), Seen::Absent) => Step::DeleteIfEmpty,
        (Place::Container(_), Seen::Present(_)) => Step::CreateKey,
    })
}

/// Reads a `reg.exe` result: whether the thing exists. Exit 1 also means a denied or broken read,
/// so only the message that says "not found" counts as absent.
///
/// # Errors
/// Any other failure: nothing is known about the registry.
pub fn classify_read(success: bool, stderr: &str) -> Result<bool> {
    if success {
        return Ok(true);
    }
    if stderr.contains("unable to find the specified registry key or value") {
        return Ok(false);
    }
    bail!("reg.exe could not say what is there: {}", stderr.trim())
}

/// `HKCU\x` as `.reg` files write it.
///
/// # Errors
/// A key outside `HKCU`: this module never touches anything else.
pub fn long_key(key: &str) -> Result<String> {
    let rest = key
        .strip_prefix(r"HKCU\")
        .with_context(|| format!("{key} is not under HKCU"))?;
    Ok(format!(r"HKEY_CURRENT_USER\{rest}"))
}

/// The text of a `reg export` file, which is UTF-16 LE with a byte order mark.
///
/// # Errors
/// Anything else.
pub fn decode_reg_file(bytes: &[u8]) -> Result<String> {
    let body = bytes
        .strip_prefix(&[0xff, 0xfe])
        .context("a .reg file without a UTF-16 byte order mark")?;
    ensure!(
        body.len().is_multiple_of(2),
        "a .reg file with half a character"
    );
    let units: Vec<u16> = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    String::from_utf16(&units).context("a .reg file that is not valid UTF-16")
}

/// `text` as a `.reg` file.
#[must_use]
pub fn encode_reg_file(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    bytes
}

/// Whether `text` is an export of `key`: the proof that a saved key can be restored.
#[must_use]
pub fn export_holds_key(text: &str, key: &str) -> bool {
    let Ok(long) = long_key(key) else {
        return false;
    };
    let wanted = format!("[{long}]");
    let mut lines = text.lines().map(str::trim_end);
    lines.next() == Some(REG_HEADER) && lines.any(|l| l == wanted)
}

/// The entry of value `name` in an export of one key, continuation lines included; `None` when
/// the key has no such value.
#[must_use]
pub fn value_entry(export: &str, name: &str) -> Option<String> {
    let start = format!("\"{name}\"=");
    let mut lines = export.lines();
    let first = lines.find(|l| l.starts_with(&start))?;
    let mut entry = vec![first];
    // Long binary data continues with `\` at the line's end; a string ends with its quote.
    while entry.last().is_some_and(|l| l.ends_with('\\')) {
        match lines.next() {
            Some(next) => entry.push(next),
            None => break,
        }
    }
    Some(entry.join("\r\n"))
}

/// A `.reg` text holding one value of `key` and nothing else.
///
/// # Errors
/// A key outside `HKCU`.
pub fn value_reg_text(key: &str, entry: &str) -> Result<String> {
    Ok(format!(
        "{REG_HEADER}\r\n\r\n[{}]\r\n{entry}\r\n\r\n",
        long_key(key)?
    ))
}

/// Whether `reg query <key>` printed only the key's own name.
#[must_use]
pub fn query_shows_empty_key(stdout: &str, key: &str) -> bool {
    let Ok(long) = long_key(key) else {
        return false;
    };
    stdout
        .lines()
        .map(str::trim)
        .all(|l| l.is_empty() || l.eq_ignore_ascii_case(&long))
}

/// The system's `reg.exe`, never one found on `PATH`.
#[derive(Debug, Clone)]
pub struct Reg {
    exe: PathBuf,
}

impl Reg {
    /// The `reg.exe` of this Windows.
    ///
    /// # Errors
    /// `SystemRoot` is not set.
    pub fn system() -> Result<Self> {
        let root = std::env::var_os("SystemRoot").context("SystemRoot is not set")?;
        Ok(Self {
            exe: Path::new(&root).join("System32").join("reg.exe"),
        })
    }

    fn run(&self, args: &[&std::ffi::OsStr]) -> Result<CmdOutput> {
        let mut cmd = Command::new(&self.exe);
        cmd.args(args);
        let out = run_with_timeout(&mut cmd, REG_TIMEOUT)
            .with_context(|| format!("reg.exe {args:?} did not finish within the deadline"))?;
        out.require_complete("reg.exe's output")?;
        Ok(out)
    }

    fn change(&self, args: &[&str]) -> Result<()> {
        let args: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        let out = self.run(&args)?;
        ensure!(out.success, "reg.exe {args:?} failed: {}", out.describe());
        Ok(())
    }

    /// The text of `reg export <key>`, or `None` when the key does not exist.
    fn export(&self, key: &str, scratch: &Path) -> Result<Option<String>> {
        let file = scratch.join("export.reg");
        let out = self.run(&[
            "export".as_ref(),
            key.as_ref(),
            file.as_os_str(),
            "/y".as_ref(),
        ])?;
        if !classify_read(out.success, &out.stderr)? {
            return Ok(None);
        }
        let bytes = std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
        std::fs::remove_file(&file).with_context(|| format!("remove {}", file.display()))?;
        let text = decode_reg_file(&bytes)?;
        ensure!(
            export_holds_key(&text, key),
            "the export of {key} does not hold that key"
        );
        Ok(Some(text))
    }

    /// Whether `key` exists.
    ///
    /// # Errors
    /// `reg.exe` cannot say.
    pub fn key_exists(&self, key: &str) -> Result<bool> {
        let out = self.run(&["query".as_ref(), key.as_ref()])?;
        classify_read(out.success, &out.stderr)
    }

    /// Whether value `name` of `key` exists.
    ///
    /// # Errors
    /// `reg.exe` cannot say.
    pub fn value_exists(&self, key: &str, name: &str) -> Result<bool> {
        let out = self.run(&["query".as_ref(), key.as_ref(), "/v".as_ref(), name.as_ref()])?;
        classify_read(out.success, &out.stderr)
    }

    /// What `reg query <key> /ve` prints, or `None` when the key does not exist.
    ///
    /// # Errors
    /// `reg.exe` cannot say.
    pub fn default_value_text(&self, key: &str) -> Result<Option<String>> {
        let out = self.run(&["query".as_ref(), key.as_ref(), "/ve".as_ref()])?;
        Ok(classify_read(out.success, &out.stderr)?.then_some(out.stdout))
    }

    /// Sets value `name` of `key` to the string `data`, creating the key if needed.
    ///
    /// # Errors
    /// `reg.exe` fails.
    pub fn set_string(&self, key: &str, name: &str, data: &str) -> Result<()> {
        self.change(&["add", key, "/v", name, "/t", "REG_SZ", "/d", data, "/f"])
    }

    /// What is at `place` now.
    fn read(&self, place: &Place, scratch: &Path) -> Result<Seen> {
        Ok(match place {
            Place::Value { key, name } => match self
                .export(key, scratch)?
                .and_then(|text| value_entry(&text, name))
            {
                Some(entry) => Seen::Present(value_reg_text(key, &entry)?),
                None => Seen::Absent,
            },
            Place::Key(key) => self
                .export(key, scratch)?
                .map_or(Seen::Absent, Seen::Present),
            Place::Container(key) => {
                if self.key_exists(key)? {
                    Seen::Present(String::new())
                } else {
                    Seen::Absent
                }
            }
        })
    }

    /// Carries out `step` at `place`, with `file` the saved `.reg` file.
    fn apply(&self, step: Step, place: &Place, file: &Path) -> Result<()> {
        let saved = file.display().to_string();
        match (step, place) {
            (Step::DeleteValue, Place::Value { key, name }) => {
                self.change(&["delete", key, "/v", name, "/f"])
            }
            (Step::ImportValue, Place::Value { .. }) => self.change(&["import", &saved]),
            (Step::DeleteKey, Place::Key(key)) => self.change(&["delete", key, "/f"]),
            (Step::ReplaceKey, Place::Key(key)) => {
                if self.key_exists(key)? {
                    self.change(&["delete", key, "/f"])?;
                }
                self.change(&["import", &saved])
            }
            (Step::DeleteIfEmpty, Place::Container(key)) => {
                let out = self.run(&["query".as_ref(), key.as_ref()])?;
                ensure!(out.success, "reg query {key} failed: {}", out.describe());
                ensure!(
                    query_shows_empty_key(&out.stdout, key),
                    "{key} did not exist before and now holds something else: left as it is"
                );
                self.change(&["delete", key, "/f"])
            }
            (Step::CreateKey, Place::Container(key)) => self.change(&["add", key, "/f"]),
            (step, place) => bail!("{step:?} does not apply to {place:?}"),
        }
    }
}

/// Holds what was at some [`Place`]s and puts it back on the way out, on every path.
#[derive(Debug)]
pub struct RegistryGuard {
    reg: Reg,
    saved: Vec<(Place, Seen, PathBuf)>,
    dir: PathBuf,
    finished: bool,
}

impl RegistryGuard {
    /// Saves `places` into the new folder `dir`, changing nothing in the registry.
    ///
    /// # Errors
    /// The save cannot be proven: `dir` is left from a run that did not restore, a read fails or
    /// differs when repeated, or a saved file does not read back. The caller must then touch
    /// nothing.
    pub fn save(reg: Reg, places: Vec<Place>, dir: &Path) -> Result<Self> {
        ensure!(
            !dir.exists(),
            "{} is left from a run that did not finish restoring the registry. Compare its \
             `saved-*.reg` files with the registry, `reg import` what is missing, then delete \
             the folder",
            dir.display()
        );
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let result = Self::save_into(&reg, places, dir);
        let saved = match result {
            Ok(saved) => saved,
            Err(e) => {
                // Nothing has changed, so what was written here protects nothing.
                let _ = std::fs::remove_dir_all(dir);
                return Err(e.context("the registry could not be saved, so nothing was run"));
            }
        };
        Ok(Self {
            reg,
            saved,
            dir: dir.to_path_buf(),
            finished: false,
        })
    }

    fn save_into(reg: &Reg, places: Vec<Place>, dir: &Path) -> Result<Vec<(Place, Seen, PathBuf)>> {
        let mut saved = Vec::new();
        for (i, place) in places.into_iter().enumerate() {
            let seen = reg.read(&place, dir)?;
            ensure!(
                reg.read(&place, dir)? == seen,
                "{place:?} changed between two reads"
            );
            let file = dir.join(format!("saved-{i}.reg"));
            if let Seen::Present(text) = &seen
                && !text.is_empty()
            {
                std::fs::write(&file, encode_reg_file(text))
                    .with_context(|| format!("write {}", file.display()))?;
                let back =
                    std::fs::read(&file).with_context(|| format!("read {}", file.display()))?;
                ensure!(
                    decode_reg_file(&back)? == *text,
                    "{} does not read back",
                    file.display()
                );
            }
            saved.push((place, seen, file));
        }
        Ok(saved)
    }

    /// Puts every place back and reads it again; lists what differs or failed.
    fn restore(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for (place, saved, file) in &self.saved {
            let attempt = || -> Result<()> {
                let now = self.reg.read(place, &self.dir)?;
                if let Some(step) = restore_step(place, saved, &now) {
                    self.reg.apply(step, place, file)?;
                }
                ensure!(
                    self.reg.read(place, &self.dir)? == *saved,
                    "it differs from what was saved after restoring"
                );
                Ok(())
            };
            if let Err(e) = attempt() {
                problems.push(format!("{place:?}: {e:#}"));
            }
        }
        problems
    }

    /// Restores, verifies and removes the saved files.
    ///
    /// # Errors
    /// Something could not be put back; the saved files stay in the folder the error names.
    pub fn finish(mut self) -> Result<()> {
        self.finished = true;
        let problems = self.restore();
        ensure!(
            problems.is_empty(),
            "the registry was not restored; saved files kept in {}: {problems:?}",
            self.dir.display()
        );
        std::fs::remove_dir_all(&self.dir).with_context(|| format!("remove {}", self.dir.display()))
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let problems = self.restore();
        if problems.is_empty() {
            let _ = std::fs::remove_dir_all(&self.dir);
        } else {
            eprintln!(
                "bundle-check: the registry was not restored; saved files kept in {}: {problems:?}",
                self.dir.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(text: &str) -> Seen {
        Seen::Present(text.to_owned())
    }

    #[test]
    fn build_bundle_registry_places_are_the_three_and_the_folder_holding_one() {
        let p = places("vendor", "Earshot");
        assert_eq!(
            p,
            [
                Place::Value {
                    key: r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run".to_owned(),
                    name: "Earshot".to_owned(),
                },
                Place::Key(
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Earshot".to_owned()
                ),
                Place::Key(r"HKCU\Software\vendor\Earshot".to_owned()),
                Place::Container(r"HKCU\Software\vendor".to_owned()),
            ]
        );
    }

    #[test]
    fn build_bundle_registry_rule_restores_absent_and_present_for_every_kind() {
        let value = Place::Value {
            key: RUN_KEY.to_owned(),
            name: "x".to_owned(),
        };
        let key = Place::Key(r"HKCU\k".to_owned());
        let container = Place::Container(r"HKCU\c".to_owned());
        let (a, b) = (present("a"), present("b"));
        for place in [&value, &key, &container] {
            assert_eq!(restore_step(place, &Seen::Absent, &Seen::Absent), None);
            assert_eq!(restore_step(place, &a, &a), None);
        }
        assert_eq!(
            restore_step(&value, &Seen::Absent, &a),
            Some(Step::DeleteValue)
        );
        assert_eq!(
            restore_step(&value, &a, &Seen::Absent),
            Some(Step::ImportValue)
        );
        assert_eq!(restore_step(&value, &a, &b), Some(Step::ImportValue));
        assert_eq!(restore_step(&key, &Seen::Absent, &a), Some(Step::DeleteKey));
        assert_eq!(
            restore_step(&key, &a, &Seen::Absent),
            Some(Step::ReplaceKey)
        );
        assert_eq!(restore_step(&key, &a, &b), Some(Step::ReplaceKey));
        assert_eq!(
            restore_step(&container, &Seen::Absent, &present("")),
            Some(Step::DeleteIfEmpty)
        );
        assert_eq!(
            restore_step(&container, &present(""), &Seen::Absent),
            Some(Step::CreateKey)
        );
    }

    #[test]
    fn build_bundle_registry_only_the_not_found_message_means_absent() {
        assert!(classify_read(true, "").unwrap());
        let found = "ERROR: The system was unable to find the specified registry key or value.\r\n";
        assert!(!classify_read(false, found).unwrap());
        for unknown in ["", "ERROR: Access is denied.", "FEHLER: nicht gefunden"] {
            assert!(classify_read(false, unknown).is_err(), "{unknown}");
        }
    }

    #[test]
    fn build_bundle_registry_files_round_trip_and_refuse_anything_else() {
        let text = "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\é]\r\n";
        let bytes = encode_reg_file(text);
        assert_eq!(&bytes[..2], &[0xff, 0xfe]);
        assert_eq!(decode_reg_file(&bytes).unwrap(), text);
        assert!(decode_reg_file(b"Windows Registry").is_err(), "no mark");
        assert!(decode_reg_file(&[0xff, 0xfe, 0x41]).is_err(), "half a unit");
        assert!(
            decode_reg_file(&[0xff, 0xfe, 0x00, 0xd8]).is_err(),
            "a lone surrogate"
        );
    }

    #[test]
    fn build_bundle_registry_an_export_must_hold_its_key() {
        let text = "Windows Registry Editor Version 5.00\r\n\r\n\
                    [HKEY_CURRENT_USER\\Software\\a]\r\n\"x\"=\"1\"\r\n\r\n\
                    [HKEY_CURRENT_USER\\Software\\a\\b]\r\n";
        assert!(export_holds_key(text, r"HKCU\Software\a"));
        assert!(export_holds_key(text, r"HKCU\Software\a\b"));
        assert!(!export_holds_key(text, r"HKCU\Software\other"));
        assert!(!export_holds_key("", r"HKCU\Software\a"));
        assert!(!export_holds_key(
            "[HKEY_CURRENT_USER\\Software\\a]",
            r"HKCU\Software\a"
        ));
        assert!(long_key(r"HKLM\Software\a").is_err());
    }

    #[test]
    fn build_bundle_registry_a_value_is_saved_without_its_neighbours() {
        let export = "Windows Registry Editor Version 5.00\r\n\r\n\
                      [HKEY_CURRENT_USER\\Run]\r\n\
                      \"Other\"=\"C:\\\\other.exe\"\r\n\
                      \"Earshot\"=\"\\\"C:\\\\a b\\\\earshot.exe\\\" --autostart\"\r\n\
                      \"Binary\"=hex:01,02,\\\r\n  03,04\r\n\
                      \"EarshotTwo\"=\"no\"\r\n";
        assert_eq!(
            value_entry(export, "Earshot").unwrap(),
            "\"Earshot\"=\"\\\"C:\\\\a b\\\\earshot.exe\\\" --autostart\""
        );
        assert_eq!(
            value_entry(export, "Binary").unwrap(),
            "\"Binary\"=hex:01,02,\\\r\n  03,04"
        );
        assert_eq!(value_entry(export, "Absent"), None);
        assert_eq!(value_entry(export, "Earsho"), None);
        let file = value_reg_text(r"HKCU\Run", "\"Earshot\"=\"x\"").unwrap();
        assert!(export_holds_key(&file, r"HKCU\Run"));
        assert!(!file.contains("Other"));
        assert_eq!(value_entry(&file, "Earshot").unwrap(), "\"Earshot\"=\"x\"");
    }

    #[test]
    fn build_bundle_registry_a_container_is_deleted_only_when_empty() {
        let key = r"HKCU\Software\vendor";
        assert!(query_shows_empty_key(
            "\r\nHKEY_CURRENT_USER\\Software\\vendor\r\n",
            key
        ));
        assert!(query_shows_empty_key("", key));
        assert!(!query_shows_empty_key(
            "\r\nHKEY_CURRENT_USER\\Software\\vendor\r\n    x    REG_SZ    1\r\n",
            key
        ));
        assert!(!query_shows_empty_key(
            "\r\nHKEY_CURRENT_USER\\Software\\vendor\\Other\r\n",
            key
        ));
    }

    /// A folder of its own for one real-registry case.
    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("earshot-reg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    #[ignore = "writes to the real registry under a per-pid HKCU\\Software key, which it deletes; \
                run with cargo test -p xtask build_bundle_registry -- --ignored"]
    fn build_bundle_registry_guard_restores_the_real_registry_even_after_a_panic() {
        let reg = Reg::system().unwrap();
        let root = format!(
            r"HKCU\Software\EarshotXtaskRegistryTest{}",
            std::process::id()
        );
        let run_key = format!(r"{root}\run");
        let kept = format!(r"{root}\kept");
        let missing = format!(r"{root}\missing");
        let vendor = format!(r"{root}\vendor");
        let cleanup = |reg: &Reg| {
            let _ = reg.change(&["delete", &root, "/f"]);
        };
        cleanup(&reg);
        reg.set_string(&run_key, "App", "\"C:\\a b\\app.exe\" --flag")
            .unwrap();
        reg.set_string(&run_key, "Neighbour", "before").unwrap();
        reg.set_string(&kept, "x", "1").unwrap();
        reg.set_string(&format!(r"{kept}\sub"), "y", "2").unwrap();
        let places = vec![
            Place::Value {
                key: run_key.clone(),
                name: "App".to_owned(),
            },
            Place::Value {
                key: run_key.clone(),
                name: "AbsentValue".to_owned(),
            },
            Place::Key(kept.clone()),
            Place::Key(missing.clone()),
            Place::Key(format!(r"{vendor}\product")),
            Place::Container(vendor.clone()),
        ];

        // What an install and uninstall do, and a panic on the way out.
        let dir = scratch("panic");
        let guard = RegistryGuard::save(reg.clone(), places.clone(), &dir).unwrap();
        let saved_kept = reg.export(&kept, &dir).unwrap().unwrap();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = guard;
            reg.change(&["delete", &run_key, "/v", "App", "/f"])
                .unwrap();
            reg.set_string(&run_key, "AbsentValue", "added").unwrap();
            reg.set_string(&run_key, "Neighbour", "changed meanwhile")
                .unwrap();
            reg.change(&["delete", &kept, "/f"]).unwrap();
            reg.set_string(&missing, "x", "added").unwrap();
            reg.set_string(&format!(r"{vendor}\product"), "path", "C:\\install")
                .unwrap();
            panic!("on the way out");
        }));
        assert!(panicked.is_err());
        assert!(!dir.exists(), "a verified restore removes the saved files");
        let run = reg.export(&run_key, &scratch_dir()).unwrap().unwrap();
        assert_eq!(
            value_entry(&run, "App").unwrap(),
            "\"App\"=\"\\\"C:\\\\a b\\\\app.exe\\\" --flag\""
        );
        assert_eq!(value_entry(&run, "AbsentValue"), None);
        assert_eq!(
            value_entry(&run, "Neighbour").unwrap(),
            "\"Neighbour\"=\"changed meanwhile\"",
            "another program's value is not put back"
        );
        assert_eq!(
            reg.export(&kept, &scratch_dir()).unwrap().unwrap(),
            saved_kept
        );
        assert!(!reg.key_exists(&missing).unwrap());
        assert!(!reg.key_exists(&vendor).unwrap());

        // A key that gained a value is put back exactly, by finish.
        let dir = scratch("finish");
        let guard = RegistryGuard::save(reg.clone(), vec![Place::Key(kept.clone())], &dir).unwrap();
        reg.set_string(&kept, "added", "later").unwrap();
        guard.finish().unwrap();
        assert_eq!(
            reg.export(&kept, &scratch_dir()).unwrap().unwrap(),
            saved_kept
        );
        assert!(!dir.exists());

        // A container holding something it did not hold before is reported and left.
        let dir = scratch("container");
        let guard =
            RegistryGuard::save(reg.clone(), vec![Place::Container(vendor.clone())], &dir).unwrap();
        reg.set_string(&vendor, "foreign", "1").unwrap();
        let e = guard.finish().unwrap_err().to_string();
        assert!(e.contains("left as it is"), "{e}");
        assert!(reg.value_exists(&vendor, "foreign").unwrap());
        assert!(dir.exists(), "saved files stay after a failed restore");
        // That folder makes the next run refuse before it touches anything.
        let e = RegistryGuard::save(reg.clone(), vec![Place::Key(kept.clone())], &dir)
            .unwrap_err()
            .to_string();
        assert!(e.contains("did not finish restoring"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(scratch_dir());
        cleanup(&reg);
        assert!(!reg.key_exists(&root).unwrap());
    }

    /// A folder for reads outside any guard.
    fn scratch_dir() -> PathBuf {
        let d = scratch("reads");
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
