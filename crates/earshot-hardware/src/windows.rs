//! The Windows reads behind [`crate::read`]. Every `unsafe` block of the crate is here,
//! each as small as the call it wraps; a failing call gives an unknown fact, never a
//! panic. The raw answers are turned into facts by the portable functions of the crate
//! root.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::{ERROR_SUCCESS, FreeLibrary, HMODULE, LUID};
use windows::Win32::Graphics::DXCore::{
    DXCORE_ADAPTER_ATTRIBUTE_D3D11_GRAPHICS, DXCORE_ADAPTER_ATTRIBUTE_D3D12_GRAPHICS,
    DXCoreAdapterProperty, DXCoreCreateAdapterFactory, DriverDescription, DriverVersion,
    IDXCoreAdapter, IDXCoreAdapterFactory, IDXCoreAdapterList, IsHardware, IsIntegrated,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GlobalMemoryStatusEx, MEMORYSTATUSEX, RelationProcessorCore,
};
use windows::core::{GUID, Interface, PCSTR, s, w};

use crate::{
    ComputeCapability, Cpu, Gpu, Hardware, MlAdapter, NVIDIA_VENDOR_ID, Npu, c_name,
    compute_capability, count_cores, dedicated_memory, is_npu, is_software_adapter,
    known_driver_version, nvml_pci_device_id, utf16_name,
};

/// `DXCORE_ADAPTER_ATTRIBUTE_D3D12_GENERIC_ML` from the Windows SDK's `dxcore_interface.h`
/// (not in the `windows` crate 0.62 metadata).
const DXCORE_ADAPTER_ATTRIBUTE_D3D12_GENERIC_ML: GUID =
    GUID::from_u128(0xb71b0d41_1088_422f_a27c_0250b7d3a988);

/// At most this many DXGI adapters are asked for (a loop guard; machines have a handful).
const MAX_ADAPTERS: u32 = 64;

pub(crate) fn read() -> Hardware {
    let (physical_cores, logical_cores) = cores().map_or((None, None), |(p, l)| (Some(p), Some(l)));
    let dxcore = dxcore_factory();
    Hardware {
        cpu: Cpu {
            name: cpu_name(),
            physical_cores,
            logical_cores,
        },
        gpus: gpus(dxcore.as_ref()),
        npus: dxcore.as_ref().map_or_else(Vec::new, npus),
        total_memory: total_memory(),
    }
}

/// The processor's brand string from the registry (written by Windows at boot on x64 and
/// ARM64 alike).
fn cpu_name() -> Option<String> {
    let key = w!(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0");
    let value = w!("ProcessorNameString");
    let mut bytes = 0u32;
    // SAFETY: a size query: no data buffer, `bytes` is a valid out pointer; the key and
    // value names are NUL-terminated static strings.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&raw mut bytes),
        )
    };
    if status != ERROR_SUCCESS || bytes == 0 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let mut len = u32::try_from(buf.len() * 2).ok()?;
    // SAFETY: `buf` holds `len` writable bytes and outlives the call; `len` is a valid
    // in/out pointer. RegGetValueW NUL-terminates REG_SZ data within `len`.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&raw mut len),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    buf.truncate((len as usize / 2).min(buf.len()));
    utf16_name(&buf)
}

/// Physical and logical cores ([`count_cores`] over the processor-core records).
fn cores() -> Option<(usize, usize)> {
    let mut len = 0u32;
    // SAFETY: a size query (no buffer); `len` is a valid out pointer. The call fails with
    // ERROR_INSUFFICIENT_BUFFER and sets `len`, which is what is wanted here.
    let _ = unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, None, &raw mut len) };
    if len == 0 {
        return None;
    }
    // u64 elements keep the buffer 8-byte aligned, as the records require.
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` is 8-byte aligned and holds at least `len` writable bytes for the
    // duration of the call; `len` is a valid in/out pointer.
    unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buf.as_mut_ptr().cast()),
            &raw mut len,
        )
    }
    .ok()?;
    let bytes: Vec<u8> = buf.iter().flat_map(|w| w.to_le_bytes()).collect();
    count_cores(&bytes[..(len as usize).min(bytes.len())])
}

/// `ullTotalPhys` (the same reading as `earshot_store::total_memory`).
fn total_memory() -> Option<u64> {
    let mut status = MEMORYSTATUSEX {
        dwLength: u32::try_from(size_of::<MEMORYSTATUSEX>()).ok()?,
        ..Default::default()
    };
    // SAFETY: `status` is a valid MEMORYSTATUSEX with `dwLength` set, as the call requires.
    unsafe { GlobalMemoryStatusEx(&raw mut status) }.ok()?;
    (status.ullTotalPhys > 0).then_some(status.ullTotalPhys)
}

fn dxcore_factory() -> Option<IDXCoreAdapterFactory> {
    // SAFETY: creates a COM object; no pointers are passed. DXCore needs no COM
    // initialisation.
    unsafe { DXCoreCreateAdapterFactory::<IDXCoreAdapterFactory>() }.ok()
}

/// The hardware display adapters from DXGI, with the integrated flag from DXCore.
fn gpus(dxcore: Option<&IDXCoreAdapterFactory>) -> Vec<Gpu> {
    // SAFETY: creates a COM object; no pointers are passed.
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for index in 0..MAX_ADAPTERS {
        // SAFETY: `factory` is a live IDXGIFactory1; an index past the last adapter
        // returns DXGI_ERROR_NOT_FOUND, which ends the loop.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        // SAFETY: `adapter` is a live IDXGIAdapter1; the descriptor is returned by value.
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        if is_software_adapter(desc.Flags, desc.VendorId, desc.DeviceId) {
            continue;
        }
        let Some(name) = utf16_name(&desc.Description) else {
            continue;
        };
        // SAFETY: `adapter` is live; the IID is a valid static GUID. The answer is the
        // user-mode driver version (documented for IDXGIDevice).
        let umd = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) };
        out.push(Gpu {
            name,
            vendor_id: desc.VendorId,
            dedicated_memory: dedicated_memory(desc.DedicatedVideoMemory as u64),
            driver_version: umd
                .ok()
                .and_then(|v| known_driver_version(v.cast_unsigned())),
            integrated: dxcore.and_then(|f| integrated(f, desc.AdapterLuid)),
            compute_capability: (desc.VendorId == NVIDIA_VENDOR_ID)
                .then(|| nvml_capability(nvml_pci_device_id(desc.VendorId, desc.DeviceId)))
                .flatten(),
        });
    }
    out
}

/// The compute capability NVML reports for the card with this `pciDeviceId`. Cards of one PCI
/// device id share a chip, so the first answer serves them all.
fn nvml_capability(pci_device_id: u32) -> Option<ComputeCapability> {
    // Read once per process: the first call starts up NVML, and a card does not change while the
    // app runs. A failed read stays unknown until the next start.
    static CARDS: OnceLock<Vec<(u32, ComputeCapability)>> = OnceLock::new();
    CARDS
        .get_or_init(|| nvml_cards().unwrap_or_default())
        .iter()
        .find(|(id, _)| *id == pci_device_id)
        .map(|(_, c)| *c)
}

/// What `GetProcAddress` returns.
type Export = unsafe extern "system" fn() -> isize;
/// `nvmlDevice_t`.
type NvmlDevice = *mut c_void;
/// `nvmlInit_v2`, `nvmlShutdown`.
type NvmlVoid = unsafe extern "C" fn() -> i32;
/// `nvmlDeviceGetCount_v2`.
type NvmlCount = unsafe extern "C" fn(*mut u32) -> i32;
/// `nvmlDeviceGetHandleByIndex_v2`.
type NvmlHandle = unsafe extern "C" fn(u32, *mut NvmlDevice) -> i32;
/// `nvmlDeviceGetPciInfo_v3`.
type NvmlPci = unsafe extern "C" fn(NvmlDevice, *mut NvmlPciInfo) -> i32;
/// `nvmlDeviceGetCudaComputeCapability`.
type NvmlCapability = unsafe extern "C" fn(NvmlDevice, *mut i32, *mut i32) -> i32;

/// `NVML_SUCCESS`.
const NVML_SUCCESS: i32 = 0;

/// `nvmlPciInfo_t` from the CUDA toolkit's `nvml.h`.
#[repr(C)]
struct NvmlPciInfo {
    bus_id_legacy: [u8; 16],
    domain: u32,
    bus: u32,
    device: u32,
    pci_device_id: u32,
    pci_sub_system_id: u32,
    bus_id: [u8; 32],
}

/// `nvml.dll`, unloaded on drop.
struct NvmlLibrary(HMODULE);

impl Drop for NvmlLibrary {
    fn drop(&mut self) {
        // SAFETY: the handle came from LoadLibraryExW and nothing of the library is used after
        // this: the function pointers live only inside `nvml_cards`.
        let _ = unsafe { FreeLibrary(self.0) };
    }
}

impl NvmlLibrary {
    /// An export by name, unconverted.
    fn export(&self, name: PCSTR) -> Option<Export> {
        // SAFETY: the module is loaded for as long as `self` lives; `name` is a static
        // NUL-terminated string.
        unsafe { GetProcAddress(self.0, name) }
    }
}

/// `nvmlShutdown` when dropped.
struct NvmlSession(NvmlVoid);

impl Drop for NvmlSession {
    fn drop(&mut self) {
        // SAFETY: pairs the nvmlInit_v2 that succeeded before this was built; the library is
        // still loaded (it is dropped after this in `nvml_cards`).
        let _ = unsafe { (self.0)() };
    }
}

/// Every NVIDIA card NVML lists, by `pciDeviceId`. The library is the driver's own, loaded only
/// from System32 (never the current folder or PATH), so nothing is added to what the exe imports
/// and a machine without it answers `None`.
fn nvml_cards() -> Option<Vec<(u32, ComputeCapability)>> {
    // SAFETY: the name is a static NUL-terminated string; the search is limited to System32.
    let library = unsafe { LoadLibraryExW(w!("nvml.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) };
    let library = NvmlLibrary(library.ok()?);
    let init = library.export(s!("nvmlInit_v2"))?;
    let shutdown = library.export(s!("nvmlShutdown"))?;
    let count = library.export(s!("nvmlDeviceGetCount_v2"))?;
    let handle = library.export(s!("nvmlDeviceGetHandleByIndex_v2"))?;
    let pci = library.export(s!("nvmlDeviceGetPciInfo_v3"))?;
    let capability = library.export(s!("nvmlDeviceGetCudaComputeCapability"))?;
    // SAFETY: each export has the signature `nvml.h` gives it (CUDA toolkit 13.4, the same names
    // versioned by NVIDIA when a signature changes); a function pointer has one representation,
    // and "C" and "system" are the same call on the 64-bit Windows targets this builds for.
    let (init, shutdown, count, handle, pci, capability) = unsafe {
        (
            std::mem::transmute::<Export, NvmlVoid>(init),
            std::mem::transmute::<Export, NvmlVoid>(shutdown),
            std::mem::transmute::<Export, NvmlCount>(count),
            std::mem::transmute::<Export, NvmlHandle>(handle),
            std::mem::transmute::<Export, NvmlPci>(pci),
            std::mem::transmute::<Export, NvmlCapability>(capability),
        )
    };
    // SAFETY: takes no arguments; a failure is its return value.
    if unsafe { init() } != NVML_SUCCESS {
        return None;
    }
    let _session = NvmlSession(shutdown);
    let mut devices = 0u32;
    // SAFETY: NVML is initialised and `devices` is a valid out pointer.
    if unsafe { count(&raw mut devices) } != NVML_SUCCESS {
        return None;
    }
    let mut out = Vec::new();
    for index in 0..devices.min(MAX_ADAPTERS) {
        let mut device: NvmlDevice = std::ptr::null_mut();
        // SAFETY: NVML is initialised, `index` is below its count and `device` is a valid out
        // pointer. A card it refuses to hand over is skipped.
        if unsafe { handle(index, &raw mut device) } != NVML_SUCCESS {
            continue;
        }
        let mut info = NvmlPciInfo {
            bus_id_legacy: [0; 16],
            domain: 0,
            bus: 0,
            device: 0,
            pci_device_id: 0,
            pci_sub_system_id: 0,
            bus_id: [0; 32],
        };
        // SAFETY: `device` came from the call above and `info` has the layout of `nvmlPciInfo_t`,
        // which the `_v3` name pins.
        if unsafe { pci(device, &raw mut info) } != NVML_SUCCESS {
            continue;
        }
        let (mut major, mut minor) = (0i32, 0i32);
        // SAFETY: `device` came from the call above; both are valid out pointers.
        if unsafe { capability(device, &raw mut major, &raw mut minor) } != NVML_SUCCESS {
            continue;
        }
        if let Some(c) = compute_capability(major, minor) {
            out.push((info.pci_device_id, c));
        }
    }
    Some(out)
}

/// DXCore's `IsIntegrated` for the adapter with `luid`.
fn integrated(factory: &IDXCoreAdapterFactory, luid: LUID) -> Option<bool> {
    // SAFETY: `factory` is live and `luid` a valid LUID that outlives the call.
    let adapter = unsafe { factory.GetAdapterByLuid::<IDXCoreAdapter>(&raw const luid) }.ok()?;
    bool_property(&adapter, IsIntegrated)
}

/// A one-byte boolean DXCore property.
fn bool_property(adapter: &IDXCoreAdapter, property: DXCoreAdapterProperty) -> Option<bool> {
    // SAFETY: `adapter` is live.
    if !unsafe { adapter.IsPropertySupported(property) } {
        return None;
    }
    let mut value = 0u8;
    // SAFETY: boolean properties are one byte; `value` is a valid one-byte buffer.
    unsafe { adapter.GetProperty(property, 1, (&raw mut value).cast()) }.ok()?;
    Some(value != 0)
}

/// The adapters DXCore lists for machine learning that offer no graphics ([`is_npu`]).
fn npus(factory: &IDXCoreAdapterFactory) -> Vec<Npu> {
    // SAFETY: `factory` is live; the filter is a valid slice of GUIDs. Windows versions
    // that do not know the attribute fail the call, which means no NPU is listed.
    let Ok(list) = (unsafe {
        factory
            .CreateAdapterList::<IDXCoreAdapterList>(&[DXCORE_ADAPTER_ATTRIBUTE_D3D12_GENERIC_ML])
    }) else {
        return Vec::new();
    };
    // SAFETY: `list` is live.
    let count = unsafe { list.GetAdapterCount() };
    let mut out = Vec::new();
    for index in 0..count {
        // SAFETY: `list` is live and `index` is below its count.
        let Ok(adapter) = (unsafe { list.GetAdapter::<IDXCoreAdapter>(index) }) else {
            continue;
        };
        // SAFETY: `adapter` is live; the attribute GUIDs are valid statics.
        let (d3d12_graphics, d3d11_graphics) = unsafe {
            (
                adapter.IsAttributeSupported(&DXCORE_ADAPTER_ATTRIBUTE_D3D12_GRAPHICS),
                adapter.IsAttributeSupported(&DXCORE_ADAPTER_ATTRIBUTE_D3D11_GRAPHICS),
            )
        };
        let facts = MlAdapter {
            d3d12_graphics,
            d3d11_graphics,
            hardware: bool_property(&adapter, IsHardware),
        };
        if !is_npu(&facts) {
            continue;
        }
        let Some(name) = description(&adapter) else {
            continue;
        };
        out.push(Npu {
            name,
            driver_version: u64_property(&adapter, DriverVersion).and_then(known_driver_version),
        });
    }
    out
}

/// DXCore's `DriverDescription` (a NUL-terminated C string).
fn description(adapter: &IDXCoreAdapter) -> Option<String> {
    // SAFETY: `adapter` is live.
    if !unsafe { adapter.IsPropertySupported(DriverDescription) } {
        return None;
    }
    // SAFETY: `adapter` is live.
    let size = unsafe { adapter.GetPropertySize(DriverDescription) }.ok()?;
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` holds `size` writable bytes, the size DXCore asked for.
    unsafe { adapter.GetProperty(DriverDescription, size, buf.as_mut_ptr().cast()) }.ok()?;
    c_name(&buf)
}

/// An eight-byte DXCore property (`DriverVersion`).
fn u64_property(adapter: &IDXCoreAdapter, property: DXCoreAdapterProperty) -> Option<u64> {
    // SAFETY: `adapter` is live.
    if !unsafe { adapter.IsPropertySupported(property) } {
        return None;
    }
    let mut value = 0u64;
    // SAFETY: `value` is a valid eight-byte buffer, the property's documented size.
    unsafe { adapter.GetProperty(property, 8, (&raw mut value).cast()) }.ok()?;
    Some(value)
}
