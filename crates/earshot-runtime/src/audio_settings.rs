//! The Audio page settings (T7.4i): which microphone and output a meeting records, and the
//! capture mode a new meeting starts with. Device-local (`local_state`, never synced, no
//! migration), one key each, written only through [`set_audio_settings`].
//!
//! - **Microphone** and **output** ([`DeviceSetting`]): the Windows default (followed when
//!   it changes) or one endpoint by its id (`IMMDevice::GetId`), kept with the name it had
//!   when chosen so the page and the meeting's warning can name it while it is unplugged.
//!   The id decides; a chosen endpoint that is gone when a meeting starts is replaced by the
//!   default with a warning (`earshot_capture_wasapi::devices`, through
//!   [`crate::host::wasapi_starter_on`]).
//! - **Capture mode** ([`DefaultCaptureMode`]): what a new meeting records unless the user
//!   picks the other one (a call: mic and computer audio; in the room: mic only).
//!
//! Each value is stored as text: `default` or the endpoint id, the name, `call` or
//! `in-room`. A value this build cannot use reads as the default and is reported
//! ([`UnknownSetting`]), as the Models page settings do. Setting a value checks it first
//! ([`validate`]).

use std::fmt;

use earshot_store::Store;

use crate::RuntimeError;
use crate::local::DeviceRecord;
pub use crate::model_settings::UnknownSetting;

#[cfg(test)]
#[path = "audio_settings_tests.rs"]
mod tests;

/// `local_state` key of the chosen microphone (`default` or an endpoint id).
pub const MICROPHONE_KEY: &str = "settings/microphone";
/// `local_state` key of the chosen microphone's name when it was chosen.
pub const MICROPHONE_NAME_KEY: &str = "settings/microphone-name";
/// `local_state` key of the chosen output (`default` or an endpoint id).
pub const OUTPUT_KEY: &str = "settings/output";
/// `local_state` key of the chosen output's name when it was chosen.
pub const OUTPUT_NAME_KEY: &str = "settings/output-name";
/// `local_state` key of the capture mode a new meeting starts with.
pub const CAPTURE_MODE_KEY: &str = "settings/capture-mode";

/// The longest endpoint id kept (Windows' are about 55 characters).
pub const MAX_DEVICE_ID: usize = 512;
/// The longest endpoint name kept.
pub const MAX_DEVICE_NAME: usize = 256;

/// The stored text of [`DeviceSetting::Default`].
const DEFAULT: &str = "default";

/// Which endpoint a meeting records.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeviceSetting {
    /// The Windows default, followed when it changes.
    #[default]
    Default,
    /// One endpoint.
    Chosen {
        /// Its id (`IMMDevice::GetId`).
        id: String,
        /// Its name when it was chosen (may be empty).
        name: String,
    },
}

impl DeviceSetting {
    /// The chosen id, `None` for the default.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Default => None,
            Self::Chosen { id, .. } => Some(id),
        }
    }

    /// The capture layer's choice.
    #[cfg(windows)]
    #[must_use]
    pub fn choice(&self) -> earshot_capture_wasapi::DeviceChoice {
        match self {
            Self::Default => earshot_capture_wasapi::DeviceChoice::Default,
            Self::Chosen { id, name } => earshot_capture_wasapi::DeviceChoice::Endpoint {
                id: id.clone(),
                name: name.clone(),
            },
        }
    }
}

/// What a new meeting records unless the user picks the other mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefaultCaptureMode {
    /// A call: the mic and the computer's audio.
    #[default]
    Call,
    /// In the room: the mic only.
    InRoom,
}

impl DefaultCaptureMode {
    /// The stored name: `call`, `in-room`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::InRoom => "in-room",
        }
    }

    /// Reads [`DefaultCaptureMode::as_str`].
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        [Self::Call, Self::InRoom]
            .into_iter()
            .find(|m| m.as_str() == text)
    }
}

/// Every Audio page setting.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioSettings {
    /// The microphone.
    pub microphone: DeviceSetting,
    /// The output.
    pub output: DeviceSetting,
    /// The capture mode a new meeting starts with.
    pub capture_mode: DefaultCaptureMode,
}

impl AudioSettings {
    /// The capture layer's device selection.
    #[cfg(windows)]
    #[must_use]
    pub fn devices(&self) -> earshot_capture_wasapi::DeviceSelection {
        earshot_capture_wasapi::DeviceSelection {
            microphone: self.microphone.choice(),
            output: self.output.choice(),
        }
    }
}

/// Whether a choice can be recorded now from the endpoints `listed` (endpoint id, whether
/// it is the Windows default): the default (`chosen` `None`) when Windows has one, a chosen
/// endpoint when it is listed by exactly that id (its name does not decide). The one rule;
/// the Audio page uses it and the UI mock is pinned to it by
/// `fixtures/audio/device_available_cases.json`, which both test suites run.
pub fn device_available<'a>(
    listed: impl IntoIterator<Item = (&'a str, bool)>,
    chosen: Option<&str>,
) -> bool {
    let mut listed = listed.into_iter();
    match chosen {
        None => listed.any(|(_, is_default)| is_default),
        Some(id) => listed.any(|(listed_id, _)| listed_id == id),
    }
}

/// Why a setting was refused (nothing was stored).
#[derive(Debug)]
pub enum AudioSettingsError {
    /// A chosen endpoint id that is empty, too long, holds a control character or is the
    /// word that stands for the default.
    BadDeviceId,
    /// A device name that is too long or holds a control character.
    BadDeviceName,
    /// Reading or writing the store failed.
    Store(RuntimeError),
}

impl fmt::Display for AudioSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadDeviceId => f.write_str("this device cannot be chosen: its id is not usable"),
            Self::BadDeviceName => write!(
                f,
                "this device cannot be chosen: its name is longer than {MAX_DEVICE_NAME} \
                 characters or holds a line break"
            ),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AudioSettingsError {}

impl From<RuntimeError> for AudioSettingsError {
    fn from(e: RuntimeError) -> Self {
        Self::Store(e)
    }
}

fn usable_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_DEVICE_ID
        && id != DEFAULT
        && !id.chars().any(char::is_control)
}

fn usable_name(name: &str) -> bool {
    name.chars().count() <= MAX_DEVICE_NAME && !name.chars().any(char::is_control)
}

fn checked(device: &DeviceSetting) -> Result<DeviceSetting, AudioSettingsError> {
    match device {
        DeviceSetting::Default => Ok(DeviceSetting::Default),
        DeviceSetting::Chosen { id, name } => {
            let (id, name) = (id.trim(), name.trim());
            if !usable_id(id) {
                return Err(AudioSettingsError::BadDeviceId);
            }
            if !usable_name(name) {
                return Err(AudioSettingsError::BadDeviceName);
            }
            Ok(DeviceSetting::Chosen {
                id: id.to_owned(),
                name: name.to_owned(),
            })
        }
    }
}

/// The settings `settings` would be stored as: ids and names trimmed, everything checked.
///
/// # Errors
/// [`AudioSettingsError`] (never [`AudioSettingsError::Store`]).
pub fn validate(settings: &AudioSettings) -> Result<AudioSettings, AudioSettingsError> {
    Ok(AudioSettings {
        microphone: checked(&settings.microphone)?,
        output: checked(&settings.output)?,
        capture_mode: settings.capture_mode,
    })
}

/// One stored device (`key` the id, `name_key` its name): the default when absent; an id
/// this build cannot use is reported and read as the default.
fn stored_device(
    store: &Store,
    key: &'static str,
    name_key: &'static str,
    unknown: &mut Vec<UnknownSetting>,
) -> Result<DeviceSetting, RuntimeError> {
    let Some(bytes) = store.local_value(key)? else {
        return Ok(DeviceSetting::Default);
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if text == DEFAULT {
        return Ok(DeviceSetting::Default);
    }
    if !usable_id(&text) {
        unknown.push(UnknownSetting { key, stored: text });
        return Ok(DeviceSetting::Default);
    }
    let name = store
        .local_value(name_key)?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .filter(|n| usable_name(n))
        .unwrap_or_default();
    Ok(DeviceSetting::Chosen { id: text, name })
}

/// The stored settings (absent values are the defaults) and every stored value this build
/// cannot use (its default is used).
///
/// # Errors
/// Store errors.
pub fn audio_settings(store: &Store) -> Result<(AudioSettings, Vec<UnknownSetting>), RuntimeError> {
    let mut unknown = Vec::new();
    let microphone = stored_device(store, MICROPHONE_KEY, MICROPHONE_NAME_KEY, &mut unknown)?;
    let output = stored_device(store, OUTPUT_KEY, OUTPUT_NAME_KEY, &mut unknown)?;
    let capture_mode = match store.local_value(CAPTURE_MODE_KEY)? {
        None => DefaultCaptureMode::default(),
        Some(bytes) => {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            DefaultCaptureMode::parse(&text).unwrap_or_else(|| {
                unknown.push(UnknownSetting {
                    key: CAPTURE_MODE_KEY,
                    stored: text,
                });
                DefaultCaptureMode::default()
            })
        }
    };
    Ok((
        AudioSettings {
            microphone,
            output,
            capture_mode,
        },
        unknown,
    ))
}

/// Checks and stores every setting in one transaction (replacing earlier ones); returns
/// what was stored.
///
/// # Errors
/// [`AudioSettingsError`]: nothing is stored on a refusal.
pub fn set_audio_settings(
    store: &mut Store,
    settings: &AudioSettings,
) -> Result<AudioSettings, AudioSettingsError> {
    let s = validate(settings)?;
    let device = |d: &DeviceSetting| match d {
        DeviceSetting::Default => (DEFAULT.to_owned(), String::new()),
        DeviceSetting::Chosen { id, name } => (id.clone(), name.clone()),
    };
    let (mic, mic_name) = device(&s.microphone);
    let (output, output_name) = device(&s.output);
    let values = [
        (DeviceRecord::Microphone, mic.as_str()),
        (DeviceRecord::MicrophoneName, mic_name.as_str()),
        (DeviceRecord::Output, output.as_str()),
        (DeviceRecord::OutputName, output_name.as_str()),
        (DeviceRecord::CaptureMode, s.capture_mode.as_str()),
    ];
    store
        .transaction(|tx| -> Result<(), earshot_store::StoreError> {
            for (record, value) in values {
                crate::local::set_tx(tx, &record.key(), value.as_bytes())?;
            }
            Ok(())
        })
        .map_err(RuntimeError::from)?;
    Ok(s)
}
