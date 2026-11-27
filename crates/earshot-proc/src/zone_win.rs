//! This computer's UTC offset at an instant (T14.67, docs/sync-spec.md 18.5): the documented
//! path, `GetDynamicTimeZoneInformation` then `SystemTimeToTzSpecificLocalTimeEx` with that
//! structure (never NULL), so the zone's dynamic DST rule for the instant's own year applies.
//!
//! Four kernel32 functions and three structures, declared by hand as `job_win.rs` does (one
//! call per `unsafe` block, layouts pinned by tests, no `windows-sys` feature added).
//! `EnumDynamicTimeZoneInformation` (advapi32) is declared only for the tests.

use std::mem::MaybeUninit;

/// `FILETIME`: 100 ns ticks since 1601-01-01 UTC, split in two `u32`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low: u32,
    high: u32,
}

/// `SYSTEMTIME`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

/// `DYNAMIC_TIME_ZONE_INFORMATION` (`TIME_ZONE_INFORMATION`, then the key name and the
/// `BOOLEAN` that turns dynamic DST off).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct DynamicTimeZone {
    bias: i32,
    standard_name: [u16; 32],
    standard_date: SystemTime,
    standard_bias: i32,
    daylight_name: [u16; 32],
    daylight_date: SystemTime,
    daylight_bias: i32,
    key_name: [u16; 128],
    dynamic_daylight_time_disabled: u8,
}

/// `GetDynamicTimeZoneInformation`'s failure value.
const TIME_ZONE_ID_INVALID: u32 = u32::MAX;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn FileTimeToSystemTime(file_time: *const FileTime, system_time: *mut SystemTime) -> i32;
    fn SystemTimeToFileTime(system_time: *const SystemTime, file_time: *mut FileTime) -> i32;
    fn GetDynamicTimeZoneInformation(zone: *mut DynamicTimeZone) -> u32;
    fn SystemTimeToTzSpecificLocalTimeEx(
        zone: *const DynamicTimeZone,
        universal: *const SystemTime,
        local: *mut SystemTime,
    ) -> i32;
}

/// The active time zone, `None` when Windows cannot say.
pub(crate) fn current_zone() -> Option<DynamicTimeZone> {
    let mut zone = MaybeUninit::<DynamicTimeZone>::zeroed();
    // SAFETY: `zone` is a writable, properly sized and aligned DYNAMIC_TIME_ZONE_INFORMATION
    // (layout pinned by a test); the call only writes it.
    let id = unsafe { GetDynamicTimeZoneInformation(zone.as_mut_ptr()) };
    if id == TIME_ZONE_ID_INVALID {
        return None;
    }
    // SAFETY: zeroed is a valid value of this plain-data struct, and the call succeeded.
    Some(unsafe { zone.assume_init() })
}

/// The offset in `zone` at `ticks` (a FILETIME value already range-checked by
/// [`crate::filetime_ticks`]), through [`crate::offset_minutes`].
pub(crate) fn offset_in(zone: &DynamicTimeZone, ticks: u64) -> Option<i32> {
    let utc_file = FileTime {
        low: u32::try_from(ticks & 0xFFFF_FFFF).ok()?,
        high: u32::try_from(ticks >> 32).ok()?,
    };
    let mut utc = SystemTime::default();
    // SAFETY: both pointers name live, properly typed locals; the call reads one, writes one.
    if unsafe { FileTimeToSystemTime(&raw const utc_file, &raw mut utc) } == 0 {
        return None;
    }
    let mut local = SystemTime::default();
    // SAFETY: `zone` is a whole DYNAMIC_TIME_ZONE_INFORMATION from Windows (never NULL);
    // `utc` and `local` are live locals; the call reads the first two and writes the third.
    if unsafe { SystemTimeToTzSpecificLocalTimeEx(zone, &raw const utc, &raw mut local) } == 0 {
        return None;
    }
    let mut local_file = FileTime::default();
    // SAFETY: both pointers name live, properly typed locals; the call reads one, writes one.
    if unsafe { SystemTimeToFileTime(&raw const local, &raw mut local_file) } == 0 {
        return None;
    }
    let local_ticks = u64::from(local_file.high) << 32 | u64::from(local_file.low);
    // The SYSTEMTIME round trip keeps milliseconds only, so compare at that resolution.
    crate::offset_minutes(local_ticks / 10_000 * 10_000, ticks / 10_000 * 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn EnumDynamicTimeZoneInformation(index: u32, zone: *mut DynamicTimeZone) -> u32;
    }

    const ERROR_SUCCESS: u32 = 0;

    /// The zone whose key name is `key`, from Windows' list.
    fn zone_named(key: &str) -> DynamicTimeZone {
        for index in 0.. {
            let mut zone = MaybeUninit::<DynamicTimeZone>::zeroed();
            // SAFETY: `zone` is a writable, properly sized DYNAMIC_TIME_ZONE_INFORMATION.
            let status = unsafe { EnumDynamicTimeZoneInformation(index, zone.as_mut_ptr()) };
            assert_eq!(status, ERROR_SUCCESS, "no zone named {key}");
            // SAFETY: zeroed is a valid value, and the call succeeded.
            let zone = unsafe { zone.assume_init() };
            let end = zone.key_name.iter().position(|&c| c == 0).unwrap_or(128);
            if String::from_utf16_lossy(&zone.key_name[..end]) == key {
                return zone;
            }
        }
        unreachable!()
    }

    /// 2026-01-15 and 2026-07-15, 00:00 UTC.
    const JANUARY: u64 = 1_768_435_200_000;
    const JULY: u64 = 1_784_073_600_000;

    fn at(zone: &DynamicTimeZone, unix_ms: u64) -> Option<i32> {
        offset_in(zone, crate::filetime_ticks(unix_ms).unwrap())
    }

    #[test]
    fn zone_layouts_match_the_windows_headers() {
        assert_eq!(size_of::<FileTime>(), 8);
        assert_eq!(size_of::<SystemTime>(), 16);
        assert_eq!(size_of::<DynamicTimeZone>(), 432);
    }

    #[test]
    fn zone_explicit_zones_follow_their_own_daylight_rules() {
        let sydney = zone_named("AUS Eastern Standard Time");
        assert_eq!(at(&sydney, JANUARY), Some(660));
        assert_eq!(at(&sydney, JULY), Some(600));
        // The control: Sydney's two readings differ, so DST is applied by year and date.
        assert_ne!(at(&sydney, JANUARY), at(&sydney, JULY));
        let brisbane = zone_named("E. Australia Standard Time");
        assert_eq!(at(&brisbane, JANUARY), Some(600));
        assert_eq!(at(&brisbane, JULY), Some(600));
        assert_eq!(at(&zone_named("UTC"), JANUARY), Some(0));
        assert_eq!(at(&zone_named("India Standard Time"), JULY), Some(330));
    }

    #[test]
    fn zone_shipped_reader_is_the_explicit_path_with_the_current_zone() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let now = u64::try_from(now.as_millis()).unwrap();
        assert!(crate::utc_offset_minutes_at(now).is_some());
        let zone = current_zone().expect("Windows names the active zone");
        for instant in [JANUARY, JULY] {
            assert_eq!(crate::utc_offset_minutes_at(instant), at(&zone, instant));
        }
    }
}
