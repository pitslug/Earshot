//! The Audio page of Settings (T7.4i): the microphone and output a meeting records (the
//! Windows default or one endpoint, by id) and the capture mode a new meeting starts with.
//! The settings and their rules are `earshot_runtime::audio_settings`; the endpoints are
//! listed by `earshot_capture_wasapi` (WASAPI, on a thread of its own), and whether a chosen
//! one is connected is `earshot_capture_wasapi::DeviceList`'s rule. This module only maps
//! them to the IPC types. The meeting start reads the same settings
//! (`earshot_runtime::host::audio_setup`, in `meeting.rs`).

use earshot_runtime::Services;
use earshot_runtime::audio_settings::{
    AudioSettings, AudioSettingsError, DefaultCaptureMode, DeviceSetting,
};
use serde::{Deserialize, Serialize};
use specta::Type;

use crate::ipc::IpcError;
use crate::meeting_events::CaptureMode;

/// A chosen endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ChosenDevice {
    /// Its id (`IMMDevice::GetId`).
    pub id: String,
    /// Its name when it was chosen.
    pub name: String,
}

/// The Audio page settings as the UI sends and reads them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct AudioSettingsValue {
    /// The microphone; `null` for the Windows default.
    pub microphone: Option<ChosenDevice>,
    /// The output; `null` for the Windows default.
    pub output: Option<ChosenDevice>,
    /// What a new meeting records unless the user picks the other mode.
    pub capture_mode: CaptureMode,
}

fn device_value(d: &DeviceSetting) -> Option<ChosenDevice> {
    match d {
        DeviceSetting::Default => None,
        DeviceSetting::Chosen { id, name } => Some(ChosenDevice {
            id: id.clone(),
            name: name.clone(),
        }),
    }
}

fn device_setting(d: Option<&ChosenDevice>) -> DeviceSetting {
    d.map_or(DeviceSetting::Default, |c| DeviceSetting::Chosen {
        id: c.id.clone(),
        name: c.name.clone(),
    })
}

impl AudioSettingsValue {
    fn of(s: &AudioSettings) -> Self {
        Self {
            microphone: device_value(&s.microphone),
            output: device_value(&s.output),
            capture_mode: match s.capture_mode {
                DefaultCaptureMode::Call => CaptureMode::Call,
                DefaultCaptureMode::InRoom => CaptureMode::InRoom,
            },
        }
    }

    fn settings(&self) -> AudioSettings {
        AudioSettings {
            microphone: device_setting(self.microphone.as_ref()),
            output: device_setting(self.output.as_ref()),
            capture_mode: match self.capture_mode {
                CaptureMode::Call => DefaultCaptureMode::Call,
                CaptureMode::InRoom => DefaultCaptureMode::InRoom,
            },
        }
    }
}

/// One listed endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct DeviceItem {
    /// Its id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Windows uses it by default.
    pub is_default: bool,
    /// A microphone that is a Bluetooth headset in hands-free mode (phone quality; T6.24).
    pub hands_free: bool,
}

/// The endpoints Windows lists now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Devices {
    /// Capture endpoints.
    pub microphones: Vec<DeviceItem>,
    /// Render endpoints.
    pub outputs: Vec<DeviceItem>,
}

impl Devices {
    /// The capture layer's list as the page shows it.
    #[cfg(windows)]
    #[must_use]
    pub fn of(list: &earshot_capture_wasapi::DeviceList) -> Self {
        let item = |d: &earshot_capture_wasapi::AudioDevice, mic: bool| DeviceItem {
            id: d.id.clone(),
            name: d.name.clone(),
            is_default: d.is_default,
            hands_free: mic && d.mic_kind() == earshot_capture_wasapi::MicKind::BluetoothHandsFree,
        };
        Self {
            microphones: list.microphones.iter().map(|d| item(d, true)).collect(),
            outputs: list.outputs.iter().map(|d| item(d, false)).collect(),
        }
    }

    /// Whether `choice` is usable now (`earshot_runtime::audio_settings::device_available`,
    /// the one rule, on the page's items).
    fn has(devices: &[DeviceItem], choice: Option<&ChosenDevice>) -> bool {
        earshot_runtime::audio_settings::device_available(
            devices.iter().map(|d| (d.id.as_str(), d.is_default)),
            choice.map(|c| c.id.as_str()),
        )
    }
}

/// The Audio page: the stored settings, the endpoints listed now and what the page says
/// about them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Type)]
pub struct AudioPageView {
    /// The settings in use (defaults for anything absent or unknown).
    pub value: AudioSettingsValue,
    /// One line per stored value this version cannot use.
    pub unknown: Vec<String>,
    /// The microphones Windows lists now (empty when the list could not be read).
    pub microphones: Vec<DeviceItem>,
    /// The outputs Windows lists now.
    pub outputs: Vec<DeviceItem>,
    /// Why the endpoints could not be listed (`null` when they were).
    pub devices_error: Option<String>,
    /// The chosen microphone (or, with none chosen, any default one) is not listed now, so
    /// a meeting would record the default (or fail without one). False when the list could
    /// not be read.
    pub microphone_missing: bool,
    /// The same for the output.
    pub output_missing: bool,
}

/// The page for stored `settings` (with `unknown` lines) and the endpoints `devices` (or why
/// they could not be listed).
#[must_use]
pub fn page_view(
    settings: &AudioSettings,
    unknown: Vec<String>,
    devices: Result<Devices, String>,
) -> AudioPageView {
    let value = AudioSettingsValue::of(settings);
    let (devices, devices_error) = match devices {
        Ok(d) => (Some(d), None),
        Err(e) => (None, Some(e)),
    };
    let missing = |pick: fn(&Devices) -> &[DeviceItem], choice: Option<&ChosenDevice>| {
        devices
            .as_ref()
            .is_some_and(|d| !Devices::has(pick(d), choice))
    };
    AudioPageView {
        microphone_missing: missing(|d| &d.microphones, value.microphone.as_ref()),
        output_missing: missing(|d| &d.outputs, value.output.as_ref()),
        microphones: devices
            .as_ref()
            .map(|d| d.microphones.clone())
            .unwrap_or_default(),
        outputs: devices.map(|d| d.outputs).unwrap_or_default(),
        value,
        unknown,
        devices_error,
    }
}

/// `text` with its first letter upper-case and a full stop.
fn sentence(text: &str) -> String {
    let mut c = text.chars();
    let mut out: String = c
        .next()
        .map(|f| f.to_uppercase().collect())
        .unwrap_or_default();
    out.push_str(c.as_str());
    if !out.ends_with('.') {
        out.push('.');
    }
    out
}

/// The endpoints Windows lists now, or why they could not be listed.
///
/// # Errors
/// The reason, as page text.
pub fn list_devices() -> Result<Devices, String> {
    #[cfg(windows)]
    {
        earshot_capture_wasapi::list_audio_devices()
            .map(|l| Devices::of(&l))
            .map_err(|e| sentence(&format!("Windows could not list the audio devices: {e}")))
    }
    #[cfg(not(windows))]
    {
        Err("Audio devices are listed only on Windows.".to_owned())
    }
}

/// The page from `services`' store with the endpoints `devices`.
///
/// # Errors
/// Store errors.
pub fn audio_page(
    services: &Services,
    devices: Result<Devices, String>,
) -> Result<AudioPageView, IpcError> {
    let (s, unknown) = earshot_runtime::audio_settings::audio_settings(&services.store())?;
    Ok(page_view(
        &s,
        unknown.iter().map(ToString::to_string).collect(),
        devices,
    ))
}

/// The UI error of a refused setting.
#[must_use]
pub fn settings_error(e: AudioSettingsError) -> IpcError {
    match e {
        AudioSettingsError::Store(e) => IpcError::from(e),
        other => IpcError {
            kind: "invalid_setting".into(),
            message: sentence(&other.to_string()),
            retryable: false,
        },
    }
}

/// Checks and stores `value`; returns the page as stored, with the endpoints `devices`.
///
/// # Errors
/// `invalid_setting` (nothing stored); store errors.
pub fn set_settings(
    services: &Services,
    value: &AudioSettingsValue,
    devices: Result<Devices, String>,
) -> Result<AudioPageView, IpcError> {
    let stored = {
        let mut db = services.store();
        earshot_runtime::audio_settings::set_audio_settings(&mut db, &value.settings())
            .map_err(settings_error)?
    };
    Ok(page_view(&stored, Vec::new(), devices))
}

/// The Audio page: the stored settings and the endpoints Windows lists now (listed again on
/// every call, so the page can refresh after a device is plugged in).
#[tauri::command]
#[specta::specta]
pub async fn audio_settings(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<AudioPageView, IpcError> {
    let services = state.services()?;
    blocking(move || audio_page(&services, list_devices())).await
}

/// Checks and stores the Audio page settings (`invalid_setting`: nothing stored); returns
/// the page with the endpoints listed again.
#[tauri::command]
#[specta::specta]
pub async fn set_audio_settings(
    state: tauri::State<'_, crate::ipc::AppState>,
    settings: AudioSettingsValue,
) -> Result<AudioPageView, IpcError> {
    let services = state.services()?;
    blocking(move || set_settings(&services, &settings, list_devices())).await
}

/// The stored capture mode a new meeting starts with: a store read only, no device listing
/// (the new-meeting screen asks for it every time it opens).
#[tauri::command]
#[specta::specta]
pub async fn default_capture_mode(
    state: tauri::State<'_, crate::ipc::AppState>,
) -> Result<CaptureMode, IpcError> {
    let services = state.services()?;
    blocking(move || stored_capture_mode(&services)).await
}

/// The stored capture mode (the default for a value this version cannot use).
///
/// # Errors
/// Store errors.
pub fn stored_capture_mode(services: &Services) -> Result<CaptureMode, IpcError> {
    let (s, _) = earshot_runtime::audio_settings::audio_settings(&services.store())?;
    Ok(AudioSettingsValue::of(&s).capture_mode)
}

/// Runs blocking work off the main thread.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, IpcError> + Send + 'static,
) -> Result<T, IpcError> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|_| IpcError {
            kind: "internal".into(),
            message: "the task stopped".into(),
            retryable: false,
        })?
}

#[cfg(test)]
#[path = "audio_settings_tests.rs"]
mod tests;
