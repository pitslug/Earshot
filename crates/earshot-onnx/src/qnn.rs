//! The QNN execution provider (Snapdragon Hexagon NPU) as an ONNX Runtime plugin (T8.3).
//!
//! Since ONNX Runtime 1.25 the QNN EP is not part of Microsoft's release: Qualcomm ships it
//! as a *plugin* execution provider (github.com/onnxruntime/onnxruntime-qnn, release 2.7.0,
//! "ONNX Runtime Compatibility: >= 1.24.1"), registered at run time into the official ONNX
//! Runtime loaded by [`crate::init`] (`RegisterExecutionProviderLibrary`; a session then
//! selects the plugin's device with `SessionOptionsAppendExecutionProvider_V2`). Both the
//! plugin and Qualcomm's HTP backend library are checksum-pinned in `models/manifest.toml`
//! (sets `qnn` for Windows ARM64 and `qnn-x64` for x64) and verified before anything is
//! loaded; nothing is found on `PATH`.
//!
//! - On Windows ARM64 the plugin offers the NPU as an `NPU` device; only that device is used
//!   (a QNN GPU or CPU device is refused, so the NPU is never claimed for something else).
//! - On x64 the package's `QnnHtp.dll` is Qualcomm's HTP *simulator* (meant for
//!   ahead-of-time compilation); the plugin offers it as a `CPU` device. It runs real QNN
//!   graphs on the desktop and is what the host tests use; it is reported as
//!   [`QnnHardware::Simulator`], never as the NPU, and it is never auto-selected.
//!
//! Which nodes the QNN EP actually took comes from ONNX Runtime's placement log, as for
//! every provider ([`crate::SessionBackend`]); for a model meant for the NPU any node left
//! on the CPU is an error unless the model's documented partition allows it
//! ([`crate::SessionBackend::check_partition`]).

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ort::memory::DeviceType;

use crate::runtime::{OnnxError, Provider, environment, file_sha256};

/// Version of the QNN execution provider package pinned in `models/manifest.toml`.
pub const QNN_EP_VERSION: &str = "2.7.0";
/// The plugin library inside a [`QnnPackage`] directory.
pub const QNN_PROVIDER_LIB: &str = "onnxruntime_providers_qnn.dll";
/// Qualcomm's HTP backend library inside a [`QnnPackage`] directory (the NPU on ARM64, the
/// HTP simulator on x64), passed to the plugin as `backend_path`.
pub const QNN_HTP_LIB: &str = "QnnHtp.dll";
/// Name the plugin is registered under (also its execution-provider name).
pub const QNN_REGISTRATION: &str = crate::runtime::QNN_EP;

/// One platform's QNN package: where it is extracted (relative to the models directory) and
/// the SHA-256 pins of the two libraries that are loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QnnPackage {
    /// Extraction directory, relative to the models directory (manifest `extract.into`).
    pub dir: &'static str,
    /// SHA-256 of [`QNN_PROVIDER_LIB`] (manifest member pin).
    pub provider_sha256: &'static str,
    /// SHA-256 of [`QNN_HTP_LIB`] (manifest member pin).
    pub htp_sha256: &'static str,
}

/// The Windows ARM64 package (manifest `qnn-ep-win-arm64`, set `qnn`).
pub const QNN_ARM64: QnnPackage = QnnPackage {
    dir: "onnxruntime-qnn/win-arm64-2.7.0",
    provider_sha256: "b4934e352c4989e631f37381f7303dd34dfe890e5c98396d7377710faab6795c",
    htp_sha256: "b12363877c1b7d0355467f4f16092b21a8ca0b9f5871a71459b5590378699450",
};

/// The x64 package with Qualcomm's HTP simulator (manifest `qnn-ep-win-x64`, set
/// `qnn-x64`).
pub const QNN_X64: QnnPackage = QnnPackage {
    dir: "onnxruntime-qnn/win-x64-2.7.0",
    provider_sha256: "836ec9b4da396641aa06d052909198e1ef4549b3f541b849576bf14c5e20d40b",
    htp_sha256: "d336a0337eafc9180e6a2922bcc7a5bfdc5d735210e4fda95a584e1c8de4bb77",
};

/// The package for the architecture this binary was built for.
pub const QNN_HOST: QnnPackage = if cfg!(target_arch = "aarch64") {
    QNN_ARM64
} else {
    QNN_X64
};

/// The `cargo xtask fetch-models` set of the package for this architecture.
pub const QNN_HOST_SET: &str = if cfg!(target_arch = "aarch64") {
    "qnn"
} else {
    "qnn-x64"
};

/// What the QNN EP device runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QnnHardware {
    /// The Hexagon NPU (Windows ARM64).
    Npu,
    /// Qualcomm's HTP simulator on the CPU (x64 package; tests and compilation only).
    Simulator,
}

/// The device a registered QNN plugin offers and that sessions use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QnnDevice {
    /// NPU or simulator.
    pub hardware: QnnHardware,
    /// The hardware vendor ONNX Runtime reports.
    pub vendor: String,
    /// Absolute path of the HTP backend library given to the plugin.
    pub backend_path: PathBuf,
}

/// Picks the QNN device among `(execution provider, device type)` pairs reported by ONNX
/// Runtime: on ARM64 (`arm`) only an NPU device of the QNN EP, elsewhere the simulator's CPU
/// device. Returns its index and what it is.
#[must_use]
pub fn choose_qnn_device(
    devices: &[(&str, DeviceType)],
    arm: bool,
) -> Option<(usize, QnnHardware)> {
    let wanted = if arm {
        (DeviceType::NPU, QnnHardware::Npu)
    } else {
        (DeviceType::CPU, QnnHardware::Simulator)
    };
    devices
        .iter()
        .position(|(ep, ty)| *ep == QNN_REGISTRATION && *ty == wanted.0)
        .map(|i| (i, wanted.1))
}

struct Registered {
    dir: PathBuf,
    device: QnnDevice,
    // Kept for the life of the process: sessions hold the plugin's code.
    _library: ort::ep::ExecutionProviderLibrary,
}

/// The successful registration (a failure is not kept: a later call tries again).
static QNN: OnceLock<Registered> = OnceLock::new();
/// Serialises registration attempts.
static REGISTERING: Mutex<()> = Mutex::new(());

fn verify(path: &Path, expected: &str) -> Result<(), OnnxError> {
    if !path.is_file() {
        return Err(OnnxError::Library(format!(
            "{} not found; run `cargo xtask fetch-models --set {QNN_HOST_SET}`",
            path.display()
        )));
    }
    let actual =
        file_sha256(path).map_err(|e| OnnxError::Library(format!("{}: {e}", path.display())))?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(OnnxError::Library(format!(
            "{} has SHA-256 {actual}, expected {expected}; refusing to load it \
             (re-run `cargo xtask fetch-models --set {QNN_HOST_SET} --verify`)",
            path.display()
        )));
    }
    Ok(())
}

fn failed(message: impl Into<String>) -> OnnxError {
    OnnxError::ProviderFailed {
        provider: Provider::Qnn,
        message: message.into(),
    }
}

/// Registers the QNN plugin of `package` under the models directory `models` into the
/// ONNX Runtime loaded by [`crate::init`] (call that first) and picks its device
/// ([`choose_qnn_device`] for this architecture). Both libraries must match their pins
/// before anything is loaded. Idempotent for the same package directory.
///
/// # Errors
/// [`OnnxError::Library`] for a missing or mismatched file (with the fetch command);
/// [`OnnxError::NotInitialised`] before [`crate::init`]; [`OnnxError::ProviderFailed`] when
/// the plugin does not register, offers no usable device, or another package was registered
/// already.
pub fn register_qnn(models: &Path, package: &QnnPackage) -> Result<&'static QnnDevice, OnnxError> {
    // Absolute, not canonical: the plugin gets `backend_path` as a plain string, and a
    // verbatim `\\?\` path from `canonicalize` is not what the QNN SDK expects.
    let rel = models.join(package.dir);
    let dir = std::path::absolute(&rel)
        .map_err(|e| OnnxError::Library(format!("{}: {e}", rel.display())))?;
    let same = |r: &'static Registered| -> Result<&'static QnnDevice, OnnxError> {
        if r.dir == dir {
            Ok(&r.device)
        } else {
            Err(failed(format!(
                "{} requested but the QNN plugin from {} is already registered",
                dir.display(),
                r.dir.display()
            )))
        }
    };
    // Already registered: no second hash of the 100 MB libraries.
    if let Some(r) = QNN.get() {
        return same(r);
    }
    let _guard = REGISTERING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(r) = QNN.get() {
        return same(r);
    }
    verify(&dir.join(QNN_PROVIDER_LIB), package.provider_sha256)?;
    verify(&dir.join(QNN_HTP_LIB), package.htp_sha256)?;
    let env = environment()?;
    let library = env
        .register_ep_library(QNN_REGISTRATION, dir.join(QNN_PROVIDER_LIB))
        .map_err(|e| failed(e.to_string()))?;
    let devices: Vec<(&str, DeviceType)> = env
        .devices()
        .map(|d| (d.ep().unwrap_or(""), d.hardware_device().ty()))
        .collect();
    let arm = cfg!(target_arch = "aarch64");
    let Some((index, hardware)) = choose_qnn_device(&devices, arm) else {
        let message = format!(
            "the QNN plugin offers no {} device (ONNX Runtime devices: {devices:?})",
            if arm { "NPU" } else { "HTP simulator (CPU)" }
        );
        // Unregistered again, so a later call can try afresh.
        let _ = library.unregister();
        return Err(failed(message));
    };
    let vendor = env
        .devices()
        .nth(index)
        .and_then(|d| d.hardware_device().vendor().ok().map(str::to_owned))
        .unwrap_or_default();
    let registered = QNN.get_or_init(|| Registered {
        device: QnnDevice {
            hardware,
            vendor,
            backend_path: dir.join(QNN_HTP_LIB),
        },
        dir: dir.clone(),
        _library: library,
    });
    same(registered)
}

/// The QNN device registered by [`register_qnn`], if any.
#[must_use]
pub fn registered_qnn() -> Option<&'static QnnDevice> {
    QNN.get().map(|r| &r.device)
}

/// The session builder with the registered QNN device appended (backend `QnnHtp.dll`).
pub(crate) fn with_qnn_device(
    builder: ort::session::builder::SessionBuilder,
) -> Result<ort::session::builder::SessionBuilder, OnnxError> {
    let device = registered_qnn().ok_or_else(|| {
        failed("the QNN plugin is not registered; call earshot_onnx::qnn::register_qnn first")
    })?;
    let env = environment()?;
    let want = match device.hardware {
        QnnHardware::Npu => DeviceType::NPU,
        QnnHardware::Simulator => DeviceType::CPU,
    };
    let devices: Vec<_> = env
        .devices()
        .filter(|d| d.ep().ok() == Some(QNN_REGISTRATION) && d.hardware_device().ty() == want)
        .collect();
    if devices.is_empty() {
        return Err(failed("the registered QNN device has disappeared"));
    }
    let options = [(
        format!("{QNN_REGISTRATION}.backend_path"),
        device.backend_path.display().to_string(),
    )];
    builder
        .with_devices(devices, Some(&options))
        .map_err(|e| failed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qnn_device_on_arm_is_only_the_npu() {
        let devices = [
            ("CPUExecutionProvider", DeviceType::CPU),
            (QNN_REGISTRATION, DeviceType::GPU),
            (QNN_REGISTRATION, DeviceType::NPU),
        ];
        assert_eq!(
            choose_qnn_device(&devices, true),
            Some((2, QnnHardware::Npu))
        );
        // A QNN GPU or CPU device is never taken for the NPU.
        assert_eq!(choose_qnn_device(&devices[..2], true), None);
        assert_eq!(
            choose_qnn_device(&[(QNN_REGISTRATION, DeviceType::CPU)], true),
            None
        );
    }

    #[test]
    fn qnn_device_on_x64_is_the_simulator() {
        // Recorded on the desktop (2026-10-24, plugin 2.7.0 in ONNX Runtime 1.31): the x64
        // plugin offers its HTP simulator as a CPU device beside the CPU EP's.
        let devices = [
            ("CPUExecutionProvider", DeviceType::CPU),
            (QNN_REGISTRATION, DeviceType::CPU),
        ];
        assert_eq!(
            choose_qnn_device(&devices, false),
            Some((1, QnnHardware::Simulator))
        );
        assert_eq!(choose_qnn_device(&devices[..1], false), None);
    }

    #[test]
    fn qnn_registration_refuses_missing_and_mismatched_libraries() {
        let dir = std::env::temp_dir().join(format!("earshot-qnn-pin-{}", std::process::id()));
        let pkg = QnnPackage {
            dir: "pkg",
            ..QNN_X64
        };
        let err = register_qnn(&dir, &pkg).unwrap_err();
        match err {
            OnnxError::Library(m) => {
                assert!(m.contains("cargo xtask fetch-models --set qnn"), "{m}");
            }
            other => panic!("{other:?}"),
        }
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("pkg").join(QNN_PROVIDER_LIB), b"not the plugin").unwrap();
        std::fs::write(dir.join("pkg").join(QNN_HTP_LIB), b"not htp").unwrap();
        match register_qnn(&dir, &pkg).unwrap_err() {
            OnnxError::Library(m) => assert!(m.contains("refusing to load"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(registered_qnn().is_none(), "nothing was registered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn qnn_host_package_follows_the_target_architecture() {
        if cfg!(target_arch = "aarch64") {
            assert_eq!(QNN_HOST, QNN_ARM64);
            assert_eq!(QNN_HOST_SET, "qnn");
        } else {
            assert_eq!(QNN_HOST, QNN_X64);
            assert_eq!(QNN_HOST_SET, "qnn-x64");
        }
    }
}
