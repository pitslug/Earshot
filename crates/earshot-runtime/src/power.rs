//! The machine's power supply for the "after the call on battery" speaker-detection setting
//! (T6.13d4, [`crate::speaker_timing`]). The Windows call (`GetSystemPowerStatus`) lives in
//! the WASAPI adapter crate (all `unsafe` there), wrapped here as [`priority`](crate::priority)
//! wraps the thread-priority call. Other platforms have no power API yet and report
//! [`PowerSupply::Mains`]: the setting then behaves as "during the call", the default.
//!
//! A live session reads it through a [`PowerProbe`] (a [`PowerSource`]; tests use fakes):
//! once at the start and then every [`crate::live::LiveConfig::power_poll`] on the session
//! thread while it can still change what the session does.

use std::fmt;
use std::sync::Arc;

/// Where the machine's power comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSupply {
    /// The machine has no battery (a desktop): the battery setting does not apply.
    NoBattery,
    /// On mains power.
    Mains,
    /// On battery power.
    Battery,
    /// The OS does not know (or could not be asked): treated as mains, so speaker work
    /// runs as by default.
    Unknown,
}

/// Something that reports the power supply (the OS, or a test fake).
pub trait PowerSource: Send + Sync {
    /// The supply now. Called on the session thread; must not block.
    fn supply(&self) -> PowerSupply;
}

/// The OS's power status ([`system_power`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPower;

impl PowerSource for SystemPower {
    fn supply(&self) -> PowerSupply {
        system_power()
    }
}

/// The power supply as the OS reports it: on Windows `GetSystemPowerStatus` (a failed call
/// is [`PowerSupply::Unknown`]); elsewhere [`PowerSupply::Mains`] (module docs).
#[must_use]
pub fn system_power() -> PowerSupply {
    #[cfg(windows)]
    {
        use earshot_capture_wasapi::PowerSupply as Os;
        match earshot_capture_wasapi::wasapi::power_supply() {
            Ok(Os::NoBattery) => PowerSupply::NoBattery,
            Ok(Os::Mains) => PowerSupply::Mains,
            Ok(Os::Battery) => PowerSupply::Battery,
            Ok(Os::Unknown) | Err(_) => PowerSupply::Unknown,
        }
    }
    #[cfg(not(windows))]
    {
        PowerSupply::Mains
    }
}

/// A shared [`PowerSource`] for [`crate::live::LiveConfig`] (default:
/// [`PowerProbe::system`]). Every system probe equals every other (they read the same OS
/// state); any other probe equals only its clones (the same source).
#[derive(Clone)]
pub struct PowerProbe {
    source: Arc<dyn PowerSource>,
    system: bool,
}

impl PowerProbe {
    /// A probe reading `source` (a test fake).
    pub fn new(source: impl PowerSource + 'static) -> Self {
        Self {
            source: Arc::new(source),
            system: false,
        }
    }

    /// The OS's power status ([`SystemPower`]).
    #[must_use]
    pub fn system() -> Self {
        Self {
            source: Arc::new(SystemPower),
            system: true,
        }
    }

    /// The supply now.
    #[must_use]
    pub fn supply(&self) -> PowerSupply {
        self.source.supply()
    }
}

impl Default for PowerProbe {
    fn default() -> Self {
        Self::system()
    }
}

impl PartialEq for PowerProbe {
    fn eq(&self, other: &Self) -> bool {
        (self.system && other.system)
            || std::ptr::addr_eq(Arc::as_ptr(&self.source), Arc::as_ptr(&other.source))
    }
}

impl fmt::Debug for PowerProbe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.system {
            "PowerProbe(system)"
        } else {
            "PowerProbe(custom)"
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_system_probe_answers_without_blocking() {
        // Whatever the host is; on Windows this is the real `GetSystemPowerStatus`.
        let probe = PowerProbe::default();
        let _ = probe.supply();
        // System probes are interchangeable; a fake equals only its own clones.
        assert_eq!(probe, PowerProbe::system());
        let fake = PowerProbe::new(SystemPower);
        assert_eq!(fake, fake.clone());
        assert_ne!(fake, probe);
        assert_ne!(fake, PowerProbe::new(SystemPower));
    }
}
