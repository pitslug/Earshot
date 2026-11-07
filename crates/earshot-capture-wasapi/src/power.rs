//! The machine's power supply (T6.13d4): whether it runs on mains or on battery, for the
//! "after the call on battery" speaker-detection setting. The Windows call
//! (`GetSystemPowerStatus`) is `wasapi::power_supply`; the classification of its result
//! is here, portable and unit-tested.

/// Where the machine's power comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSupply {
    /// The machine has no battery (a desktop).
    NoBattery,
    /// On mains power (a laptop plugged in).
    Mains,
    /// On battery power.
    Battery,
    /// Windows does not know.
    Unknown,
}

/// `SYSTEM_POWER_STATUS.BatteryFlag`: no system battery.
const NO_SYSTEM_BATTERY: u8 = 128;
/// `BatteryFlag` / `ACLineStatus`: status unknown.
const UNKNOWN_STATUS: u8 = 255;

/// Classifies `GetSystemPowerStatus`'s `ACLineStatus` (0 offline, 1 online, 255 unknown)
/// and `BatteryFlag` (a bit set; 128 no system battery, 255 unknown), as documented for
/// `SYSTEM_POWER_STATUS`.
#[must_use]
pub fn classify(ac_line_status: u8, battery_flag: u8) -> PowerSupply {
    if battery_flag != UNKNOWN_STATUS && battery_flag & NO_SYSTEM_BATTERY != 0 {
        return PowerSupply::NoBattery;
    }
    match ac_line_status {
        0 => PowerSupply::Battery,
        1 => PowerSupply::Mains,
        _ => PowerSupply::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_supply_classifies_the_documented_values() {
        // A desktop: no system battery, whatever the AC line says.
        assert_eq!(classify(1, 128), PowerSupply::NoBattery);
        assert_eq!(classify(255, 128), PowerSupply::NoBattery);
        // A laptop plugged in (high, charging) and unplugged (low).
        assert_eq!(classify(1, 1 | 8), PowerSupply::Mains);
        assert_eq!(classify(0, 2), PowerSupply::Battery);
        assert_eq!(classify(0, 0), PowerSupply::Battery);
        // Unknown battery state: the AC line decides.
        assert_eq!(classify(0, 255), PowerSupply::Battery);
        assert_eq!(classify(1, 255), PowerSupply::Mains);
        // Unknown AC line.
        assert_eq!(classify(255, 1), PowerSupply::Unknown);
        assert_eq!(classify(255, 255), PowerSupply::Unknown);
    }
}
