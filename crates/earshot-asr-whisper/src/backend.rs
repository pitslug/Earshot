//! Which ggml device a whisper.cpp context computes on.
//!
//! whisper.h has no call that returns the backend of a context, so this reads ggml's device
//! registry (the same registry whisper.cpp reads) and applies whisper.cpp's own selection
//! rule from `whisper_backend_init_gpu` (whisper.cpp 1.8.3, `src/whisper.cpp`): with
//! `use_gpu`, the `gpu_device`-th device of type GPU or IGPU, in registry order; otherwise,
//! or when there is none, the CPU. The result therefore reflects the devices this build
//! and this machine actually expose, not the Cargo features. The tests cross-check it
//! against whisper.cpp's own log (`whisper_backend_init_gpu: using <dev> backend` /
//! `no GPU found`).

use std::ffi::CStr;

use earshot_core::asr::{AsrError, BackendKind};
use whisper_rs_sys as sys;

/// Type of a ggml device (`enum ggml_backend_dev_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceType {
    /// `GGML_BACKEND_DEVICE_TYPE_CPU`.
    Cpu,
    /// `GGML_BACKEND_DEVICE_TYPE_GPU`.
    Gpu,
    /// `GGML_BACKEND_DEVICE_TYPE_IGPU`.
    IntegratedGpu,
    /// `GGML_BACKEND_DEVICE_TYPE_ACCEL` (or a type this build does not know).
    Accelerator,
}

/// One entry of ggml's device registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Device {
    /// Device type.
    pub kind: DeviceType,
    /// ggml device name, for example `CUDA0` or `Vulkan0`.
    pub name: String,
    /// Human-readable description, for example `NVIDIA GeForce RTX 5080`.
    pub description: String,
}

/// The device whisper.cpp picks (see the module docs), or `None` for the CPU.
pub(crate) fn select_gpu(devices: &[Device], use_gpu: bool, gpu_device: u32) -> Option<&Device> {
    if !use_gpu {
        return None;
    }
    let index = usize::try_from(gpu_device).ok()?;
    devices
        .iter()
        .filter(|d| matches!(d.kind, DeviceType::Gpu | DeviceType::IntegratedGpu))
        .nth(index)
}

/// The `gpu_device` index (among GPU and IGPU devices, as [`select_gpu`] counts) of the
/// device of backend `kind` to use: the first discrete one, else the first integrated one;
/// `None` when this build and machine have none. In a build with CUDA and Vulkan the same
/// card appears twice (`CUDA0` and `Vulkan1`), so the index picks the backend, not only the
/// card. A discrete GPU comes first because it has its own memory and is the faster device
/// (the desktop lists its integrated AMD Radeon as `Vulkan0` before the RTX 5080).
pub(crate) fn gpu_index(devices: &[Device], kind: BackendKind) -> Option<u32> {
    let gpus: Vec<&Device> = devices
        .iter()
        .filter(|d| matches!(d.kind, DeviceType::Gpu | DeviceType::IntegratedGpu))
        .collect();
    let of_kind = |t: DeviceType| {
        gpus.iter()
            .position(|d| d.kind == t && gpu_kind(d).ok() == Some(kind))
    };
    of_kind(DeviceType::Gpu)
        .or_else(|| of_kind(DeviceType::IntegratedGpu))
        .and_then(|i| u32::try_from(i).ok())
}

/// Backend family of a ggml GPU device, from its registry name.
///
/// # Errors
/// [`AsrError::Backend`] for a GPU backend Earshot does not support.
pub(crate) fn gpu_kind(device: &Device) -> Result<BackendKind, AsrError> {
    let name = device.name.as_str();
    if name.starts_with("CUDA") {
        Ok(BackendKind::Cuda)
    } else if name.starts_with("Vulkan") {
        Ok(BackendKind::Vulkan)
    } else {
        Err(AsrError::Backend(format!(
            "whisper.cpp selected unsupported GPU backend {name} ({})",
            device.description
        )))
    }
}

/// A GPU whisper.cpp can use in this build on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDevice {
    /// Its backend (CUDA or Vulkan).
    pub backend: BackendKind,
    /// ggml device name, for example `CUDA0` or `Vulkan0`.
    pub name: String,
    /// Human-readable description, for example `AMD Radeon(TM) Graphics`.
    pub description: String,
    /// An integrated GPU (sharing system memory).
    pub integrated: bool,
}

/// The GPUs of ggml's registry that whisper.cpp can pick, in registry order (the order
/// `gpu_device` counts). Empty in a CPU-only build or on a machine without such a device.
#[must_use]
pub fn gpu_devices() -> Vec<GpuDevice> {
    registry()
        .iter()
        .filter(|d| matches!(d.kind, DeviceType::Gpu | DeviceType::IntegratedGpu))
        .filter_map(|d| {
            Some(GpuDevice {
                backend: gpu_kind(d).ok()?,
                name: d.name.clone(),
                description: d.description.clone(),
                integrated: d.kind == DeviceType::IntegratedGpu,
            })
        })
        .collect()
}

/// Reads ggml's device registry.
pub(crate) fn registry() -> Vec<Device> {
    // SAFETY: ggml_backend_dev_count has no preconditions; the registry is initialised on
    // first use and is immutable afterwards (no dynamic backend loading in this build).
    let count = unsafe { sys::ggml_backend_dev_count() };
    (0..count)
        .filter_map(|i| {
            // SAFETY: `i < count`, so ggml_backend_dev_get returns a valid device handle that
            // lives for the whole process; name and description are NUL-terminated strings
            // owned by the device (null is checked).
            unsafe {
                let dev = sys::ggml_backend_dev_get(i);
                if dev.is_null() {
                    return None;
                }
                let kind = match sys::ggml_backend_dev_type(dev) {
                    sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_CPU => DeviceType::Cpu,
                    sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU => DeviceType::Gpu,
                    sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU => {
                        DeviceType::IntegratedGpu
                    }
                    _ => DeviceType::Accelerator,
                };
                Some(Device {
                    kind,
                    name: c_string(sys::ggml_backend_dev_name(dev)),
                    description: c_string(sys::ggml_backend_dev_description(dev)),
                })
            }
        })
        .collect()
}

/// # Safety
/// `ptr` is null or a valid NUL-terminated string.
unsafe fn c_string(ptr: *const std::ffi::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: non-null and NUL-terminated per the caller's contract.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(kind: DeviceType, name: &str) -> Device {
        Device {
            kind,
            name: name.into(),
            description: format!("{name} description"),
        }
    }

    #[test]
    fn cpu_when_gpu_not_requested_or_absent() {
        let devices = [dev(DeviceType::Gpu, "CUDA0"), dev(DeviceType::Cpu, "CPU")];
        assert_eq!(select_gpu(&devices, false, 0), None);
        let cpu_only = [
            dev(DeviceType::Accelerator, "BLAS"),
            dev(DeviceType::Cpu, "CPU"),
        ];
        assert_eq!(select_gpu(&cpu_only, true, 0), None);
        assert_eq!(select_gpu(&[], true, 0), None);
    }

    #[test]
    fn picks_the_nth_gpu_or_igpu_in_registry_order() {
        let devices = [
            dev(DeviceType::Cpu, "CPU"),
            dev(DeviceType::Gpu, "CUDA0"),
            dev(DeviceType::Accelerator, "BLAS"),
            dev(DeviceType::IntegratedGpu, "Vulkan0"),
        ];
        assert_eq!(select_gpu(&devices, true, 0).unwrap().name, "CUDA0");
        assert_eq!(select_gpu(&devices, true, 1).unwrap().name, "Vulkan0");
        assert_eq!(select_gpu(&devices, true, 2), None);
    }

    #[test]
    fn gpu_index_picks_the_backend_not_only_the_card() {
        let both = [
            dev(DeviceType::Gpu, "CUDA0"),
            dev(DeviceType::Accelerator, "BLAS"),
            dev(DeviceType::Gpu, "Vulkan0"),
            dev(DeviceType::IntegratedGpu, "Vulkan1"),
            dev(DeviceType::Cpu, "CPU"),
        ];
        assert_eq!(gpu_index(&both, BackendKind::Cuda), Some(0));
        assert_eq!(gpu_index(&both, BackendKind::Vulkan), Some(1));
        assert_eq!(
            select_gpu(&both, true, gpu_index(&both, BackendKind::Vulkan).unwrap())
                .unwrap()
                .name,
            "Vulkan0"
        );
        // The desktop: the integrated Radeon is Vulkan0, the RTX 5080 Vulkan1.
        let desktop = [
            dev(DeviceType::IntegratedGpu, "Vulkan0"),
            dev(DeviceType::Gpu, "Vulkan1"),
            dev(DeviceType::Cpu, "CPU"),
        ];
        assert_eq!(gpu_index(&desktop, BackendKind::Vulkan), Some(1));
        let vulkan_only = [
            dev(DeviceType::IntegratedGpu, "Vulkan0"),
            dev(DeviceType::Cpu, "CPU"),
        ];
        assert_eq!(gpu_index(&vulkan_only, BackendKind::Vulkan), Some(0));
        assert_eq!(gpu_index(&vulkan_only, BackendKind::Cuda), None);
        assert_eq!(
            gpu_index(&[dev(DeviceType::Cpu, "CPU")], BackendKind::Vulkan),
            None
        );
        assert_eq!(gpu_index(&both, BackendKind::Cpu), None);
    }

    #[test]
    fn gpu_kind_follows_the_ggml_backend_name() {
        assert_eq!(
            gpu_kind(&dev(DeviceType::Gpu, "CUDA0")).unwrap(),
            BackendKind::Cuda
        );
        assert_eq!(
            gpu_kind(&dev(DeviceType::Gpu, "CUDA1")).unwrap(),
            BackendKind::Cuda
        );
        assert_eq!(
            gpu_kind(&dev(DeviceType::IntegratedGpu, "Vulkan0")).unwrap(),
            BackendKind::Vulkan
        );
        assert!(matches!(
            gpu_kind(&dev(DeviceType::Gpu, "Metal")),
            Err(AsrError::Backend(_))
        ));
    }

    /// The linked whisper.cpp was built for the portable baseline in `.cargo/config.toml`
    /// (`GGML_NATIVE=OFF`, AVX2, no AVX-512), not for the build host's CPU (T2.5a). This
    /// checks the binary, not the config text: a stale native build (the env vars have no
    /// rebuild trigger) fails here until `cargo clean -p whisper-rs-sys`.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn whisper_cpu_backend_is_built_for_the_avx2_baseline() {
        let info = whisper_rs::print_system_info();
        let flag = |name: &str| {
            info.split('|')
                .map(str::trim)
                .find_map(|f| f.strip_prefix(name)?.trim().strip_prefix("= "))
                .map(str::trim)
        };
        assert_eq!(flag("AVX2"), Some("1"), "{info}");
        assert_eq!(flag("FMA"), Some("1"), "{info}");
        assert_ne!(flag("AVX512"), Some("1"), "{info}");
    }

    #[test]
    fn registry_always_lists_the_cpu() {
        let devices = registry();
        assert!(
            devices.iter().any(|d| d.kind == DeviceType::Cpu),
            "{devices:?}"
        );
    }
}
