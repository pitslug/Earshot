//! Audio device choice (T7.4i): the capture and render endpoints Windows lists, which of
//! them the user chose for the microphone and the output, and what happens when a chosen
//! one is gone. Platform independent; `wasapi::list_devices` fills a [`DeviceList`] and the
//! capture loop opens what a [`DeviceSelection`] names.
//!
//! The rules:
//! - A choice is [`DeviceChoice::Default`] (the endpoint Windows uses by default for the
//!   console role, followed when it changes, as before T7.4i) or one endpoint by its id
//!   (`IMMDevice::GetId`), which stays chosen when the Windows default changes.
//! - At the start of a capture, a chosen endpoint that cannot be opened (not connected,
//!   disabled, or failing to open) is replaced by the default endpoint, and the replacement is
//!   reported as a [`DeviceFallback`] so the meeting can say so ([`fallback`]).
//! - During a capture, a chosen endpoint that is lost is reported unavailable (silence is
//!   recorded) and retried every second; when it still cannot be opened after
//!   [`crate::follow::CHOSEN_DEVICE_GRACE_QPC`], the track moves to the default endpoint,
//!   names the given-up endpoint in that re-open (`EndpointWarning::ChosenEndpointGone`) and
//!   follows the default for the rest of the capture (the chosen endpoint is not taken back
//!   when it returns; the next meeting starts on it again).
//! - The output choice names the render endpoint whose loopback is recorded when a call is
//!   captured from a device (no call app found, or a device loopback requested); process
//!   loopback of a call app does not depend on it.

use std::fmt;

use earshot_core::capture::TrackId;

use crate::endpoint::{EndpointProperties, MicKind, classify_mic};

/// Which endpoint a track records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeviceChoice {
    /// The Windows default endpoint (followed when it changes).
    #[default]
    Default,
    /// One endpoint, by id.
    Endpoint {
        /// `IMMDevice::GetId`.
        id: String,
        /// The endpoint's name when it was chosen (for messages; the id decides).
        name: String,
    },
}

impl DeviceChoice {
    /// The chosen endpoint id; `None` for the default.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Default => None,
            Self::Endpoint { id, .. } => Some(id),
        }
    }
}

/// The microphone and output a capture uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceSelection {
    /// The capture endpoint of the mic track.
    pub microphone: DeviceChoice,
    /// The render endpoint whose loopback is the reference track when a call is captured
    /// from a device.
    pub output: DeviceChoice,
}

/// One endpoint as listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    /// `IMMDevice::GetId`.
    pub id: String,
    /// Friendly name.
    pub name: String,
    /// Windows uses it by default (console role).
    pub is_default: bool,
    /// What the property store says (the mix format's rate when it could be read).
    pub properties: EndpointProperties,
}

impl AudioDevice {
    /// For a capture endpoint: what kind of mic it is ([`classify_mic`]).
    #[must_use]
    pub fn mic_kind(&self) -> MicKind {
        classify_mic(&self.properties)
    }
}

/// The active endpoints, in Windows' order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceList {
    /// Capture endpoints.
    pub microphones: Vec<AudioDevice>,
    /// Render endpoints.
    pub outputs: Vec<AudioDevice>,
}

impl DeviceList {
    /// The default microphone, if one is listed.
    #[must_use]
    pub fn default_microphone(&self) -> Option<&AudioDevice> {
        self.microphones.iter().find(|d| d.is_default)
    }

    /// The default output, if one is listed.
    #[must_use]
    pub fn default_output(&self) -> Option<&AudioDevice> {
        self.outputs.iter().find(|d| d.is_default)
    }
}

// Whether a listed choice is connected is `earshot_runtime::audio_settings::device_available`
// (one rule, pinned by `fixtures/audio/device_available_cases.json`).

/// Why a chosen endpoint was not used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FallbackCause {
    /// Windows does not list it as active (unplugged, switched off, disabled).
    NotConnected,
    /// It is listed but could not be opened.
    OpenFailed(String),
}

impl fmt::Display for FallbackCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => f.write_str("it is not connected"),
            Self::OpenFailed(e) => write!(f, "it could not be opened ({e})"),
        }
    }
}

/// A chosen endpoint replaced by the default at the start of a capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceFallback {
    /// The track: [`TrackId::Mic`] for the microphone, [`TrackId::Reference`] for the output.
    pub track: TrackId,
    /// The chosen endpoint's id.
    pub chosen_id: String,
    /// The chosen endpoint's name when it was chosen.
    pub chosen_name: String,
    /// Why it was not used.
    pub cause: FallbackCause,
    /// The default endpoint recorded instead.
    pub used: String,
}

/// The fallback to report when opening `choice` for `track` failed with `error` and the
/// default endpoint `used` was opened instead; `gone` says the endpoint is not active
/// (from the open attempt). `None` for the default choice (its failure is the capture's).
#[must_use]
pub fn fallback(
    track: TrackId,
    choice: &DeviceChoice,
    gone: bool,
    error: &str,
    used: &str,
) -> Option<DeviceFallback> {
    let DeviceChoice::Endpoint { id, name } = choice else {
        return None;
    };
    Some(DeviceFallback {
        track,
        chosen_id: id.clone(),
        chosen_name: name.clone(),
        cause: if gone {
            FallbackCause::NotConnected
        } else {
            FallbackCause::OpenFailed(error.to_owned())
        },
        used: used.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, default: bool) -> AudioDevice {
        AudioDevice {
            id: id.into(),
            name: format!("{id} name"),
            is_default: default,
            properties: EndpointProperties {
                name: format!("{id} name"),
                id: Some(id.into()),
                ..EndpointProperties::default()
            },
        }
    }

    fn chosen(id: &str) -> DeviceChoice {
        DeviceChoice::Endpoint {
            id: id.into(),
            name: "Headset".into(),
        }
    }

    #[test]
    fn devices_list_names_its_defaults() {
        let list = DeviceList {
            microphones: vec![device("a", false), device("b", true)],
            outputs: vec![device("c", false)],
        };
        assert_eq!(list.default_microphone().map(|d| d.id.as_str()), Some("b"));
        assert_eq!(list.default_output(), None);
        assert!(DeviceList::default().microphones.is_empty());
    }

    #[test]
    fn devices_fallback_is_reported_only_for_a_chosen_endpoint() {
        assert_eq!(
            fallback(TrackId::Mic, &DeviceChoice::Default, true, "x", "Mic"),
            None
        );
        let gone = fallback(TrackId::Mic, &chosen("a"), true, "not found", "Surface Mic").unwrap();
        assert_eq!(gone.track, TrackId::Mic);
        assert_eq!(gone.chosen_id, "a");
        assert_eq!(gone.chosen_name, "Headset");
        assert_eq!(gone.cause, FallbackCause::NotConnected);
        assert_eq!(gone.used, "Surface Mic");
        assert_eq!(gone.cause.to_string(), "it is not connected");
        let failed = fallback(
            TrackId::Reference,
            &chosen("c"),
            false,
            "in use",
            "Speakers",
        )
        .unwrap();
        assert_eq!(failed.cause, FallbackCause::OpenFailed("in use".into()));
        assert_eq!(failed.cause.to_string(), "it could not be opened (in use)");
    }

    #[test]
    fn devices_choice_id() {
        assert_eq!(DeviceChoice::Default.id(), None);
        assert_eq!(chosen("a").id(), Some("a"));
        assert_eq!(DeviceSelection::default().microphone, DeviceChoice::Default);
    }
}
