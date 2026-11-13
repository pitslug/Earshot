//! The General page of Settings (T7.4h): "start Earshot with Windows". The theme on the same
//! page is a browser-side preference (T9.1n, `EarshotThemeProvider`) and never reaches Rust.
//!
//! Starting with Windows is the per-user `Run` key (`HKCU\Software\Microsoft\Windows\
//! CurrentVersion\Run`, value [`RUN_VALUE`], the app's own exe in quotes with
//! [`crate::shell::AUTOSTART_ARG`], so it starts in the tray: T7.5). The key itself is
//! the setting: nothing is stored in Earshot's database. It is read and written with the
//! system's `reg.exe` through `earshot_proc` (kill-on-close job, deadline, complete output),
//! so the app needs no new `unsafe` and no new dependency; the command line holds only
//! fixed text and the exe path. Earshot is "on" only when the value names this exe (a value
//! left by an older copy elsewhere reads as off; turning it on replaces it and turning it
//! off removes it). Tests use
//! [`Autostart`] fakes; the real key is touched only by the ignored round-trip test.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::ipc::IpcError;

/// The `Run` key.
pub const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
/// The value name under it.
pub const RUN_VALUE: &str = "Earshot";

/// Reading or changing "start with Windows".
pub trait Autostart: Send + Sync {
    /// Whether Earshot starts with Windows.
    ///
    /// # Errors
    /// `autostart` when the setting cannot be read.
    fn enabled(&self) -> Result<bool, IpcError>;
    /// Turns it on or off.
    ///
    /// # Errors
    /// `autostart` when the setting cannot be changed.
    fn set(&self, on: bool) -> Result<(), IpcError>;
}

fn autostart_error(message: impl Into<String>) -> IpcError {
    IpcError {
        kind: "autostart".into(),
        message: message.into(),
        retryable: false,
    }
}

/// The General page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct GeneralView {
    /// Earshot starts with Windows (`null` when it cannot be read).
    pub start_with_windows: Option<bool>,
    /// Why it cannot be read.
    pub start_error: Option<String>,
    /// The app version.
    pub version: String,
    /// The global shortcut that starts and stops a meeting (T7.5).
    pub shortcut: String,
    /// Why that shortcut does nothing here (`null` when it works).
    pub shortcut_error: Option<String>,
}

/// The page.
#[must_use]
pub fn general_view(autostart: &dyn Autostart, shortcut_error: Option<String>) -> GeneralView {
    let (start_with_windows, start_error) = match autostart.enabled() {
        Ok(on) => (Some(on), None),
        Err(e) => (None, Some(e.message)),
    };
    GeneralView {
        start_with_windows,
        start_error,
        version: env!("CARGO_PKG_VERSION").into(),
        shortcut: crate::shell::SHORTCUT_TEXT.into(),
        shortcut_error,
    }
}

/// Turns "start with Windows" on or off and returns the page as it is then.
///
/// # Errors
/// `autostart` when the setting cannot be changed.
pub fn apply_start_with_windows(
    autostart: &dyn Autostart,
    on: bool,
    shortcut_error: Option<String>,
) -> Result<GeneralView, IpcError> {
    autostart.set(on)?;
    Ok(general_view(autostart, shortcut_error))
}

/// The `Run` value data for `exe`: the path in quotes and the argument that keeps it in the tray.
#[must_use]
pub fn run_command(exe: &Path) -> String {
    format!("\"{}\" {}", exe.display(), crate::shell::AUTOSTART_ARG)
}

/// The data of value `name` in `reg query` output, if it is listed as a string.
#[must_use]
pub fn parse_query(output: &str, name: &str) -> Option<String> {
    output.lines().find_map(|line| {
        // Registry value names are case-insensitive.
        let line = line.trim_start();
        let head = line.get(..name.len())?;
        if !head.eq_ignore_ascii_case(name) {
            return None;
        }
        let rest = &line[name.len()..];
        // The name is followed by whitespace, the type, whitespace and the data.
        if !rest.starts_with([' ', '\t']) {
            return None;
        }
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix("REG_SZ")
            .or_else(|| rest.strip_prefix("REG_EXPAND_SZ"))?;
        Some(rest.trim().to_owned())
    })
}

/// The per-user `Run` key through `reg.exe` (module docs).
#[derive(Debug, Clone)]
pub struct RunKey {
    reg: PathBuf,
    key: String,
    name: String,
    command: String,
}

/// What the page says when Windows cannot be asked whether Earshot starts with it.
pub const READ_FAILED: &str = "Earshot could not read this setting from Windows.";
/// What the page says when the setting cannot be changed.
pub const CHANGE_FAILED: &str = "Earshot could not change this setting in Windows. Try again.";

/// How long one `reg.exe` run may take.
const REG_DEADLINE: Duration = Duration::from_secs(10);

impl RunKey {
    /// The `Run` value [`RUN_VALUE`] for this process's exe.
    ///
    /// # Errors
    /// `autostart` when the exe or the Windows folder is unknown.
    pub fn for_this_app() -> Result<Self, IpcError> {
        let exe = std::env::current_exe()
            .map_err(|e| autostart_error(format!("cannot find Earshot's program file: {e}")))?;
        Self::new(RUN_KEY, RUN_VALUE, &exe)
    }

    /// Value `name` under `key` naming `exe` (tests use their own name).
    ///
    /// # Errors
    /// `autostart` when the Windows folder is unknown.
    pub fn new(key: &str, name: &str, exe: &Path) -> Result<Self, IpcError> {
        let windows = std::env::var_os("SystemRoot")
            .ok_or_else(|| autostart_error("the Windows folder is unknown (SystemRoot)"))?;
        Ok(Self {
            reg: PathBuf::from(windows).join("System32").join("reg.exe"),
            key: key.into(),
            name: name.into(),
            command: run_command(exe),
        })
    }

    /// Runs `reg.exe` with `args`; `failure` is what the page says when it cannot run.
    fn reg(
        &self,
        args: &[&str],
        failure: &'static str,
    ) -> Result<earshot_proc::ChildOutput, IpcError> {
        reg_at(&self.reg, args, failure)
    }
}

fn reg_at(
    reg: &Path,
    args: &[&str],
    failure: &'static str,
) -> Result<earshot_proc::ChildOutput, IpcError> {
    let spec = earshot_proc::ChildSpec {
        program: reg.to_path_buf(),
        args: args.iter().map(OsString::from).collect(),
        current_dir: std::env::temp_dir(),
        env_remove: Vec::new(),
        env_set: Vec::new(),
        stdin: Vec::new(),
        deadline: REG_DEADLINE,
        max_output: 64 * 1024,
    };
    earshot_proc::run(&spec, &|| false)
        .and_then(earshot_proc::ChildOutput::require_complete)
        .map_err(|_| autostart_error(failure))
}

/// Runs the system's `reg.exe` (the notifications' app id, T7.5).
///
/// # Errors
/// `autostart` with `failure` when it cannot run.
#[cfg(windows)]
pub fn run_reg(
    args: &[&str],
    failure: &'static str,
) -> Result<earshot_proc::ChildOutput, IpcError> {
    let windows = std::env::var_os("SystemRoot")
        .ok_or_else(|| autostart_error("the Windows folder is unknown (SystemRoot)"))?;
    reg_at(
        &PathBuf::from(windows).join("System32").join("reg.exe"),
        args,
        failure,
    )
}

impl Autostart for RunKey {
    fn enabled(&self) -> Result<bool, IpcError> {
        let out = self.reg(&["query", &self.key, "/v", &self.name], READ_FAILED)?;
        if !out.status.success() {
            // `reg query` fails when the value does not exist.
            return Ok(false);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(parse_query(&text, &self.name).is_some_and(|d| d.eq_ignore_ascii_case(&self.command)))
    }

    fn set(&self, on: bool) -> Result<(), IpcError> {
        if on {
            let out = self.reg(
                &[
                    "add",
                    &self.key,
                    "/v",
                    &self.name,
                    "/t",
                    "REG_SZ",
                    "/d",
                    &self.command,
                    "/f",
                ],
                CHANGE_FAILED,
            )?;
            if !out.status.success() {
                return Err(autostart_error(
                    "Windows refused to start Earshot at sign-in.",
                ));
            }
        } else if self.enabled_any()? {
            let out = self.reg(
                &["delete", &self.key, "/v", &self.name, "/f"],
                CHANGE_FAILED,
            )?;
            if !out.status.success() {
                return Err(autostart_error(
                    "Windows refused to stop starting Earshot at sign-in.",
                ));
            }
        }
        Ok(())
    }
}

impl RunKey {
    /// Whether the value exists at all (whatever it names).
    fn enabled_any(&self) -> Result<bool, IpcError> {
        Ok(self
            .reg(&["query", &self.key, "/v", &self.name], CHANGE_FAILED)?
            .status
            .success())
    }
}

/// "Start with Windows" when the `Run` key cannot be named (no exe path or Windows folder).
struct Unavailable(IpcError);

impl Autostart for Unavailable {
    fn enabled(&self) -> Result<bool, IpcError> {
        Err(self.0.clone())
    }
    fn set(&self, _: bool) -> Result<(), IpcError> {
        Err(self.0.clone())
    }
}

/// The General page: start with Windows, the theme and the version.
#[tauri::command]
#[specta::specta]
pub async fn general_settings(
    shortcut: tauri::State<'_, crate::shell::ShortcutStatus>,
) -> Result<GeneralView, IpcError> {
    let shortcut_error = shortcut.error();
    crate::ipc::blocking(move || {
        Ok(match RunKey::for_this_app() {
            Ok(key) => general_view(&key, shortcut_error),
            Err(e) => general_view(&Unavailable(e), shortcut_error),
        })
    })
    .await
}

/// Whose provisional words a meeting shows (T9.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum PreviewsChoice {
    /// None.
    Off,
    /// The people the user is talking to in a call; everyone in the room.
    OtherPeople,
    /// Also the user's own voice in a call.
    Everyone,
}

impl From<earshot_runtime::preview_settings::PreviewSetting> for PreviewsChoice {
    fn from(setting: earshot_runtime::preview_settings::PreviewSetting) -> Self {
        use earshot_runtime::preview_settings::PreviewSetting;
        match setting {
            PreviewSetting::Off => Self::Off,
            PreviewSetting::OtherPeople => Self::OtherPeople,
            PreviewSetting::Everyone => Self::Everyone,
        }
    }
}

impl From<PreviewsChoice> for earshot_runtime::preview_settings::PreviewSetting {
    fn from(choice: PreviewsChoice) -> Self {
        match choice {
            PreviewsChoice::Off => Self::Off,
            PreviewsChoice::OtherPeople => Self::OtherPeople,
            PreviewsChoice::Everyone => Self::Everyone,
        }
    }
}

/// Provisional words on this computer (T9.1mb, T9.14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct PreviewsView {
    /// The choice that reads selected.
    pub choice: PreviewsChoice,
    /// The user can change it here.
    pub can_change: bool,
    /// Something to say about this machine.
    pub note: Option<String>,
    /// A stored value this version does not know, read as automatic.
    pub unknown: Option<String>,
}

fn previews_view(services: &earshot_runtime::Services) -> Result<PreviewsView, IpcError> {
    previews_view_on(
        services,
        &earshot_runtime::host::model_dir(&services.data),
        &earshot_runtime::backends::accelerator_order(),
        &earshot_runtime::hardware(),
    )
}

/// The previews page for a build with accelerator `order` on `hardware`, judged by where the next
/// meeting's transcription model runs (T12.30): the model and order of
/// `host::transcription_setup`, placed by `recommend::runs_on_here`, so a Parakeet model without
/// its NPU version is judged on the processor, as its previews run.
fn previews_view_on(
    services: &earshot_runtime::Services,
    models: &earshot_runtime::backends::ModelDir,
    order: &[earshot_runtime::backends::Accelerator],
    hardware: &earshot_runtime::earshot_hardware::Hardware,
) -> Result<PreviewsView, IpcError> {
    use earshot_runtime::backends::Accelerator;
    let (choice, unknown, setup) = {
        let db = services.store();
        let (choice, unknown) = earshot_runtime::preview_settings::preview_choice(&db)?;
        let setup = earshot_runtime::host::transcription_setup(&db, models, order, hardware)?;
        (choice, unknown, setup)
    };
    // Its NPU files are looked at with the store lock free.
    let runs_on =
        earshot_runtime::recommend::runs_on_here(models, &setup.choice.model, &setup.order);
    let lead = Accelerator::of(runs_on).unwrap_or(Accelerator::Cpu);
    let page = earshot_runtime::preview_settings::preview_page(choice, lead, hardware);
    Ok(PreviewsView {
        choice: page.shown.into(),
        can_change: page.can_change,
        note: page.note.map(str::to_owned),
        unknown,
    })
}

/// Whether meetings show provisional words.
#[tauri::command]
#[specta::specta]
pub async fn previews_settings(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<PreviewsView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || previews_view(&services)).await
}

/// Stores whose provisional words meetings show, from the next meeting.
#[tauri::command]
#[specta::specta]
pub async fn set_previews(
    state: tauri::State<'_, crate::ipc::AppState>,
    choice: PreviewsChoice,
) -> Result<PreviewsView, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || store_previews(&services, choice)).await
}

fn store_previews(
    services: &earshot_runtime::Services,
    choice: PreviewsChoice,
) -> Result<PreviewsView, IpcError> {
    earshot_runtime::preview_settings::set_previews(&mut services.store(), choice.into())?;
    previews_view(services)
}

/// Turns "start Earshot with Windows" on or off (`autostart` when Windows refuses).
#[tauri::command]
#[specta::specta]
pub async fn set_start_with_windows(
    shortcut: tauri::State<'_, crate::shell::ShortcutStatus>,
    on: bool,
) -> Result<GeneralView, IpcError> {
    let shortcut_error = shortcut.error();
    crate::ipc::blocking(move || {
        apply_start_with_windows(&RunKey::for_this_app()?, on, shortcut_error)
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// An in-memory setting.
    struct Fake {
        on: Mutex<bool>,
        fail: bool,
    }

    impl Autostart for Fake {
        fn enabled(&self) -> Result<bool, IpcError> {
            if self.fail {
                return Err(autostart_error("cannot read"));
            }
            Ok(*self.on.lock().unwrap())
        }
        fn set(&self, on: bool) -> Result<(), IpcError> {
            if self.fail {
                return Err(autostart_error("cannot change"));
            }
            *self.on.lock().unwrap() = on;
            Ok(())
        }
    }

    #[test]
    fn general_view_shows_the_setting_and_the_version() {
        let fake = Fake {
            on: Mutex::new(false),
            fail: false,
        };
        let v = general_view(&fake, None);
        assert_eq!((v.start_with_windows, v.start_error), (Some(false), None));
        assert_eq!(v.shortcut, crate::shell::SHORTCUT_TEXT);
        assert_eq!(v.shortcut_error, None);
        assert_eq!(v.version, env!("CARGO_PKG_VERSION"));
        let v = apply_start_with_windows(&fake, true, None).unwrap();
        assert_eq!(v.start_with_windows, Some(true));
        let v = apply_start_with_windows(&fake, false, Some("taken".into())).unwrap();
        assert_eq!(v.shortcut_error.as_deref(), Some("taken"));
        assert_eq!(v.start_with_windows, Some(false));
    }

    #[test]
    fn general_view_reports_a_setting_it_cannot_read_or_change() {
        let fake = Fake {
            on: Mutex::new(false),
            fail: true,
        };
        let v = general_view(&fake, None);
        assert_eq!(v.start_with_windows, None);
        assert_eq!(v.start_error.as_deref(), Some("cannot read"));
        assert_eq!(
            apply_start_with_windows(&fake, true, None)
                .unwrap_err()
                .kind,
            "autostart"
        );
    }

    #[test]
    fn general_run_value_is_this_exe_in_quotes_in_the_tray_and_is_read_back_from_reg_output() {
        let exe = Path::new(r"C:\Program Files\Earshot\earshot.exe");
        let cmd = run_command(exe);
        assert_eq!(cmd, r#""C:\Program Files\Earshot\earshot.exe" --autostart"#);
        let out = format!(
            "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    \
             Earshot    REG_SZ    {cmd}\r\n\r\n"
        );
        assert_eq!(parse_query(&out, "Earshot").as_deref(), Some(cmd.as_str()));
        // Another value whose name starts the same is not ours.
        let other = "    EarshotOld    REG_SZ    \"C:\\old.exe\"\r\n";
        // Windows keeps the case a value was written with, and matches names in any case.
        let lower = format!("    earshot    REG_SZ    {cmd}\r\n");
        assert_eq!(
            parse_query(&lower, "Earshot").as_deref(),
            Some(cmd.as_str())
        );
        assert_eq!(parse_query(other, "Earshot"), None);
        assert_eq!(parse_query("", "Earshot"), None);
        let expand = "    Earshot    REG_EXPAND_SZ    \"%LOCALAPPDATA%\\e.exe\"";
        assert_eq!(
            parse_query(expand, "Earshot").as_deref(),
            Some("\"%LOCALAPPDATA%\\e.exe\"")
        );
    }

    /// Writes and removes a test value under the real `Run` key (never `Earshot`).
    #[test]
    #[ignore = "changes the user's Run key (a test value, removed at the end); run: cargo test -p earshot-desktop general_run_key -- --ignored"]
    fn general_run_key_round_trips_a_test_value() {
        let name = format!("EarshotTest-{}", std::process::id());
        let key = RunKey::new(RUN_KEY, &name, Path::new(r"C:\nowhere\earshot-test.exe")).unwrap();
        struct Cleanup<'a>(&'a RunKey);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.set(false);
            }
        }
        let _cleanup = Cleanup(&key);
        assert!(!key.enabled().unwrap());
        key.set(true).unwrap();
        assert!(key.enabled().unwrap());
        key.set(false).unwrap();
        assert!(!key.enabled().unwrap());
        key.set(false).unwrap();
    }

    #[test]
    fn previews_choice_is_three_named_values_on_the_wire() {
        for (choice, wire) in [
            (PreviewsChoice::Off, "\"off\""),
            (PreviewsChoice::OtherPeople, "\"other_people\""),
            (PreviewsChoice::Everyone, "\"everyone\""),
        ] {
            assert_eq!(serde_json::to_string(&choice).unwrap(), wire);
            assert_eq!(
                serde_json::from_str::<PreviewsChoice>(wire).unwrap(),
                choice
            );
        }
    }

    #[test]
    fn previews_each_choice_is_stored_and_read_back() {
        let dir = crate::testutil::TempDir::new("previews");
        let services = crate::testutil::services(&dir);
        let first = previews_view(&services).unwrap();
        assert_eq!(first.unknown, None);
        for (choice, stored) in [
            (PreviewsChoice::Off, &b"off"[..]),
            (PreviewsChoice::OtherPeople, b"on"),
            (PreviewsChoice::Everyone, b"everyone"),
        ] {
            let view = store_previews(&services, choice).unwrap();
            assert_eq!(
                services
                    .store()
                    .local_value(earshot_runtime::preview_settings::PREVIEWS_KEY)
                    .unwrap()
                    .as_deref(),
                Some(stored)
            );
            // A machine that cannot run them at all (the NPU, a small card) shows Off whatever
            // is stored; everywhere else the stored choice reads back.
            let expected = if view.can_change {
                choice
            } else {
                PreviewsChoice::Off
            };
            assert_eq!(view.choice, expected, "{view:?}");
            assert_eq!(view.unknown, None);
        }
    }

    /// T12.30: the page judges where the chosen transcription model runs, not what the build
    /// leads with: on an NPU machine a Parakeet model without its NPU version runs on the
    /// processor, and so do its previews.
    #[test]
    fn previews_page_follows_where_the_chosen_model_runs() {
        use earshot_runtime::backends::{Accelerator, AsrModel, ModelDir, asr_model};
        use earshot_runtime::earshot_hardware::{Cpu, Hardware, Npu};
        use earshot_runtime::preview_settings::NOTE_EVERYONE_NPU;
        const GIB: u64 = 1 << 30;
        let dir = crate::testutil::TempDir::new("previews-model");
        let services = crate::testutil::services(&dir);
        let models = ModelDir::new(dir.0.join("models"));
        let order = [Accelerator::Qnn, Accelerator::Cpu];
        let hw = Hardware {
            cpu: Cpu {
                name: None,
                physical_cores: Some(12),
                logical_cores: Some(12),
            },
            gpus: Vec::new(),
            npus: vec![Npu {
                name: "An NPU".into(),
                driver_version: Some("1.0".into()),
            }],
            total_memory: Some(32 * GIB),
        };
        let model = "parakeet-tdt-0.6b-v2";
        let before = crate::models::settings_view(&services).unwrap().value;
        crate::models::set_settings(
            &services,
            &crate::models::ModelSettingsValue {
                transcription: Some(model.into()),
                ..before
            },
        )
        .unwrap();
        store_previews(&services, PreviewsChoice::Everyone).unwrap();
        // No NPU version: the model runs on the processor, so no NPU note.
        let cpu = previews_view_on(&services, &models, &order, &hw).unwrap();
        assert_ne!(cpu.note.as_deref(), Some(NOTE_EVERYONE_NPU), "{cpu:?}");
        assert!(cpu.can_change);
        // With its NPU version the model runs on the NPU, and the page says so.
        let Ok(AsrModel::Parakeet(p)) = asr_model(model) else {
            panic!("a Parakeet model");
        };
        for rel in p.qnn_files() {
            let f = models.root().join(rel);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"x").unwrap();
        }
        let npu = previews_view_on(&services, &models, &order, &hw).unwrap();
        assert_eq!(npu.note.as_deref(), Some(NOTE_EVERYONE_NPU), "{npu:?}");
    }
}
