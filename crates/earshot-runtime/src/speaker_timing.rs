//! When speakers are detected (T6.13d4): a device-local setting with
//! three values.
//!
//! - [`SpeakerTiming::DuringCall`] (default): live speaker work as T6.13d3, on the
//!   session's below-normal speaker thread; the cleanup only finishes the tail.
//! - [`SpeakerTiming::AfterCall`]: no live speaker work; the cleanup diarises every track
//!   whole. [`AFTER_CALL_WARNING`] is what the user is told.
//! - [`SpeakerTiming::AfterCallOnBattery`] (laptops): during the call on mains power, after
//!   it on battery ([`crate::power`]). A machine with no battery treats it as during the
//!   call. Unplugging mid-meeting stops speaker work for the rest of the meeting (the
//!   windows finished before stay valid); plugging in mid-meeting does not start it, since
//!   speaker windows are computed from the start of the track and the audio before is no
//!   longer held.
//!
//! Whenever live speaker work does not run to the end because of this setting, the track's
//! speaker record ([`crate::speakers`]) says why, so the cleanup reports it.
//!
//! With the battery setting the session reads the supply every
//! [`crate::live::LiveConfig::power_poll`] (5 s), so after an unplug speaker work may run on
//! battery for up to that long (plus the model call in progress, which the stop interrupts).
//!
//! The preference is stored device-locally (`local_state` key [`SPEAKER_TIMING_KEY`],
//! never synced, no migration) as its [`SpeakerTiming::as_str`] name. A value this build
//! does not know (written by a newer one) reads as the default ([`StoredTiming::Unknown`]).

use earshot_store::Store;

use crate::RuntimeError;
use crate::local::DeviceRecord;
use crate::power::PowerSupply;

/// What the user is told about [`SpeakerTiming::AfterCall`] (the CLI `--help` and the
/// settings screen, T7.4).
pub const AFTER_CALL_WARNING: &str = "After the call: the final transcript and summary take \
    longer after hang-up, but no cores are used for speaker detection during the meeting.";

/// The `local_state` key of the stored preference.
pub const SPEAKER_TIMING_KEY: &str = "settings/speaker-timing";

/// When speakers are detected (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpeakerTiming {
    /// During the call (default).
    #[default]
    DuringCall,
    /// After the call.
    AfterCall,
    /// After the call on battery, during it on mains power.
    AfterCallOnBattery,
}

impl SpeakerTiming {
    /// Every value, in the order a settings screen lists them.
    pub const ALL: [Self; 3] = [Self::DuringCall, Self::AfterCall, Self::AfterCallOnBattery];

    /// The stored and command-line name: `during`, `after`, `after-on-battery`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DuringCall => "during",
            Self::AfterCall => "after",
            Self::AfterCallOnBattery => "after-on-battery",
        }
    }

    /// Reads [`SpeakerTiming::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == name)
    }

    /// How the user sees it ("During the call", ...).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::DuringCall => "During the call",
            Self::AfterCall => "After the call",
            Self::AfterCallOnBattery => "After the call on battery",
        }
    }

    /// Whether live speaker work runs at the start of a session on `supply`; `Err` with
    /// the reason recorded when it does not.
    ///
    /// # Errors
    /// The reason live speaker work does not run.
    pub fn at_start(self, supply: PowerSupply) -> Result<(), String> {
        match (self, supply) {
            (Self::DuringCall, _) => Ok(()),
            (Self::AfterCall, _) => Err(AFTER_CALL_REASON.to_owned()),
            (Self::AfterCallOnBattery, PowerSupply::Battery) => Err(ON_BATTERY_REASON.to_owned()),
            (
                Self::AfterCallOnBattery,
                PowerSupply::NoBattery | PowerSupply::Mains | PowerSupply::Unknown,
            ) => Ok(()),
        }
    }

    /// Whether a running session must watch the power supply: only the battery setting
    /// on a machine that has a battery.
    #[must_use]
    pub fn watches_power(self, supply: PowerSupply) -> bool {
        self == Self::AfterCallOnBattery && supply != PowerSupply::NoBattery
    }
}

/// The recorded reason with [`SpeakerTiming::AfterCall`].
pub const AFTER_CALL_REASON: &str = "speaker detection is set to after the call";
/// The recorded reason when the battery setting finds the machine on battery at the start.
pub const ON_BATTERY_REASON: &str =
    "speaker detection waits for the end of the call on battery power";
/// The recorded reason when the machine switched to battery during the meeting.
pub const UNPLUGGED_REASON: &str = "speaker detection stopped for the rest of the call when the computer switched to battery power";

/// The stored preference as read ([`speaker_timing`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredTiming {
    /// Nothing stored.
    Absent,
    /// A known value.
    Set(SpeakerTiming),
    /// A value this build does not know (written by a newer one), as stored.
    Unknown(String),
}

impl StoredTiming {
    /// The setting to use: the stored one, else the default.
    #[must_use]
    pub fn timing(&self) -> SpeakerTiming {
        match self {
            Self::Set(t) => *t,
            Self::Absent | Self::Unknown(_) => SpeakerTiming::default(),
        }
    }
}

/// The stored preference (one read).
///
/// # Errors
/// Store errors.
pub fn speaker_timing(store: &Store) -> Result<StoredTiming, RuntimeError> {
    let Some(bytes) = store.local_value(SPEAKER_TIMING_KEY)? else {
        return Ok(StoredTiming::Absent);
    };
    let name = String::from_utf8_lossy(&bytes).into_owned();
    Ok(match SpeakerTiming::parse(&name) {
        Some(t) => StoredTiming::Set(t),
        None => StoredTiming::Unknown(name),
    })
}

/// Stores the preference (replacing an earlier one).
///
/// # Errors
/// Store errors.
pub fn set_speaker_timing(store: &mut Store, timing: SpeakerTiming) -> Result<(), RuntimeError> {
    crate::local::set(
        store,
        &DeviceRecord::SpeakerTiming.key(),
        timing.as_str().as_bytes(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_timing_names_round_trip() {
        for t in SpeakerTiming::ALL {
            assert_eq!(SpeakerTiming::parse(t.as_str()), Some(t));
        }
        assert_eq!(SpeakerTiming::parse("During"), None);
        assert_eq!(SpeakerTiming::default(), SpeakerTiming::DuringCall);
        assert!(AFTER_CALL_WARNING.starts_with("After the call: the final transcript"));
        assert!(!AFTER_CALL_WARNING.contains('\u{2014}'));
    }

    #[test]
    fn speaker_timing_decides_each_mode_at_the_start() {
        use PowerSupply::*;
        let all = [NoBattery, Mains, Battery, Unknown];
        for p in all {
            assert_eq!(SpeakerTiming::DuringCall.at_start(p), Ok(()));
            assert_eq!(
                SpeakerTiming::AfterCall.at_start(p),
                Err(AFTER_CALL_REASON.to_owned())
            );
            assert!(!SpeakerTiming::DuringCall.watches_power(p));
            assert!(!SpeakerTiming::AfterCall.watches_power(p));
        }
        let battery = SpeakerTiming::AfterCallOnBattery;
        assert_eq!(battery.at_start(Battery), Err(ON_BATTERY_REASON.to_owned()));
        // A desktop ignores the battery mode, and never watches the power supply.
        assert_eq!(battery.at_start(NoBattery), Ok(()));
        assert!(!battery.watches_power(NoBattery));
        // Mains, or a status the OS cannot report: as during the call, watched.
        for p in [Mains, Unknown] {
            assert_eq!(battery.at_start(p), Ok(()));
            assert!(battery.watches_power(p));
        }
    }

    #[test]
    fn speaker_timing_preference_round_trips_in_the_store() {
        let dir = crate::testutil::TempDir::new("speaker-timing");
        let mut store = Store::open(&dir.0.join("earshot.db")).unwrap();
        assert_eq!(speaker_timing(&store).unwrap(), StoredTiming::Absent);
        assert_eq!(StoredTiming::Absent.timing(), SpeakerTiming::DuringCall);
        for t in SpeakerTiming::ALL {
            set_speaker_timing(&mut store, t).unwrap();
            assert_eq!(speaker_timing(&store).unwrap(), StoredTiming::Set(t));
            assert_eq!(StoredTiming::Set(t).timing(), t);
        }
        // Kept across a reopen (it is on disk, device-locally).
        set_speaker_timing(&mut store, SpeakerTiming::AfterCall).unwrap();
        drop(store);
        let mut store = Store::open(&dir.0.join("earshot.db")).unwrap();
        assert_eq!(
            speaker_timing(&store).unwrap(),
            StoredTiming::Set(SpeakerTiming::AfterCall)
        );
        // A value from a newer build: the default, with a note.
        store
            .set_local_value(SPEAKER_TIMING_KEY, b"after-on-solar")
            .unwrap();
        let stored = speaker_timing(&store).unwrap();
        assert_eq!(stored, StoredTiming::Unknown("after-on-solar".into()));
        assert_eq!(stored.timing(), SpeakerTiming::DuringCall);
    }
}
