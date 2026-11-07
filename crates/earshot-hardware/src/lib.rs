//! The hardware facts Earshot shows and decides from (T7.4d): the processor, every display
//! adapter, the NPUs and the total memory, as the OS reports them.
//!
//! - [`read`] asks the OS (Windows: the registry for the processor name,
//!   `GetLogicalProcessorInformationEx` for the cores, DXGI for the display adapters,
//!   DXCore for the integrated flag and the NPUs, `GlobalMemoryStatusEx` for the memory).
//!   Every OS call and all `unsafe` code live in `windows.rs`; this file holds the
//!   portable rules that turn raw answers into facts, tested on fixed inputs.
//! - A fact that cannot be read is `None` (or an empty list), never a guess; a failing
//!   call never panics. Off Windows every fact is unknown.
//! - Software adapters (the Microsoft Basic Render Driver, WARP) are not hardware and are
//!   left out ([`is_software_adapter`]); an NPU is an adapter that offers machine learning
//!   but not graphics ([`is_npu`]).
//! - An NVIDIA card's compute capability is the driver's own answer through NVML
//!   ([`ComputeCapability`]), never a table of device ids that a new card would fall out of.
//!   Whether the card can use NVIDIA's libraries is one rule, [`cuda_card`].

#![deny(unsafe_code)]

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;

/// What this machine has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hardware {
    /// The processor.
    pub cpu: Cpu,
    /// The display adapters (hardware only), in the OS's order (DXGI lists the adapter of
    /// the main display first).
    pub gpus: Vec<Gpu>,
    /// Neural processing units (compute adapters that offer machine learning and no
    /// graphics).
    pub npus: Vec<Npu>,
    /// Physical memory the OS reports, in bytes (`GlobalMemoryStatusEx` `ullTotalPhys`, the
    /// installed memory minus what firmware and an integrated GPU reserve).
    pub total_memory: Option<u64>,
}

/// The processor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cpu {
    /// Its brand string, whitespace tidied ([`clean_name`]).
    pub name: Option<String>,
    /// Physical cores.
    pub physical_cores: Option<usize>,
    /// Logical processors (hardware threads).
    pub logical_cores: Option<usize>,
}

/// A hardware display adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpu {
    /// Its exact name as the driver gives it.
    pub name: String,
    /// The vendor id DXGI reports (PCI ids such as `0x10de` NVIDIA, `0x1002` AMD,
    /// `0x8086` Intel; Qualcomm's Adreno reports the ACPI id `0x4d4f5351`, "QCOM").
    pub vendor_id: u32,
    /// Video memory dedicated to it, in bytes (`None`: reported as 0, as integrated
    /// graphics usually do).
    pub dedicated_memory: Option<u64>,
    /// The user-mode driver version (`a.b.c.d`).
    pub driver_version: Option<String>,
    /// Whether it is integrated with the processor (DXCore), when known.
    pub integrated: Option<bool>,
    /// Its CUDA compute capability, as the NVIDIA driver reports it; `None` for any other
    /// vendor and whenever the driver's answer could not be read.
    pub compute_capability: Option<ComputeCapability>,
}

/// A CUDA compute capability ("12.0"): the number that says which code a card can run.
/// Ordered by major, then minor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ComputeCapability {
    /// The major version.
    pub major: u32,
    /// The minor version.
    pub minor: u32,
}

/// NVIDIA's PCI vendor id.
pub const NVIDIA_VENDOR_ID: u32 = 0x10de;

/// Whether the CUDA set this project builds (`75-real;86-real;120a-real`) holds code for a card:
/// a real binary runs on the same major with an equal or newer minor, never across a major
/// (docs/cuda-libraries.md, NVIDIA's binary rules), and `120a` on 12.0 alone. So 7.5 and up within
/// major 7 (RTX 20, GTX 16), 8.6 and up within major 8 (RTX 30, RTX 40), and exactly 12.0 (RTX 50).
/// Everything else has no code in the set, and offering it would cost a download only for the trial
/// to refuse it: 7.0 and older, 8.0, 9.x, 10.x, 12.1 and future majors.
#[must_use]
pub fn set_holds_code_for(capability: ComputeCapability) -> bool {
    match capability.major {
        7 => capability.minor >= 5,
        8 => capability.minor >= 6,
        12 => capability.minor == 0,
        _ => false,
    }
}

/// The oldest NVIDIA driver release offered them: existing CUDA 13.x applications run on 580 or
/// newer (docs/cuda-libraries.md, the 13.4 release notes).
pub const MIN_NVIDIA_DRIVER_RELEASE: u32 = 580;

/// The release number NVIDIA's own tools print for a Windows driver version: the last five digits
/// of the last two parts, as `xxx.yy`, of which this is the `xxx` (`32.0.16.1714` is 617.14, so
/// 617). `None` for anything that does not have that shape.
#[must_use]
pub fn nvidia_driver_release(version: &str) -> Option<u32> {
    let parts: Vec<&str> = version.split('.').collect();
    let [_, _, build, revision] = parts[..] else {
        return None;
    };
    let build: u32 = build.parse().ok()?;
    let revision: u32 = revision.parse().ok()?;
    (revision < 10_000).then_some((build % 10 * 10_000 + revision) / 100)
}

/// Whether `gpu` can use NVIDIA's libraries: a discrete NVIDIA card whose driver reported a
/// compute capability the set holds code for ([`set_holds_code_for`]), with a driver of
/// [`MIN_NVIDIA_DRIVER_RELEASE`] or newer. An unknown capability or driver is a no: a guess must
/// never cost someone a download of hundreds of megabytes.
#[must_use]
pub fn supports_cuda(gpu: &Gpu) -> bool {
    gpu.vendor_id == NVIDIA_VENDOR_ID
        && gpu.integrated != Some(true)
        && gpu.compute_capability.is_some_and(set_holds_code_for)
        && gpu
            .driver_version
            .as_deref()
            .and_then(nvidia_driver_release)
            .is_some_and(|r| r >= MIN_NVIDIA_DRIVER_RELEASE)
}

/// The card that makes this machine eligible for NVIDIA's libraries ([`supports_cuda`]), if any.
/// The one rule; a trial that asks the card stays as the second line (docs/cuda-install.md), for
/// the generations never run here.
#[must_use]
pub fn cuda_card(hardware: &Hardware) -> Option<&Gpu> {
    hardware.gpus.iter().find(|g| supports_cuda(g))
}

/// NVML's `pciDeviceId` for an adapter's PCI ids: the device id above the vendor id
/// (`0x2C0210DE`), which is how an NVML device is matched to a DXGI adapter.
#[must_use]
pub fn nvml_pci_device_id(vendor_id: u32, device_id: u32) -> u32 {
    (device_id << 16) | (vendor_id & 0xffff)
}

/// NVML's two `int`s as a capability; a negative or zero major is no answer.
#[must_use]
pub fn compute_capability(major: i32, minor: i32) -> Option<ComputeCapability> {
    Some(ComputeCapability {
        major: u32::try_from(major).ok().filter(|m| *m > 0)?,
        minor: u32::try_from(minor).ok()?,
    })
}

/// A neural processing unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Npu {
    /// Its name as the driver gives it.
    pub name: String,
    /// The driver version (`a.b.c.d`).
    pub driver_version: Option<String>,
}

/// The facts of this machine (module docs). Never panics; unknown facts are `None`.
#[must_use]
pub fn read() -> Hardware {
    #[cfg(windows)]
    {
        windows::read()
    }
    #[cfg(not(windows))]
    {
        Hardware::default()
    }
}

/// A name as shown: control characters and NULs dropped, runs of whitespace collapsed to
/// one space and the ends trimmed (Intel and AMD pad their brand strings). `None` when
/// nothing is left.
#[must_use]
pub fn clean_name(raw: &str) -> Option<String> {
    let words: Vec<&str> = raw
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|w| !w.is_empty())
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// A NUL-terminated UTF-16 buffer (a fixed-size name field) as a [`clean_name`].
#[must_use]
pub fn utf16_name(buf: &[u16]) -> Option<String> {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    clean_name(&String::from_utf16_lossy(&buf[..end]))
}

/// A NUL-terminated byte buffer (DXCore's `DriverDescription`, a C string) as a
/// [`clean_name`]; invalid UTF-8 is replaced rather than refused.
#[must_use]
pub fn c_name(buf: &[u8]) -> Option<String> {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    clean_name(&String::from_utf8_lossy(&buf[..end]))
}

/// A packed Windows driver version (four 16-bit parts, most significant first: DXGI's
/// `CheckInterfaceSupport` UMD version, DXCore's `DriverVersion`) as `a.b.c.d`.
#[must_use]
pub fn driver_version(packed: u64) -> String {
    format!(
        "{}.{}.{}.{}",
        (packed >> 48) & 0xffff,
        (packed >> 32) & 0xffff,
        (packed >> 16) & 0xffff,
        packed & 0xffff
    )
}

/// Microsoft's PCI vendor id (its software adapters).
pub const MICROSOFT_VENDOR_ID: u32 = 0x1414;
/// The device id of the Microsoft Basic Render Driver (WARP).
pub const BASIC_RENDER_DEVICE_ID: u32 = 0x8c;
/// `DXGI_ADAPTER_FLAG_SOFTWARE`.
pub const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;

/// Whether a DXGI adapter is software rather than hardware: flagged
/// `DXGI_ADAPTER_FLAG_SOFTWARE`, or the Microsoft Basic Render Driver (which DXGI lists
/// without the flag; its ids are documented in "Microsoft Basic Render Driver" on
/// learn.microsoft.com).
#[must_use]
pub fn is_software_adapter(flags: u32, vendor_id: u32, device_id: u32) -> bool {
    flags & DXGI_ADAPTER_FLAG_SOFTWARE != 0
        || (vendor_id == MICROSOFT_VENDOR_ID && device_id == BASIC_RENDER_DEVICE_ID)
}

/// What DXCore says about an adapter that offers machine learning
/// (`DXCORE_ADAPTER_ATTRIBUTE_D3D12_GENERIC_ML`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MlAdapter {
    /// It offers D3D12 graphics.
    pub d3d12_graphics: bool,
    /// It offers D3D11 graphics.
    pub d3d11_graphics: bool,
    /// DXCore's `IsHardware` property, when readable.
    pub hardware: Option<bool>,
}

/// Whether an adapter that offers machine learning is an NPU: no graphics of any kind
/// (a GPU also offers machine learning) and not known to be software. Microsoft's own
/// rule for enumerating NPUs ("Use DirectML on NPUs": GENERIC_ML adapters without
/// D3D12_GRAPHICS).
#[must_use]
pub fn is_npu(adapter: &MlAdapter) -> bool {
    !adapter.d3d12_graphics && !adapter.d3d11_graphics && adapter.hardware != Some(false)
}

/// `RelationProcessorCore` in `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX`.
const RELATION_PROCESSOR_CORE: u32 = 0;
/// Offset of `PROCESSOR_RELATIONSHIP.GroupCount` in a record (8-byte header, then
/// `Flags`, `EfficiencyClass`, `Reserved[20]`).
const GROUP_COUNT_OFFSET: usize = 30;
/// Offset of the first `GROUP_AFFINITY` in a record.
const GROUP_MASK_OFFSET: usize = 32;
/// Size of a `GROUP_AFFINITY` on 64-bit Windows (`KAFFINITY` mask, `Group`, `Reserved[3]`).
const GROUP_AFFINITY_SIZE: usize = 16;

/// Physical and logical cores from the buffer `GetLogicalProcessorInformationEx
/// (RelationProcessorCore, ..)` fills on 64-bit Windows: one record per physical core, its
/// group masks holding one bit per logical processor. `None` for a malformed buffer (a
/// record that runs past the end, a zero-size record) or one without cores. Records of
/// another relationship are skipped.
#[must_use]
pub fn count_cores(buf: &[u8]) -> Option<(usize, usize)> {
    let u16_at = |at: usize| -> Option<u16> {
        buf.get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |at: usize| -> Option<u32> {
        buf.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let (mut physical, mut logical, mut at) = (0usize, 0usize, 0usize);
    while at < buf.len() {
        let relationship = u32_at(at)?;
        let size = usize::try_from(u32_at(at + 4)?).ok()?;
        let end = at.checked_add(size)?;
        if size == 0 || end > buf.len() {
            return None;
        }
        if relationship == RELATION_PROCESSOR_CORE {
            let groups = usize::from(u16_at(at + GROUP_COUNT_OFFSET)?);
            let masks_end = at + GROUP_MASK_OFFSET + groups * GROUP_AFFINITY_SIZE;
            if masks_end > end {
                return None;
            }
            physical += 1;
            for g in 0..groups {
                let m = at + GROUP_MASK_OFFSET + g * GROUP_AFFINITY_SIZE;
                let mask = buf.get(m..m + 8)?;
                let mask = u64::from_le_bytes(mask.try_into().ok()?);
                logical += mask.count_ones() as usize;
            }
        }
        at = end;
    }
    (physical > 0 && logical > 0).then_some((physical, logical))
}

/// A packed driver version as a fact: 0 means "not reported" (DXCore answers 0 for the
/// Surface's Hexagon NPU), never version 0.0.0.0.
#[must_use]
pub fn known_driver_version(packed: u64) -> Option<String> {
    (packed != 0).then(|| driver_version(packed))
}

/// A dedicated memory size as a fact: 0 means "none reported".
#[must_use]
pub fn dedicated_memory(bytes: u64) -> Option<u64> {
    (bytes > 0).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_names_are_tidied() {
        assert_eq!(
            clean_name("  Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz   ").as_deref(),
            Some("Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz")
        );
        assert_eq!(
            clean_name("AMD Ryzen 9 9950X3D 16-Core Processor          \0\0").as_deref(),
            Some("AMD Ryzen 9 9950X3D 16-Core Processor")
        );
        assert_eq!(
            clean_name("Snapdragon(R) X2 Elite\t-\tX2E88100 ").as_deref(),
            Some("Snapdragon(R) X2 Elite - X2E88100")
        );
        assert_eq!(clean_name(""), None);
        assert_eq!(clean_name(" \t\0 "), None);
    }

    #[test]
    fn hardware_fixed_size_names_stop_at_the_nul() {
        let mut buf = [0u16; 128];
        for (i, c) in "NVIDIA GeForce RTX 5080".encode_utf16().enumerate() {
            buf[i] = c;
        }
        // Garbage after the terminator is ignored.
        buf[100] = u16::from(b'X');
        assert_eq!(utf16_name(&buf).as_deref(), Some("NVIDIA GeForce RTX 5080"));
        assert_eq!(utf16_name(&[0u16; 4]), None);
        // No terminator: the whole buffer.
        let full: Vec<u16> = "Adreno".encode_utf16().collect();
        assert_eq!(utf16_name(&full).as_deref(), Some("Adreno"));
        assert_eq!(
            c_name(b"Snapdragon(R) X2 Elite NPU\0junk").as_deref(),
            Some("Snapdragon(R) X2 Elite NPU")
        );
        assert_eq!(c_name(b"\0"), None);
        assert_eq!(
            c_name(b"Bad \xff byte\0").as_deref(),
            Some("Bad \u{fffd} byte")
        );
    }

    #[test]
    fn hardware_driver_versions_unpack_four_parts() {
        // NVIDIA 32.0.15.9636 as DXGI packs it.
        let packed = (32u64 << 48) | (15u64 << 16) | 9636;
        assert_eq!(driver_version(packed), "32.0.15.9636");
        assert_eq!(driver_version(0), "0.0.0.0");
        assert_eq!(driver_version(u64::MAX), "65535.65535.65535.65535");
        assert_eq!(known_driver_version(0), None);
        assert_eq!(
            known_driver_version(packed).as_deref(),
            Some("32.0.15.9636")
        );
    }

    #[test]
    fn hardware_software_adapters_are_not_hardware() {
        assert!(is_software_adapter(
            0,
            MICROSOFT_VENDOR_ID,
            BASIC_RENDER_DEVICE_ID
        ));
        assert!(is_software_adapter(
            DXGI_ADAPTER_FLAG_SOFTWARE,
            0x10de,
            0x2c02
        ));
        // A flag other than software (remote, 1) does not make it software.
        assert!(!is_software_adapter(1, 0x10de, 0x2c02));
        assert!(!is_software_adapter(0, 0x10de, 0x2c02));
        // Another Microsoft device is not the Basic Render Driver.
        assert!(!is_software_adapter(0, MICROSOFT_VENDOR_ID, 0x1));
        assert_eq!(dedicated_memory(0), None);
        assert_eq!(dedicated_memory(16 << 30), Some(16 << 30));
    }

    #[test]
    fn hardware_npus_are_ml_adapters_without_graphics() {
        let npu = MlAdapter {
            d3d12_graphics: false,
            d3d11_graphics: false,
            hardware: Some(true),
        };
        assert!(is_npu(&npu));
        // Unknown hardware flag: still an NPU (the attribute sets decide).
        assert!(is_npu(&MlAdapter {
            hardware: None,
            ..npu
        }));
        // A GPU offers ML too.
        assert!(!is_npu(&MlAdapter {
            d3d12_graphics: true,
            ..npu
        }));
        assert!(!is_npu(&MlAdapter {
            d3d11_graphics: true,
            ..npu
        }));
        // A software ML device is not an NPU.
        assert!(!is_npu(&MlAdapter {
            hardware: Some(false),
            ..npu
        }));
    }

    /// One `RelationProcessorCore` record with the given group masks.
    fn core_record(masks: &[u64]) -> Vec<u8> {
        let size = GROUP_MASK_OFFSET + masks.len() * GROUP_AFFINITY_SIZE;
        let mut r = vec![0u8; size];
        r[0..4].copy_from_slice(&RELATION_PROCESSOR_CORE.to_le_bytes());
        r[4..8].copy_from_slice(&u32::try_from(size).unwrap().to_le_bytes());
        r[GROUP_COUNT_OFFSET..GROUP_COUNT_OFFSET + 2]
            .copy_from_slice(&u16::try_from(masks.len()).unwrap().to_le_bytes());
        for (g, m) in masks.iter().enumerate() {
            let at = GROUP_MASK_OFFSET + g * GROUP_AFFINITY_SIZE;
            r[at..at + 8].copy_from_slice(&m.to_le_bytes());
            r[at + 8..at + 10].copy_from_slice(&u16::try_from(g).unwrap().to_le_bytes());
        }
        r
    }

    #[test]
    fn hardware_cores_count_records_and_mask_bits() {
        // Four cores with two threads each (SMT), then four without.
        let mut buf = Vec::new();
        for k in 0..4 {
            buf.extend(core_record(&[0b11 << (2 * k)]));
        }
        for k in 0..4 {
            buf.extend(core_record(&[1 << (8 + k)]));
        }
        assert_eq!(count_cores(&buf), Some((8, 12)));
        // Another relationship is skipped.
        let mut other = core_record(&[0xff]);
        other[0..4].copy_from_slice(&3u32.to_le_bytes());
        let mut mixed = other.clone();
        mixed.extend(core_record(&[1]));
        assert_eq!(count_cores(&mixed), Some((1, 1)));
        // Only other relationships: no answer.
        assert_eq!(count_cores(&other), None);
        assert_eq!(count_cores(&[]), None);
    }

    #[test]
    fn hardware_malformed_core_buffers_are_refused() {
        let good = core_record(&[1]);
        // Truncated record.
        assert_eq!(count_cores(&good[..good.len() - 1]), None);
        // Zero size would never advance.
        let mut zero = good.clone();
        zero[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(count_cores(&zero), None);
        // A group count larger than the record.
        let mut groups = good.clone();
        groups[GROUP_COUNT_OFFSET..GROUP_COUNT_OFFSET + 2].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(count_cores(&groups), None);
        // A header cut short.
        assert_eq!(count_cores(&good[..6]), None);
    }

    fn nvidia(cap: Option<(u32, u32)>, driver: Option<&str>) -> Gpu {
        Gpu {
            name: "NVIDIA card".into(),
            vendor_id: NVIDIA_VENDOR_ID,
            dedicated_memory: Some(8 << 30),
            driver_version: driver.map(str::to_owned),
            integrated: Some(false),
            compute_capability: cap.map(|(major, minor)| ComputeCapability { major, minor }),
        }
    }

    const NEW_DRIVER: Option<&str> = Some("32.0.16.1714");

    #[test]
    fn hardware_nvidia_driver_versions_read_as_their_release() {
        // Read beside `nvidia-smi` on the desktop: 617.14.
        assert_eq!(nvidia_driver_release("32.0.16.1714"), Some(617));
        assert_eq!(nvidia_driver_release("32.0.15.8000"), Some(580));
        assert_eq!(nvidia_driver_release("32.0.15.7999"), Some(579));
        // A build part with a leading zero digit: 27.21.14.5671 is 456.71.
        assert_eq!(nvidia_driver_release("27.21.14.5671"), Some(456));
        assert_eq!(nvidia_driver_release("32.0.16"), None);
        assert_eq!(nvidia_driver_release("32.0.16.1714.0"), None);
        assert_eq!(nvidia_driver_release("a.b.c.d"), None);
        assert_eq!(nvidia_driver_release("32.0.16.17140"), None);
        assert_eq!(nvidia_driver_release(""), None);
    }

    #[test]
    fn hardware_compute_capability_counts_only_what_the_set_holds() {
        let cap = |major, minor| ComputeCapability { major, minor };
        for (major, minor) in [(7, 5), (7, 6), (8, 6), (8, 7), (8, 9), (12, 0)] {
            assert!(set_holds_code_for(cap(major, minor)), "{major}.{minor}");
        }
        for (major, minor) in [
            (6, 1),
            (7, 0),
            (7, 4),
            (8, 0),
            (8, 5),
            (9, 0),
            (9, 9),
            (10, 0),
            (11, 0),
            (12, 1),
            (12, 9),
            (13, 0),
            (13, 5),
        ] {
            assert!(!set_holds_code_for(cap(major, minor)), "{major}.{minor}");
        }
        assert_eq!(compute_capability(12, 0), Some(cap(12, 0)));
        assert_eq!(compute_capability(0, 0), None);
        assert_eq!(compute_capability(-1, 5), None);
        assert_eq!(compute_capability(7, -1), None);
        assert_eq!(nvml_pci_device_id(NVIDIA_VENDOR_ID, 0x2c02), 0x2c02_10de);
    }

    #[test]
    fn hardware_cuda_card_takes_only_a_known_new_enough_discrete_nvidia_card() {
        let with = |gpus: Vec<Gpu>| Hardware {
            gpus,
            ..Hardware::default()
        };
        let radeon = Gpu {
            name: "AMD Radeon(TM) Graphics".into(),
            vendor_id: 0x1002,
            dedicated_memory: Some(512 << 20),
            driver_version: Some("32.0.21001.9024".into()),
            integrated: Some(true),
            compute_capability: None,
        };

        // No NVIDIA card at all.
        assert_eq!(cuda_card(&Hardware::default()), None);
        assert_eq!(cuda_card(&with(vec![radeon.clone()])), None);
        // Another vendor that somehow reports a capability is still not one.
        let other = Gpu {
            compute_capability: Some(ComputeCapability {
                major: 12,
                minor: 0,
            }),
            driver_version: NEW_DRIVER.map(str::to_owned),
            integrated: Some(false),
            ..radeon.clone()
        };
        assert_eq!(cuda_card(&with(vec![other])), None);
        // GTX 10 class (Pascal, 6.1), and Volta (7.0, also below cuDNN's minimum).
        assert_eq!(
            cuda_card(&with(vec![nvidia(Some((6, 1)), NEW_DRIVER)])),
            None
        );
        assert_eq!(
            cuda_card(&with(vec![nvidia(Some((7, 0)), NEW_DRIVER)])),
            None
        );
        // A driver older than 580, or one that cannot be read.
        let old = Some("32.0.15.7999");
        assert_eq!(cuda_card(&with(vec![nvidia(Some((12, 0)), old)])), None);
        assert_eq!(cuda_card(&with(vec![nvidia(Some((12, 0)), None)])), None);
        assert_eq!(
            cuda_card(&with(vec![nvidia(Some((12, 0)), Some("unknown"))])),
            None
        );
        // An unknown capability is no offer, whatever the card is called.
        let mut unknown = nvidia(None, NEW_DRIVER);
        unknown.name = "NVIDIA GeForce RTX 5080".into();
        assert_eq!(cuda_card(&with(vec![unknown])), None);
        // Integrated NVIDIA graphics are not offered.
        let mut built_in = nvidia(Some((12, 1)), NEW_DRIVER);
        built_in.integrated = Some(true);
        assert_eq!(cuda_card(&with(vec![built_in])), None);

        // Newer than the set's code is no offer either: 8.0, Hopper, GB100, RTX Spark, the future.
        for cap in [(8, 0), (9, 0), (10, 0), (12, 1), (13, 0)] {
            assert_eq!(
                cuda_card(&with(vec![nvidia(Some(cap), NEW_DRIVER)])),
                None,
                "{cap:?}"
            );
        }
        // RTX 20 / GTX 16, RTX 30, RTX 40 and RTX 50.
        for cap in [(7, 5), (8, 6), (8, 9), (12, 0)] {
            assert!(
                cuda_card(&with(vec![nvidia(Some(cap), NEW_DRIVER)])).is_some(),
                "{cap:?}"
            );
        }
        // Integrated graphics first, a suitable discrete card after: the card is found. So is an
        // unsuitable NVIDIA card beside a suitable one, in either order.
        let rtx = nvidia(Some((12, 0)), NEW_DRIVER);
        assert_eq!(
            cuda_card(&with(vec![radeon, rtx.clone()])),
            Some(&rtx),
            "integrated plus discrete"
        );
        let gtx = nvidia(Some((6, 1)), NEW_DRIVER);
        assert_eq!(cuda_card(&with(vec![gtx, rtx.clone()])), Some(&rtx));
    }

    /// The real read against the driver's own tool.
    #[test]
    #[ignore = "needs an NVIDIA card: cargo test -p earshot-hardware hardware_nvml -- --ignored"]
    fn hardware_nvml_capability_and_driver_release_match_nvidia_smi() {
        let out = std::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=compute_cap,driver_version",
                "--format=csv,noheader",
            ])
            .output()
            .expect("nvidia-smi runs");
        let text = String::from_utf8(out.stdout).unwrap();
        let (cap, driver) = text.lines().next().unwrap().split_once(", ").unwrap();
        let hw = read();
        let card = hw
            .gpus
            .iter()
            .find(|g| g.vendor_id == NVIDIA_VENDOR_ID)
            .expect("an NVIDIA card is listed");
        let reported = card.compute_capability.expect("NVML answered");
        assert_eq!(format!("{}.{}", reported.major, reported.minor), cap.trim());
        let release = nvidia_driver_release(card.driver_version.as_deref().unwrap()).unwrap();
        let (major, _) = driver.trim().split_once('.').unwrap();
        assert_eq!(release.to_string(), major);
    }

    #[test]
    #[cfg_attr(not(windows), ignore = "real hardware facts are read only on Windows")]
    fn hardware_of_this_windows_host_reads_every_fact() {
        let hw = read();
        println!("{hw:#?}");
        assert!(hw.cpu.logical_cores.is_some_and(|n| n > 0));
        assert!(hw.cpu.physical_cores.is_some_and(|n| n > 0));
        assert!(hw.cpu.physical_cores <= hw.cpu.logical_cores);
        assert!(hw.total_memory.is_some_and(|m| m > 0));
        assert!(hw.cpu.name.is_some());
        for g in &hw.gpus {
            assert!(!g.name.is_empty());
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "off Windows nothing is read; on Windows see the test above"
    )]
    fn hardware_off_windows_reads_nothing() {
        assert_eq!(read(), Hardware::default());
    }
}
