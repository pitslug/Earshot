//! The General page of Settings (T7.4h): "start Earshot with Windows". The theme on the same
//! page is a browser-side preference (T9.1n, `EarshotThemeProvider`) and never reaches Rust.
//!
//! Starting with Windows is the per-user `Run` key (`HKCU\Software\Microsoft\Windows\
//! CurrentVersion\Run`, value [`RUN_VALUE`], the app's own exe in quotes with
//! [`crate::shell::AUTOSTART_ARG`], so it starts in the tray: T7.5). The key itself is
//! the setting: nothing is stored in Earshot's database. It is read and written with the
//! system's `reg.exe` through `earshot_proc` (kill-on-close job, deadline, complete output),
//! so the app needs no new `unsafe` and no new dependency; the command line holds only
//! fixed text and the exe path. The value is read from the file `reg export` writes, which is
//! UTF-16 whatever the console's code page (T14.9, [`parse_export`]); `reg query`'s text is
//! never parsed. Earshot is "on" only when the value names this exe (a value
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
    IpcError::new("autostart", message)
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

/// The first line of a registration file `reg export` writes (version 5: UTF-16).
const EXPORT_HEADER: &str = "Windows Registry Editor Version 5.00";

/// `bytes` is not a version 5 registration file.
#[derive(Debug, PartialEq, Eq)]
pub struct NotAnExport;

/// The data of string value `name` directly under the exported key in a registration file
/// (`reg export`), `None` when the key has no such string value.
///
/// This is how the `Run` value is read (T14.9): `reg query` prints the data in the console's
/// code page, so a path with a letter outside it (a profile folder with an accent) came back
/// changed and the setting read as off. The file is UTF-16 whatever the code page: a byte order
/// mark, [`EXPORT_HEADER`], then `[key]` and its values as `"name"="data"` with `\` and `"`
/// escaped by `\`; data of another type follows `=` unquoted (`dword:`, `hex(2):`, ...), and the
/// key's subkeys follow as further `[key]` sections, which are not this key's values.
///
/// # Errors
/// [`NotAnExport`] for anything else: never a guess.
pub fn parse_export(bytes: &[u8], name: &str) -> Result<Option<String>, NotAnExport> {
    let (units, odd) = bytes.as_chunks::<2>();
    let Some((bom, units)) = units.split_first() else {
        return Err(NotAnExport);
    };
    if *bom != [0xFF, 0xFE] || !odd.is_empty() {
        return Err(NotAnExport);
    }
    let units: Vec<u16> = units.iter().map(|u| u16::from_le_bytes(*u)).collect();
    // The whole key is in the file, and Windows lets a value hold half of a surrogate pair:
    // another program's such value must not make this one unreadable. It decodes to U+FFFD
    // there; the value compared with this exe's command is still compared exactly.
    let text = String::from_utf16_lossy(&units);
    let rest = text
        .trim_start()
        .strip_prefix(EXPORT_HEADER)
        .ok_or(NotAnExport)?;
    let mut chars = rest.chars().peekable();
    let mut sections = 0;
    loop {
        // The start of a line.
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        match chars.peek() {
            None => return Ok(None),
            Some('[') => {
                sections += 1;
                if sections > 1 {
                    // A subkey: the key's own values are over.
                    return Ok(None);
                }
                skip_line(&mut chars);
            }
            Some('"') => {
                let value = quoted(&mut chars).ok_or(NotAnExport)?;
                if chars.next() != Some('=') {
                    return Err(NotAnExport);
                }
                // Registry value names are case-insensitive.
                let ours = value.eq_ignore_ascii_case(name);
                if chars.peek() == Some(&'"') {
                    let data = quoted(&mut chars).ok_or(NotAnExport)?;
                    if ours {
                        return Ok(Some(data));
                    }
                } else if ours {
                    // Not a plain string: not what Earshot writes.
                    return Ok(None);
                } else {
                    skip_line(&mut chars);
                }
            }
            // The key's default value (`@=`) or anything else: not a named value.
            Some(_) => skip_line(&mut chars),
        }
    }
}

/// Skips to the end of the line, and over the lines a trailing `\` continues it on (long binary
/// data is written that way).
fn skip_line(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let mut last = ' ';
    for c in chars.by_ref() {
        if c == '\n' && last != '\\' {
            return;
        }
        if c != '\r' {
            last = c;
        }
    }
}

/// A quoted name or string at the cursor, with its escapes undone; `None` when it never ends.
/// It may hold line breaks: only the closing quote ends it.
fn quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<String> {
    if chars.next() != Some('"') {
        return None;
    }
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            c => out.push(c),
        }
    }
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

/// What the page says when Earshot cannot find its own program file.
pub const NO_PROGRAM_FILE: &str = "Earshot could not find its own program file.";

/// `current_exe`'s failure as fixed copy: the operating system's words never reach the page
/// (T12.56).
fn program_file_error(_: std::io::Error) -> IpcError {
    autostart_error(NO_PROGRAM_FILE)
}

/// How long one `reg.exe` run may take.
const REG_DEADLINE: Duration = Duration::from_secs(10);

impl RunKey {
    /// The `Run` value [`RUN_VALUE`] for this process's exe.
    ///
    /// # Errors
    /// `autostart` when the exe or the Windows folder is unknown.
    pub fn for_this_app() -> Result<Self, IpcError> {
        let exe = std::env::current_exe().map_err(program_file_error)?;
        Self::new(RUN_KEY, RUN_VALUE, &exe)
    }

    /// Value `name` under `key` naming `exe` (tests use their own name).
    ///
    /// # Errors
    /// `autostart` when the Windows folder is unknown.
    pub fn new(key: &str, name: &str, exe: &Path) -> Result<Self, IpcError> {
        Ok(Self {
            reg: system_reg_exe(std::env::var_os("SystemRoot"))?,
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
    reg_at(
        &system_reg_exe(std::env::var_os("SystemRoot"))?,
        args,
        failure,
    )
}

/// The system's `reg.exe` under `windows` (the `SystemRoot` variable); an unknown folder is fixed
/// copy, never the variable's name (T12.70).
fn system_reg_exe(windows: Option<OsString>) -> Result<PathBuf, IpcError> {
    let windows = windows.ok_or_else(|| autostart_error(crate::plain_text::NO_WINDOWS_FOLDER))?;
    Ok(PathBuf::from(windows).join("System32").join("reg.exe"))
}

/// A file removed when this goes, whatever happened to it meanwhile.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A file name of this process's own in the temp folder for one `reg export`.
fn export_file() -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("earshot-run-{}-{n}.reg", std::process::id()))
}

impl Autostart for RunKey {
    fn enabled(&self) -> Result<bool, IpcError> {
        Ok(self
            .value()?
            .is_some_and(|d| d.eq_ignore_ascii_case(&self.command)))
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
        } else if self.exists(CHANGE_FAILED)? {
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
    /// Whether the value exists at all (whatever it names): `reg query` fails when it does not.
    /// Only its exit code is read; its text is in the console's code page.
    fn exists(&self, failure: &'static str) -> Result<bool, IpcError> {
        Ok(self
            .reg(&["query", &self.key, "/v", &self.name], failure)?
            .status
            .success())
    }

    /// The value's data as Windows stores it, `None` when there is no such string value. Read
    /// from `reg export`'s file ([`parse_export`]): the whole key goes to a file of this
    /// process's own in the temp folder for the moment of the read (the other programs' values
    /// under it stay on this computer and the file is removed at once), and only this value is
    /// taken from it.
    fn value(&self) -> Result<Option<String>, IpcError> {
        let unreadable = || autostart_error(READ_FAILED);
        if !self.exists(READ_FAILED)? {
            return Ok(None);
        }
        let file = RemoveOnDrop(export_file());
        let path = file.0.to_str().ok_or_else(unreadable)?;
        let out = self.reg(&["export", &self.key, path, "/y"], READ_FAILED)?;
        if !out.status.success() {
            return Err(unreadable());
        }
        let bytes = std::fs::read(&file.0).map_err(|_| unreadable())?;
        parse_export(&bytes, &self.name).map_err(|_| unreadable())
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
    /// The choice the first-run wizard marks as suggested for this computer (T12.29), from the
    /// whole rule a meeting runs on the next meeting's model and where it runs.
    pub suggested: PreviewsChoice,
    /// Why "Other people only" would show nothing on this computer (fixed copy), or none when it
    /// runs: the wizard disables it with this beside it.
    pub other_people_refused: Option<String>,
    /// Why "Everyone" would not show the user's own voice on this computer (fixed copy), or none
    /// when it does: the wizard disables it with this beside it.
    pub everyone_refused: Option<String>,
}

/// An option refused for the next meeting's transcription model.
pub const PREVIEW_OPTION_MODEL: &str =
    "Not available with the transcription model this computer uses.";
/// "Everyone" refused for want of memory.
pub const PREVIEW_OPTION_MEMORY: &str =
    "This computer does not have enough memory to show them for your own voice as well.";
/// An option refused for any other reason (none today: the options are judged only by the
/// backend, memory and model rules).
pub const PREVIEW_OPTION_NOT_HERE: &str = "Not available on this computer.";

/// What the wizard says beside an option the rules refuse (T12.29): fixed copy by kind, never the
/// runtime's text.
fn preview_option_copy(reason: earshot_runtime::live::PreviewOffReason) -> &'static str {
    use earshot_runtime::live::PreviewOffReason;
    match reason {
        PreviewOffReason::Model => PREVIEW_OPTION_MODEL,
        PreviewOffReason::NotEnoughMemory => PREVIEW_OPTION_MEMORY,
        PreviewOffReason::OnBattery
        | PreviewOffReason::CatchingUp
        | PreviewOffReason::FellBehind
        | PreviewOffReason::TranscriptStopped
        | PreviewOffReason::NeedsCompiledModel
        | PreviewOffReason::CompiledModelRefused
        | PreviewOffReason::OffByDefault
        | PreviewOffReason::TurnedOff
        | PreviewOffReason::NotAsked
        | PreviewOffReason::NotApplicable
        | PreviewOffReason::LoadFailed
        | PreviewOffReason::Failed
        | PreviewOffReason::MeetingEnded => PREVIEW_OPTION_NOT_HERE,
    }
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
    let lead = previews_lead_on(services, models, order, hardware)?;
    let page = earshot_runtime::preview_settings::preview_page(lead.choice, lead.runs_on, hardware);
    let options =
        earshot_runtime::preview_settings::preview_options(lead.runs_on, &lead.model, hardware);
    Ok(PreviewsView {
        choice: page.shown.into(),
        can_change: page.can_change,
        note: page.note.map(str::to_owned),
        unknown: lead.unknown,
        suggested: options.suggested.into(),
        other_people_refused: options
            .other_people
            .map(|r| preview_option_copy(r.reason).to_owned()),
        everyone_refused: options
            .everyone
            .map(|r| preview_option_copy(r.reason).to_owned()),
    })
}

/// What [`previews_view_on`] judges the page on.
#[derive(Debug)]
struct PreviewsLead {
    /// The stored choice.
    choice: earshot_runtime::preview_settings::PreviewChoice,
    /// A stored value this version does not know, read as automatic.
    unknown: Option<String>,
    /// Where the next meeting's transcription model runs.
    runs_on: earshot_runtime::backends::Accelerator,
    /// The next meeting's transcription model.
    model: String,
}

/// The stored choice and where the next meeting's transcription model runs.
fn previews_lead_on(
    services: &earshot_runtime::Services,
    models: &earshot_runtime::backends::ModelDir,
    order: &[earshot_runtime::backends::Accelerator],
    hardware: &earshot_runtime::earshot_hardware::Hardware,
) -> Result<PreviewsLead, IpcError> {
    use earshot_runtime::backends::Accelerator;
    // Only the stored settings are read under the store lock; the model files are looked at
    // with it free.
    let (choice, unknown, reads) = {
        let db = services.store();
        let (choice, unknown) = earshot_runtime::preview_settings::preview_choice(&db)?;
        let reads = earshot_runtime::host::TranscriptionReads::read(&db)?;
        (choice, unknown, reads)
    };
    let setup = earshot_runtime::host::transcription_setup_from(reads, models, order, hardware)?;
    let runs_on =
        earshot_runtime::recommend::runs_on_here(models, &setup.choice.model, &setup.order);
    Ok(PreviewsLead {
        choice,
        unknown,
        runs_on: Accelerator::of(runs_on).unwrap_or(Accelerator::Cpu),
        model: setup.choice.model,
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

/// Whether Earshot offers to start recording when a call starts (T12.86; on unless turned off).
#[tauri::command]
#[specta::specta]
pub async fn call_start_offer(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<bool, IpcError> {
    let services = state.services()?;
    crate::ipc::blocking(move || Ok(earshot_runtime::call_start::offer_on(&services.store())?))
        .await
}

/// Turns the offer to start recording when a call starts on or off, at once; returns it as
/// stored.
#[tauri::command]
#[specta::specta]
pub async fn set_call_start_offer(
    state: tauri::State<'_, crate::ipc::AppState>,
    shell: tauri::State<'_, std::sync::Arc<crate::shell::Shell>>,
    on: bool,
) -> Result<bool, IpcError> {
    let services = state.services()?;
    let shell = std::sync::Arc::clone(shell.inner());
    crate::ipc::blocking(move || {
        // The call-start watch reads the setting from the shell's copy (T14.9).
        store_call_start_offer(&services, on, &|stored| {
            shell.call_start_offer_stored(stored);
        })
    })
    .await
}

/// Stores the offer and returns it as stored. `tell` hears the stored value while the store's
/// lock is still held, so two of these overlapping tell in the order they stored and whoever is
/// told (the call-start watch's copy) ends on what the store holds. `tell` must not take the
/// store.
fn store_call_start_offer(
    services: &earshot_runtime::Services,
    on: bool,
    tell: &dyn Fn(bool),
) -> Result<bool, IpcError> {
    let mut db = services.store();
    earshot_runtime::call_start::set_offer_on(&mut db, on)?;
    let stored = earshot_runtime::call_start::offer_on(&db)?;
    tell(stored);
    drop(db);
    Ok(stored)
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
    fn general_a_missing_program_file_is_fixed_copy_without_the_systems_words() {
        let e = program_file_error(std::io::Error::other("os words: C:\\Users\\someone"));
        assert_eq!(
            (e.kind.as_str(), e.message.as_str()),
            ("autostart", NO_PROGRAM_FILE)
        );
    }

    /// T12.70: an unknown Windows folder is fixed copy, never the variable's name.
    #[test]
    fn general_an_unknown_windows_folder_is_fixed_copy_without_the_variables_name() {
        let e = system_reg_exe(None).unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str()),
            ("autostart", crate::plain_text::NO_WINDOWS_FOLDER)
        );
        assert!(!crate::plain_text::names_developer_tool(&e.message));
        assert!(!e.message.contains("SystemRoot"), "{}", e.message);
        assert_eq!(
            system_reg_exe(Some(OsString::from("C:\\Windows"))).unwrap(),
            PathBuf::from("C:\\Windows")
                .join("System32")
                .join("reg.exe")
        );
    }

    /// A registration file as `reg export` writes one: a byte order mark, then UTF-16.
    fn exported(text: &str) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        bytes
    }

    /// `data` as a registration file writes a string: `\` and `"` escaped.
    fn reg_string(data: &str) -> String {
        format!("\"{}\"", data.replace('\\', r"\\").replace('"', "\\\""))
    }

    /// The `Run` key's export with these value lines.
    fn run_export(values: &[String]) -> Vec<u8> {
        let mut text = format!(
            "{EXPORT_HEADER}\r\n\r\n\
             [HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run]\r\n"
        );
        for value in values {
            text.push_str(value);
            text.push_str("\r\n");
        }
        text.push_str("\r\n");
        exported(&text)
    }

    #[test]
    fn general_run_value_is_this_exe_in_quotes_in_the_tray_and_is_read_back_from_the_export() {
        let exe = Path::new(r"C:\Program Files\Earshot\earshot.exe");
        let cmd = run_command(exe);
        assert_eq!(cmd, r#""C:\Program Files\Earshot\earshot.exe" --autostart"#);
        let ours = format!("\"Earshot\"={}", reg_string(&cmd));
        assert_eq!(
            ours,
            r#""Earshot"="\"C:\\Program Files\\Earshot\\earshot.exe\" --autostart""#
        );
        // Among other programs' values, of every type a `Run` key holds.
        let others = [
            format!("\"Zq Updater\"={}", reg_string(r"C:\zq\updater.exe /quiet")),
            "\"ZqExpand\"=hex(2):25,00,4c,00,4f,00,43,00,41,00,4c,00,\\".to_owned(),
            "  41,00,50,00,50,00,00,00".to_owned(),
            "\"ZqNumber\"=dword:00000001".to_owned(),
            "@=\"a default value\"".to_owned(),
        ];
        let mut values = others.to_vec();
        // After the binary value's second line.
        values.insert(3, ours.clone());
        assert_eq!(
            parse_export(&run_export(&values), "Earshot"),
            Ok(Some(cmd.clone()))
        );
        // Another value whose name starts the same is not ours.
        let old = format!("\"EarshotOld\"={}", reg_string(r"C:\old.exe"));
        assert_eq!(parse_export(&run_export(&[old]), "Earshot"), Ok(None));
        // Windows keeps the case a value was written with, and matches names in any case.
        let lower = format!("\"earshot\"={}", reg_string(&cmd));
        assert_eq!(
            parse_export(&run_export(&[lower]), "Earshot"),
            Ok(Some(cmd.clone()))
        );
        // No such value, with or without others.
        assert_eq!(parse_export(&run_export(&[]), "Earshot"), Ok(None));
        assert_eq!(parse_export(&run_export(&others), "Earshot"), Ok(None));
        // A value of another type under the name is not what Earshot writes (a plain string):
        // it reads as off, and turning the setting on replaces it.
        let expand = "\"Earshot\"=hex(2):25,00,4c,00,00,00".to_owned();
        assert_eq!(parse_export(&run_export(&[expand]), "Earshot"), Ok(None));
    }

    #[test]
    fn general_run_value_is_read_whole_whatever_letters_the_path_holds() {
        // T14.9: `reg query` printed the path in the console's code page, so a profile folder
        // with a letter outside it came back changed and the setting read as off.
        for folder in [
            r"C:\Users\Zqë Ærø\AppData\Local\Earshot",
            r"C:\Users\Zqł Ęż\AppData\Local\Earshot",
            r"C:\Users\Ωzq 試験\AppData\Local\Earshot",
            r"C:\Users\zq 🎧\AppData\Local\Earshot",
        ] {
            let cmd = run_command(&Path::new(folder).join("earshot.exe"));
            assert!(cmd.contains(folder), "{cmd}");
            let export = run_export(&[format!("\"Earshot\"={}", reg_string(&cmd))]);
            assert_eq!(
                parse_export(&export, "Earshot"),
                Ok(Some(cmd.clone())),
                "{folder}"
            );
        }
    }

    #[test]
    fn general_run_value_is_read_beside_a_value_holding_half_a_surrogate_pair() {
        // T14.9 review: the file holds the whole key, and Windows lets a value hold a lone
        // surrogate. Another program's such value made the strict decode refuse the file, and
        // "Start with Windows" could not be read at all.
        let cmd = run_command(Path::new(r"C:\Earshot\earshot.exe"));
        let ours = format!("\"Earshot\"={}", reg_string(&cmd));
        let head = format!("{EXPORT_HEADER}\r\n\r\n[HKEY_CURRENT_USER\\Zq\\Run]\r\n");
        let broken = ("\"ZqBroken\"=\"C:\\\\zq", "x.exe\"\r\n");
        for ours_first in [false, true] {
            let mut units: Vec<u16> = head.encode_utf16().collect();
            if ours_first {
                units.extend(format!("{ours}\r\n").encode_utf16());
            }
            units.extend(broken.0.encode_utf16());
            // A lead surrogate with nothing after it.
            units.push(0xD800);
            units.extend(broken.1.encode_utf16());
            if !ours_first {
                units.extend(format!("{ours}\r\n").encode_utf16());
            }
            assert!(String::from_utf16(&units).is_err(), "the file holds one");
            let mut bytes = vec![0xFF, 0xFE];
            bytes.extend(units.iter().flat_map(|u| u.to_le_bytes()));
            assert_eq!(parse_export(&bytes, "Earshot"), Ok(Some(cmd.clone())));
            // The other program's value reads with a replacement character where the half
            // pair was: never this exe's command.
            assert_eq!(
                parse_export(&bytes, "ZqBroken"),
                Ok(Some("C:\\zq\u{FFFD}x.exe".to_owned()))
            );
        }
    }

    #[test]
    fn general_call_start_offer_is_told_while_the_store_is_still_held() {
        // T14.9 review: the watch's copy was updated after the store's lock was released, so two
        // overlapping changes could leave it opposite to what is stored. It is told under the
        // lock: here another reader of the store cannot get in until the telling is over.
        use std::sync::mpsc;
        let guard = std::time::Duration::from_secs(60);
        let dir = crate::testutil::TempDir::new("offer-told");
        let services = crate::testutil::services(&dir);
        let told = Mutex::new(Vec::new());
        let (started_tx, started) = mpsc::channel();
        let (read_tx, read) = mpsc::channel();
        let stored = store_call_start_offer(&services, false, &|on| {
            told.lock().unwrap().push(on);
            let (svc, started_tx, read_tx) =
                (services.clone(), started_tx.clone(), read_tx.clone());
            std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                let on = earshot_runtime::call_start::offer_on(&svc.store()).unwrap();
                read_tx.send(on).unwrap();
            });
            started.recv_timeout(guard).expect("the other reader runs");
            assert_eq!(
                read.recv_timeout(std::time::Duration::from_millis(200)),
                Err(mpsc::RecvTimeoutError::Timeout),
                "the store was free while the listener was told"
            );
        })
        .unwrap();
        assert!(!stored);
        assert_eq!(*told.lock().unwrap(), [false]);
        // The command is over: the other reader gets the store and reads what was stored.
        assert_eq!(read.recv_timeout(guard), Ok(false));

        // Through the watch's own copy: it ends on what the store holds, change after change.
        let copy = crate::shell::OfferSetting::default();
        for on in [true, false, true] {
            let stored = store_call_start_offer(&services, on, &|stored| copy.set(stored)).unwrap();
            assert_eq!(stored, on);
            assert_eq!(copy.get(|| None), on);
        }
    }

    #[test]
    fn general_run_export_only_the_keys_own_string_value_counts() {
        let cmd = run_command(Path::new(r"C:\Earshot\earshot.exe"));
        let ours = format!("\"Earshot\"={}", reg_string(&cmd));
        // A subkey's value of the same name is not the key's.
        let subkey = exported(&format!(
            "{EXPORT_HEADER}\r\n\r\n[HKEY_CURRENT_USER\\Zq\\Run]\r\n\"ZqOther\"=\"x\"\r\n\r\n\
             [HKEY_CURRENT_USER\\Zq\\Run\\Sub]\r\n{ours}\r\n\r\n"
        ));
        assert_eq!(parse_export(&subkey, "Earshot"), Ok(None));
        // Another program's string that holds a line break and then text shaped like our value
        // is still that program's string.
        let spoof = format!(
            "\"ZqOther\"=\"first line\r\n{}\"",
            ours.replace('\\', r"\\").replace('"', "\\\"")
        );
        assert_eq!(parse_export(&run_export(&[spoof]), "Earshot"), Ok(None));
        // Anything that is not such a file is refused, never guessed at.
        let text = format!("{EXPORT_HEADER}\r\n\r\n[HKEY_CURRENT_USER\\Zq\\Run]\r\n{ours}\r\n");
        let utf16 = exported(&text);
        assert_eq!(parse_export(&utf16, "Earshot"), Ok(Some(cmd)));
        for bad in [
            Vec::new(),
            vec![0xFF, 0xFE],
            text.as_bytes().to_vec(),
            utf16[2..].to_vec(),
            utf16[..utf16.len() - 1].to_vec(),
            exported(&text.replace("Version 5.00", "Version 4.00")),
            exported(&format!(
                "{EXPORT_HEADER}\r\n\r\n[HKEY\\Zq]\r\n\"Earshot\"=\"cut off"
            )),
            exported(&format!(
                "{EXPORT_HEADER}\r\n\r\n[HKEY\\Zq]\r\n\"Earshot\" \"x\"\r\n"
            )),
        ] {
            assert_eq!(parse_export(&bad, "Earshot"), Err(NotAnExport), "{bad:?}");
        }
    }

    #[test]
    fn general_run_export_files_are_this_processes_own_and_removed() {
        let (a, b) = (export_file(), export_file());
        assert_ne!(a, b);
        for file in [&a, &b] {
            assert!(file.starts_with(std::env::temp_dir()));
            let name = file.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with(&format!("earshot-run-{}-", std::process::id())));
            assert!(name.ends_with(".reg"));
        }
        std::fs::write(&a, b"x").unwrap();
        drop(RemoveOnDrop(a.clone()));
        assert!(!a.exists());
        // A file that was never made is no error.
        drop(RemoveOnDrop(b));
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
        use earshot_runtime::preview_settings::PreviewChoice;
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
        // T12.75 made the processor's and the NPU's pages read alike on this machine, so where
        // the page was judged shows in the lead itself.
        // No NPU version: the model runs on the processor, and the page is judged there.
        let lead = previews_lead_on(&services, &models, &order, &hw).unwrap();
        assert_eq!(lead.runs_on, Accelerator::Cpu);
        let cpu = previews_view_on(&services, &models, &order, &hw).unwrap();
        assert_eq!(cpu.note, None, "{cpu:?}");
        assert!(cpu.can_change);
        // With its NPU version the model runs on the NPU, and the page is judged there.
        let Ok(AsrModel::Parakeet(p)) = asr_model(model) else {
            panic!("a Parakeet model");
        };
        for rel in p.qnn_files() {
            let f = models.root().join(rel);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"x").unwrap();
        }
        let lead = previews_lead_on(&services, &models, &order, &hw).unwrap();
        assert_eq!(lead.choice, PreviewChoice::Everyone);
        assert_eq!(lead.unknown, None);
        assert_eq!(lead.runs_on, Accelerator::Qnn);
        let npu = previews_view_on(&services, &models, &order, &hw).unwrap();
        assert!(npu.can_change);
        // T12.75: Everyone on the NPU with this much memory shows the user's own voice too, and
        // the page says nothing about it.
        assert_eq!(npu.note, None, "{npu:?}");
    }

    /// T12.29: the view carries the suggestion and the refused options for this computer, judged
    /// on the next meeting's model where it runs, whatever is stored: Everyone with room for both
    /// sessions, Off with too little memory to run them by default (the user's own voice refused
    /// for memory), and Off with both options refused for a whisper model.
    #[test]
    fn previews_view_suggests_from_the_model_and_where_it_runs() {
        use earshot_runtime::backends::{Accelerator, ModelDir};
        use earshot_runtime::earshot_hardware::{Cpu, Hardware};
        const GIB: u64 = 1 << 30;
        let dir = crate::testutil::TempDir::new("previews-suggested");
        let services = crate::testutil::services(&dir);
        let models = ModelDir::new(dir.0.join("models"));
        let order = [Accelerator::Cpu];
        let machine = |memory| Hardware {
            cpu: Cpu {
                name: None,
                physical_cores: Some(8),
                logical_cores: Some(16),
            },
            gpus: Vec::new(),
            npus: Vec::new(),
            total_memory: memory,
        };
        let model_memory = PREVIEW_OPTION_MEMORY.to_owned();
        let model_refused = PREVIEW_OPTION_MODEL.to_owned();
        for (model, memory, suggested, other_people, everyone) in [
            (
                "parakeet-tdt-0.6b-v2",
                Some(32 * GIB),
                PreviewsChoice::Everyone,
                None,
                None,
            ),
            (
                "parakeet-tdt-0.6b-v2",
                Some(8 * GIB),
                PreviewsChoice::Off,
                None,
                Some(&model_memory),
            ),
            (
                "parakeet-tdt-0.6b-v2",
                None,
                PreviewsChoice::Off,
                None,
                Some(&model_memory),
            ),
            (
                "small.en",
                Some(32 * GIB),
                PreviewsChoice::Off,
                Some(&model_refused),
                Some(&model_refused),
            ),
        ] {
            let before = crate::models::settings_view(&services).unwrap().value;
            crate::models::set_settings(
                &services,
                &crate::models::ModelSettingsValue {
                    transcription: Some(model.into()),
                    ..before
                },
            )
            .unwrap();
            let hw = machine(memory);
            for stored in [PreviewsChoice::Off, PreviewsChoice::OtherPeople] {
                store_previews(&services, stored).unwrap();
                let view = previews_view_on(&services, &models, &order, &hw).unwrap();
                let case = format!("{model} {memory:?} {stored:?}");
                assert_eq!(view.suggested, suggested, "{case}");
                assert_eq!(view.other_people_refused.as_ref(), other_people, "{case}");
                assert_eq!(view.everyone_refused.as_ref(), everyone, "{case}");
                assert_eq!(view.choice, stored, "{case}");
                assert_eq!(
                    previews_lead_on(&services, &models, &order, &hw)
                        .unwrap()
                        .model,
                    model
                );
            }
        }
    }
}
