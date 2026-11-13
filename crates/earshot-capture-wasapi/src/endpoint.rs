//! Microphone endpoint classification (T6.24): is the chosen mic a Bluetooth hands-free
//! headset? Platform independent; `wasapi` reads the properties of the endpoint it opens
//! into [`EndpointProperties`].
//!
//! A classic Bluetooth headset carries a microphone only over the hands-free (HFP) or
//! headset (HSP) profile. While that profile is open, Windows records it at 8 kHz (CVSD)
//! or 16 kHz (mSBC) and switches the headset's playback from A2DP to the same narrowband
//! call link, so music and the far end of a call sound worse too. The fix is to record
//! with the computer's own microphone and keep the headset for listening.
//!
//! Evidence, from the endpoint's property store (Microsoft Learn, `PKEY_AudioEndpoint_FormFactor`
//! and `PKEY_Device_EnumeratorName`; `BTHHFENUM` is the enumerator of Windows' Bluetooth
//! hands-free profile driver, `bthhfenum.sys`):
//! - the bus: the endpoint's device enumerator (`BTHHFENUM` for hands-free audio, `BTHENUM`
//!   or `BTHLEDEVICE` for other Bluetooth devices; `USB`, `HDAUDIO` or a vendor bus otherwise),
//!   and the kernel-streaming filter path of the endpoint, which names the same bus and, for
//!   hands-free devices, the HFP or HSP service UUID;
//! - the sample rate of the endpoint's mix format: hands-free audio is 8 or 16 kHz;
//! - the form factor (`Headset` or `Handset` for a hands-free endpoint), recorded for
//!   reports only: a USB headset has the same form factor.
//!
//! A mic is a Bluetooth hands-free endpoint when the bus is Bluetooth and either a
//! hands-free marker (enumerator or service UUID) is present or the rate is at most 16 kHz.
//! A Bluetooth capture endpoint without either (a wideband LE Audio microphone) is
//! [`MicKind::Bluetooth`] and is not warned about. Nothing that is not on a Bluetooth bus
//! is ever classified as Bluetooth, whatever its rate or form factor.

use std::fmt;

/// `EndpointFormFactor` values of `PKEY_AudioEndpoint_FormFactor` (mmdeviceapi.h).
pub mod form_factor {
    /// `RemoteNetworkDevice`.
    pub const REMOTE_NETWORK_DEVICE: u32 = 0;
    /// `Speakers`.
    pub const SPEAKERS: u32 = 1;
    /// `LineLevel` (an audio interface's line input).
    pub const LINE_LEVEL: u32 = 2;
    /// `Headphones`.
    pub const HEADPHONES: u32 = 3;
    /// `Microphone`.
    pub const MICROPHONE: u32 = 4;
    /// `Headset`.
    pub const HEADSET: u32 = 5;
    /// `Handset`.
    pub const HANDSET: u32 = 6;
    /// `UnknownFormFactor`.
    pub const UNKNOWN: u32 = 10;
}

/// What the property store says about an endpoint (each value absent when the store has
/// none or it could not be read).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndpointProperties {
    /// Friendly name.
    pub name: String,
    /// `PKEY_Device_EnumeratorName` (for example `USB`, `HDAUDIO`, `BTHHFENUM`).
    pub enumerator: Option<String>,
    /// `PKEY_AudioEndpoint_FormFactor` ([`form_factor`]).
    pub form_factor: Option<u32>,
    /// The device interface path of the endpoint's kernel-streaming filter (property
    /// `{233164c8-1b2c-4c7d-bc68-b671687a2567},1`, for example
    /// `{2}.\\?\usb#vid_1234&pid_5678&mi_02#...\global`).
    pub filter_path: Option<String>,
    /// Sample rate of the endpoint's mix format (Hz).
    pub sample_rate: Option<u32>,
    /// The endpoint id (`IMMDevice::GetId`), unique per endpoint where friendly names
    /// are not (two identical headsets).
    pub id: Option<String>,
    /// `PKEY_Devices_AudioDevice_RawProcessingSupported` (a `VT_BOOL`): whether the endpoint
    /// can deliver a stream without the signal processing Windows or the device's driver
    /// applies by default ([`raw_support`]). `None` when the store has no such boolean.
    pub raw_processing: Option<bool>,
}

/// What signal processing a microphone stream is opened with (T12.32). Serialised as
/// `default` or `raw` (a raw recording's `timestamps.json`, schema v5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MicProcessing {
    /// The stream declares nothing: Windows applies the endpoint's default processing
    /// (on some machines the vendor's noise suppression or echo control). What the app and
    /// every live session use.
    #[default]
    Default,
    /// `AUDCLNT_STREAMOPTIONS_RAW`: the stream asks for the endpoint's signal without that
    /// processing. An explicit request of the raw recorder for measurements: an endpoint
    /// that cannot deliver it refuses the stream ([`raw_refusal`]) and is never opened
    /// with the default processing instead.
    Raw,
}

/// Whether an endpoint can be opened raw, as its property store says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawSupport {
    /// The endpoint says it supports raw processing.
    Supported,
    /// The endpoint says it does not.
    NotSupported,
    /// The property is missing or could not be read.
    Unknown,
}

/// What `p` says about raw capture (Microsoft Learn, "Audio Signal Processing Modes":
/// `PKEY_Devices_AudioDevice_RawProcessingSupported` is true when the endpoint supports the
/// raw signal processing mode). Platform independent.
#[must_use]
pub fn raw_support(p: &EndpointProperties) -> RawSupport {
    match p.raw_processing {
        Some(true) => RawSupport::Supported,
        Some(false) => RawSupport::NotSupported,
        None => RawSupport::Unknown,
    }
}

/// Why a raw microphone stream was refused. Raw is asked for explicitly, so every refusal
/// fails the open: nothing falls back to the processed stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RawRefusal {
    /// The endpoint says it does not support raw processing.
    NotSupported {
        /// The endpoint's friendly name.
        device_name: String,
    },
    /// The endpoint does not say whether it supports raw processing.
    Unknown {
        /// The endpoint's friendly name.
        device_name: String,
    },
    /// Raw was asked for on a stream that is not a microphone (a loopback).
    NotAMicrophone,
}

impl fmt::Display for RawRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSupported { device_name } => write!(
                f,
                "the mic {device_name:?} cannot be opened raw: Windows says it does not \
                 support raw processing"
            )?,
            Self::Unknown { device_name } => write!(
                f,
                "the mic {device_name:?} cannot be opened raw: Windows does not say whether \
                 it supports raw processing"
            )?,
            Self::NotAMicrophone => {
                return f.write_str("only a microphone stream can be opened raw");
            }
        }
        f.write_str("; raw was asked for, so it is not opened with Windows' processing instead")
    }
}

/// The refusal for a raw stream on the microphone `p`, or `None` when it may be opened raw:
/// only an endpoint that says it supports raw processing may.
#[must_use]
pub fn raw_refusal(p: &EndpointProperties) -> Option<RawRefusal> {
    let device_name = p.name.clone();
    match raw_support(p) {
        RawSupport::Supported => None,
        RawSupport::NotSupported => Some(RawRefusal::NotSupported { device_name }),
        RawSupport::Unknown => Some(RawRefusal::Unknown { device_name }),
    }
}

/// Enumerators of Bluetooth devices.
const BLUETOOTH_ENUMERATORS: &[&str] = &["BTHENUM", "BTHHFENUM", "BTHLEDEVICE", "BTHLE"];
/// The enumerator of Bluetooth hands-free audio.
const HANDS_FREE_ENUMERATOR: &str = "BTHHFENUM";
/// Bluetooth service class UUIDs (16-bit, in the base UUID) of the hands-free and headset
/// profiles: Headset 0x1108, Headset Audio Gateway 0x1112, Handsfree 0x111E, Handsfree
/// Audio Gateway 0x111F (Bluetooth Assigned Numbers, "Service Class").
const HANDS_FREE_UUIDS: &[&str] = &["0000111e-", "0000111f-", "00001108-", "00001112-"];
/// Hands-free audio runs at 8 kHz (CVSD) or 16 kHz (mSBC).
pub const HANDS_FREE_MAX_RATE: u32 = 16_000;

/// What kind of microphone an endpoint is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicKind {
    /// A Bluetooth hands-free (HFP/HSP) headset: narrowband recording, and the headset's
    /// playback degrades while the mic is open. Warn and suggest the computer's own mic.
    BluetoothHandsFree,
    /// Another Bluetooth microphone (wideband, for example LE Audio).
    Bluetooth,
    /// Not on a Bluetooth bus (USB, built-in, virtual).
    Other,
}

impl EndpointProperties {
    fn enumerator_upper(&self) -> Option<String> {
        self.enumerator
            .as_deref()
            .map(|e| e.trim().to_ascii_uppercase())
            .filter(|e| !e.is_empty())
    }

    fn path_lower(&self) -> String {
        self.filter_path
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    /// The endpoint's device is on a Bluetooth bus.
    #[must_use]
    pub fn is_bluetooth(&self) -> bool {
        let by_enumerator = self
            .enumerator_upper()
            .is_some_and(|e| BLUETOOTH_ENUMERATORS.contains(&e.as_str()));
        let path = self.path_lower();
        // The filter path starts `{n}.\\?\<enumerator>#...`.
        let by_path = BLUETOOTH_ENUMERATORS.iter().any(|e| {
            let e = e.to_ascii_lowercase();
            path.contains(&format!("\\{e}#"))
        });
        by_enumerator || by_path
    }

    /// The endpoint names the hands-free enumerator or a hands-free or headset service.
    fn has_hands_free_marker(&self) -> bool {
        let path = self.path_lower();
        self.enumerator_upper().as_deref() == Some(HANDS_FREE_ENUMERATOR)
            || path.contains("\\bthhfenum#")
            || HANDS_FREE_UUIDS.iter().any(|u| path.contains(u))
    }
}

/// Classifies a capture endpoint (module docs).
#[must_use]
pub fn classify_mic(p: &EndpointProperties) -> MicKind {
    if !p.is_bluetooth() {
        return MicKind::Other;
    }
    let narrowband = p.sample_rate.is_some_and(|r| r <= HANDS_FREE_MAX_RATE);
    if p.has_hands_free_marker() || narrowband {
        MicKind::BluetoothHandsFree
    } else {
        MicKind::Bluetooth
    }
}

/// The warning for a Bluetooth hands-free mic, or `None` for any other mic.
#[must_use]
pub fn hands_free_warning(p: &EndpointProperties) -> Option<HandsFreeMic> {
    (classify_mic(p) == MicKind::BluetoothHandsFree).then(|| HandsFreeMic {
        device_name: p.name.clone(),
        sample_rate: p.sample_rate,
    })
}

/// Decides when a mic track warns about a Bluetooth hands-free mic after a re-open
/// (T6.24): once per switch onto such a mic. Two endpoints are the same mic when both
/// carry an endpoint id and the ids are equal or, when either id is unknown, when their
/// friendly names are equal. The mic opened at the start is warned about by the caller of
/// `WasapiSource::start` (`WasapiSource::mic_warning`), so a follower starting on a
/// hands-free mic does not warn again until the track has been on another mic in between;
/// a switch from one hands-free mic straight to another warns. A failed re-open (no
/// device) changes nothing: a lost hands-free mic that comes back is not a new switch.
/// Platform independent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandsFreeFollower {
    /// The hands-free mic currently open (already warned about); `None` while the open
    /// mic is another kind.
    warned: Option<EndpointKey>,
}

/// How [`HandsFreeFollower`] tells endpoints apart.
#[derive(Clone, Debug, PartialEq, Eq)]
struct EndpointKey {
    id: Option<String>,
    name: String,
}

impl EndpointKey {
    /// The key of `p` when it is a hands-free mic.
    fn hands_free(p: &EndpointProperties) -> Option<Self> {
        hands_free_warning(p).map(|_| Self {
            id: p.id.clone(),
            name: p.name.clone(),
        })
    }

    /// Same endpoint: equal ids when both are known, else equal friendly names.
    fn same(&self, other: &Self) -> bool {
        match (&self.id, &other.id) {
            (Some(a), Some(b)) => a == b,
            _ => self.name == other.name,
        }
    }
}

impl HandsFreeFollower {
    /// A follower whose track opened `start` (`None`: properties unknown, not hands-free).
    #[must_use]
    pub fn new(start: Option<&EndpointProperties>) -> Self {
        Self {
            warned: start.and_then(EndpointKey::hands_free),
        }
    }

    /// The track re-opened a stream on `endpoint` (its properties; `None` when unknown).
    /// Returns the warning to surface, if any. A failed re-open is not reported here.
    pub fn opened(&mut self, endpoint: Option<&EndpointProperties>) -> Option<HandsFreeMic> {
        let now = endpoint.and_then(EndpointKey::hands_free);
        let was = std::mem::replace(&mut self.warned, now);
        let is_new = match (&was, &self.warned) {
            (_, None) => false,
            (Some(was), Some(now)) => !was.same(now),
            (None, Some(_)) => true,
        };
        if is_new {
            endpoint.and_then(hands_free_warning)
        } else {
            None
        }
    }
}

/// A microphone that is a Bluetooth hands-free headset (see [`hands_free_warning`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandsFreeMic {
    /// The endpoint's friendly name.
    pub device_name: String,
    /// Its mix-format rate (Hz), when known.
    pub sample_rate: Option<u32>,
}

impl fmt::Display for HandsFreeMic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the mic {:?} is a Bluetooth headset in hands-free mode",
            self.device_name
        )?;
        if let Some(rate) = self.sample_rate {
            write!(f, " ({} kHz)", f64::from(rate) / 1000.0)?;
        }
        f.write_str(
            ": Windows records it at phone quality and lowers the headset's playback while \
             it is open; record with the computer's own mic and keep the headset for listening",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An invented USB audio interface, shaped like one read from a real device (its
    /// vendor's own USB bus, `LineLevel`).
    fn usb_interface() -> EndpointProperties {
        EndpointProperties {
            name: "Line 1 + 2 (2- Acme USB Audio)".into(),
            enumerator: Some("ACMEUSB".into()),
            form_factor: Some(form_factor::LINE_LEVEL),
            filter_path: Some(
                r"{2}.\\?\acmeusb#audio&adapter#1&7d41e0a&0&#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\wc4800_3106"
                    .into(),
            ),
            sample_rate: Some(48_000),
            id: None,
            raw_processing: None,
        }
    }

    /// A USB webcam microphone (USB audio class,
    /// `Microphone`).
    fn usb_webcam() -> EndpointProperties {
        EndpointProperties {
            name: "Microphone (Acme Webcam)".into(),
            enumerator: Some("USB".into()),
            form_factor: Some(form_factor::MICROPHONE),
            filter_path: Some(
                r"{2}.\\?\usb#vid_1234&pid_5678&mi_02#9&1a2b3c4d&0&0002#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\global"
                    .into(),
            ),
            sample_rate: Some(48_000),
            id: None,
            raw_processing: None,
        }
    }

    /// A virtual (software) microphone.
    fn virtual_mic() -> EndpointProperties {
        EndpointProperties {
            name: "Microphone (Virtual Desktop Audio)".into(),
            enumerator: Some("ROOT".into()),
            form_factor: Some(form_factor::MICROPHONE),
            filter_path: Some(
                r"{2}.\\?\root#media#0000#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\wave".into(),
            ),
            sample_rate: Some(48_000),
            id: None,
            raw_processing: None,
        }
    }

    /// A laptop's built-in array microphone (High Definition Audio). Constructed from the
    /// documented property values, not read from a device: no laptop was available; replace
    /// with a recording from the Surface at GATE-01b.
    fn laptop_array() -> EndpointProperties {
        EndpointProperties {
            name: "Microphone Array (Realtek(R) Audio)".into(),
            enumerator: Some("HDAUDIO".into()),
            form_factor: Some(form_factor::MICROPHONE),
            filter_path: Some(
                r"{2}.\\?\hdaudio#func_01&ven_10ec&dev_0257&subsys_17aa22be&rev_1000#4&1d1a5f4&0&0001#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\elineinwave"
                    .into(),
            ),
            sample_rate: Some(48_000),
            id: None,
            raw_processing: None,
        }
    }

    /// A Bluetooth headset's hands-free endpoint. Constructed from the documented property
    /// values (enumerator `BTHHFENUM`, the Handsfree service UUID in the filter path,
    /// `Headset`, 16 kHz mSBC), not read from a device: there is no Bluetooth device on the
    /// desktop; the ignored device test checks a real one.
    fn bluetooth_hands_free() -> EndpointProperties {
        EndpointProperties {
            name: "Headset (Acme Headset Hands-Free)".into(),
            enumerator: Some("BTHHFENUM".into()),
            form_factor: Some(form_factor::HEADSET),
            filter_path: Some(
                r"{2}.\\?\bthhfenum#{0000111e-0000-1000-8000-00805f9b34fb}_vid&0002054c_pid&0df0#8&2a3b4c5d&0&88c9e8aabbcc#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\bthhfaudio"
                    .into(),
            ),
            sample_rate: Some(16_000),
            id: None,
            raw_processing: None,
        }
    }

    #[test]
    fn endpoint_bluetooth_hands_free_headset_is_classified_and_warned() {
        let p = bluetooth_hands_free();
        assert!(p.is_bluetooth());
        assert_eq!(classify_mic(&p), MicKind::BluetoothHandsFree);
        let w = hands_free_warning(&p).expect("a warning");
        assert_eq!(w.device_name, p.name);
        assert_eq!(w.sample_rate, Some(16_000));
        let text = w.to_string();
        assert!(text.contains("16 kHz"), "{text}");
        assert!(text.contains("computer's own mic"), "{text}");
    }

    #[test]
    fn endpoint_usb_interface_webcam_virtual_and_laptop_mics_are_not_bluetooth() {
        for p in [usb_interface(), usb_webcam(), virtual_mic(), laptop_array()] {
            assert!(!p.is_bluetooth(), "{}", p.name);
            assert_eq!(classify_mic(&p), MicKind::Other, "{}", p.name);
            assert_eq!(hands_free_warning(&p), None, "{}", p.name);
        }
    }

    #[test]
    fn endpoint_any_one_hands_free_signal_on_a_bluetooth_bus_is_enough() {
        // Only the enumerator.
        let enumerator_only = EndpointProperties {
            filter_path: None,
            sample_rate: None,
            form_factor: None,
            ..bluetooth_hands_free()
        };
        assert_eq!(classify_mic(&enumerator_only), MicKind::BluetoothHandsFree);
        // Only the filter path (enumerator unreadable), at an unknown rate.
        let path_only = EndpointProperties {
            enumerator: None,
            sample_rate: None,
            ..bluetooth_hands_free()
        };
        assert_eq!(classify_mic(&path_only), MicKind::BluetoothHandsFree);
        // A generic Bluetooth enumerator at 8 kHz (CVSD): narrowband is hands-free.
        let narrowband = EndpointProperties {
            enumerator: Some("BTHENUM".into()),
            filter_path: None,
            sample_rate: Some(8_000),
            ..bluetooth_hands_free()
        };
        assert_eq!(classify_mic(&narrowband), MicKind::BluetoothHandsFree);
        // Case and whitespace do not matter.
        let lower = EndpointProperties {
            enumerator: Some(" bthhfenum ".into()),
            filter_path: None,
            sample_rate: None,
            ..bluetooth_hands_free()
        };
        assert_eq!(classify_mic(&lower), MicKind::BluetoothHandsFree);
    }

    #[test]
    fn endpoint_wideband_bluetooth_mic_without_a_hands_free_marker_is_not_warned() {
        let le_audio = EndpointProperties {
            name: "Headset (LE Audio)".into(),
            enumerator: Some("BTHLEDEVICE".into()),
            form_factor: Some(form_factor::HEADSET),
            filter_path: None,
            sample_rate: Some(32_000),
            id: None,
            raw_processing: None,
        };
        assert_eq!(classify_mic(&le_audio), MicKind::Bluetooth);
        assert_eq!(hands_free_warning(&le_audio), None);
    }

    #[test]
    fn endpoint_narrowband_or_headset_mics_off_bluetooth_are_never_bluetooth() {
        // A USB headset at 16 kHz with the Headset form factor: not Bluetooth.
        let usb_headset = EndpointProperties {
            name: "Headset Microphone (USB Headset)".into(),
            form_factor: Some(form_factor::HEADSET),
            sample_rate: Some(16_000),
            ..usb_webcam()
        };
        assert_eq!(classify_mic(&usb_headset), MicKind::Other);
        // Nothing known at all.
        assert_eq!(classify_mic(&EndpointProperties::default()), MicKind::Other);
        // A path that merely mentions a Bluetooth word outside the bus position.
        let named = EndpointProperties {
            filter_path: Some(r"{2}.\\?\usb#vid_1&pid_2#bthenum-like#x\global".into()),
            ..usb_webcam()
        };
        assert_eq!(classify_mic(&named), MicKind::Other);
    }

    #[test]
    fn endpoint_follower_warns_once_per_switch_onto_a_hands_free_mic() {
        let hfp = bluetooth_hands_free();
        let other = usb_interface();
        // Started on the USB interface: a switch to the headset warns, once.
        let mut f = HandsFreeFollower::new(Some(&other));
        assert_eq!(f.opened(Some(&hfp)), hands_free_warning(&hfp));
        assert!(f.opened(Some(&hfp)).is_none(), "a re-open of the same kind");
        // Back to the USB interface: no warning; onto the headset again: a new switch.
        assert_eq!(f.opened(Some(&other)), None);
        assert_eq!(f.opened(Some(&hfp)), hands_free_warning(&hfp));
        // Properties unknown count as not hands-free.
        assert_eq!(f.opened(None), None);
        assert!(f.opened(Some(&hfp)).is_some());
    }

    #[test]
    fn endpoint_follower_started_on_a_hands_free_mic_leaves_that_warning_to_the_start() {
        let hfp = bluetooth_hands_free();
        let mut f = HandsFreeFollower::new(Some(&hfp));
        assert_eq!(f.opened(Some(&hfp)), None);
        assert_eq!(f.opened(Some(&usb_webcam())), None);
        assert!(f.opened(Some(&hfp)).is_some());
        // Unknown start properties: the first hands-free mic warns.
        let mut unknown = HandsFreeFollower::new(None);
        assert!(unknown.opened(Some(&hfp)).is_some());
    }

    #[test]
    fn endpoint_follower_warns_on_a_switch_between_two_hands_free_mics() {
        let a = bluetooth_hands_free();
        let b = EndpointProperties {
            name: "Buds (Hands-Free)".into(),
            ..bluetooth_hands_free()
        };
        let mut f = HandsFreeFollower::new(Some(&a));
        assert_eq!(f.opened(Some(&b)), hands_free_warning(&b));
        assert_eq!(f.opened(Some(&b)), None);
        assert_eq!(f.opened(Some(&a)), hands_free_warning(&a));
    }

    fn with_id(id: &str) -> EndpointProperties {
        EndpointProperties {
            id: Some(id.into()),
            ..bluetooth_hands_free()
        }
    }

    #[test]
    fn endpoint_follower_tells_identical_headsets_apart_by_endpoint_id() {
        let (a, b) = (
            with_id("{0.0.1.00000000}.{a}"),
            with_id("{0.0.1.00000000}.{b}"),
        );
        assert_eq!(a.name, b.name, "same friendly name");
        let mut f = HandsFreeFollower::new(Some(&a));
        assert_eq!(f.opened(Some(&b)), hands_free_warning(&b));
        assert_eq!(f.opened(Some(&b)), None, "the same endpoint again");
        assert_eq!(f.opened(Some(&a)), hands_free_warning(&a));
        // Started on `a` and re-opened on `a` (same id): no warning.
        let mut same = HandsFreeFollower::new(Some(&a));
        assert_eq!(same.opened(Some(&a.clone())), None);
    }

    #[test]
    fn endpoint_follower_without_both_ids_compares_friendly_names() {
        let a = with_id("{a}");
        let unknown = bluetooth_hands_free();
        // One id missing: the names are equal, so it is the same mic.
        let mut f = HandsFreeFollower::new(Some(&a));
        assert_eq!(f.opened(Some(&unknown)), None);
        assert_eq!(f.opened(Some(&a)), None);
        // One id missing and another name: a switch.
        let other = EndpointProperties {
            name: "Buds (Hands-Free)".into(),
            ..bluetooth_hands_free()
        };
        assert_eq!(f.opened(Some(&other)), hands_free_warning(&other));
    }

    fn raw(says: Option<bool>) -> EndpointProperties {
        EndpointProperties {
            raw_processing: says,
            ..laptop_array()
        }
    }

    #[test]
    fn endpoint_raw_support_is_what_the_property_says() {
        assert_eq!(raw_support(&raw(Some(true))), RawSupport::Supported);
        assert_eq!(raw_support(&raw(Some(false))), RawSupport::NotSupported);
        assert_eq!(raw_support(&raw(None)), RawSupport::Unknown);
        // Nothing else about the endpoint decides it.
        let bluetooth = EndpointProperties {
            raw_processing: Some(true),
            ..bluetooth_hands_free()
        };
        assert_eq!(raw_support(&bluetooth), RawSupport::Supported);
        assert_eq!(
            raw_support(&EndpointProperties::default()),
            RawSupport::Unknown
        );
    }

    #[test]
    fn endpoint_raw_is_refused_unless_the_endpoint_says_it_supports_it() {
        assert_eq!(raw_refusal(&raw(Some(true))), None);
        let name = laptop_array().name;
        assert_eq!(
            raw_refusal(&raw(Some(false))),
            Some(RawRefusal::NotSupported {
                device_name: name.clone()
            })
        );
        assert_eq!(
            raw_refusal(&raw(None)),
            Some(RawRefusal::Unknown { device_name: name }),
            "an unreadable property is a refusal, never a guess"
        );
    }

    #[test]
    fn endpoint_raw_refusals_name_the_mic_and_say_nothing_falls_back() {
        let no = raw_refusal(&raw(Some(false))).unwrap().to_string();
        assert_eq!(
            no,
            "the mic \"Microphone Array (Realtek(R) Audio)\" cannot be opened raw: Windows says \
             it does not support raw processing; raw was asked for, so it is not opened with \
             Windows' processing instead"
        );
        let unknown = raw_refusal(&raw(None)).unwrap().to_string();
        assert!(
            unknown.contains("does not say whether it supports raw processing"),
            "{unknown}"
        );
        assert!(
            unknown.ends_with("Windows' processing instead"),
            "{unknown}"
        );
        assert_eq!(
            RawRefusal::NotAMicrophone.to_string(),
            "only a microphone stream can be opened raw"
        );
    }

    #[test]
    fn endpoint_mic_processing_defaults_to_windows_own() {
        assert_eq!(MicProcessing::default(), MicProcessing::Default);
    }
}
