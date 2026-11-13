//! The QNN execution provider (Snapdragon Hexagon NPU) as an ONNX Runtime plugin (T8.3).
//!
//! Since ONNX Runtime 1.25 the QNN EP is not part of Microsoft's release: Qualcomm ships it
//! as a *plugin* execution provider (github.com/onnxruntime/onnxruntime-qnn, release 2.7.0,
//! "ONNX Runtime Compatibility: >= 1.24.1"), registered at run time into the official ONNX
//! Runtime loaded by [`crate::init`] (`RegisterExecutionProviderLibrary`; a session then
//! selects the plugin's device with `SessionOptionsAppendExecutionProvider_V2`). The plugin,
//! Qualcomm's HTP backend library and every companion library are checksum-pinned in
//! `models/manifest.toml` (sets `qnn` for Windows ARM64 and `qnn-x64` for x64) and verified
//! before anything is loaded; nothing is found on `PATH`. A shipped ARM64 install holds the
//! same files beside the exe (T8.4e), found there by [`package_dir`] with the same pins.
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
/// the SHA-256 pins of every library in it, the two that are loaded directly and the companions
/// Qualcomm's backend loads from the same folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QnnPackage {
    /// Extraction directory, relative to the models directory (manifest `extract.into`).
    pub dir: &'static str,
    /// SHA-256 of [`QNN_PROVIDER_LIB`] (manifest member pin).
    pub provider_sha256: &'static str,
    /// SHA-256 of [`QNN_HTP_LIB`] (manifest member pin).
    pub htp_sha256: &'static str,
    /// `(file name, SHA-256)` of every other library the manifest pins for the package (T8.4e),
    /// and on ARM64 the skels' signing catalogs (T12.12e), checked before anything is loaded,
    /// because the backend loads them from the same folder.
    pub libraries: &'static [(&'static str, &'static str)],
}

/// The Windows ARM64 package (manifest `qnn-ep-win-arm64`, set `qnn`). An ARM64 install holds
/// the same files beside the exe (T8.4e, [`package_dir`]).
pub const QNN_ARM64: QnnPackage = QnnPackage {
    dir: "onnxruntime-qnn/win-arm64-2.7.0",
    provider_sha256: "b4934e352c4989e631f37381f7303dd34dfe890e5c98396d7377710faab6795c",
    htp_sha256: "b12363877c1b7d0355467f4f16092b21a8ca0b9f5871a71459b5590378699450",
    libraries: &[
        (
            "QnnHtpPrepare.dll",
            "fbe222fa8f9bb81940956b496b0a8249c2c259c94220729ac5eb5bb3d095f3e7",
        ),
        (
            "QnnSystem.dll",
            "836d9ae9b46f1c217727d6d1c812297b13d2ed1b738de99455a4c742316e74a9",
        ),
        (
            "QnnHtpV73Stub.dll",
            "2c1ac50b3d799949e402f40ba7246805877ddd0cbd8f924970c4682cf537f76a",
        ),
        (
            "QnnHtpV81Stub.dll",
            "b2b0637fccbcfcf13a9d00723deacf5b0270d54bba639318d786ed9afc841ad4",
        ),
        (
            "libQnnHtpV73Skel.so",
            "4b53a5499d70459814079722dc4b57ba0bd23b915d50b89cdee99266a0624a28",
        ),
        (
            "libQnnHtpV81Skel.so",
            "e1eeb53f7fe1699fbe842ea2bb495a310ae7ac638c33e1ae42077d48a1ff0764",
        ),
        // The skels' signing catalogs (T12.12e): a context model compiled with them present does
        // not load without them (QNN_COMMON_ERROR_MEM_ALLOC).
        (
            "libqnnhtpv73.cat",
            "c275fd4293f3d81493b7d23ec546954d77245b6ac96a1be9ee91d19ef9186a8b",
        ),
        (
            "libqnnhtpv81.cat",
            "744c1e21b5dbd6bd7ca1f5054a5c3ec81d7b495afd6be60e699b920757b7b372",
        ),
    ],
};

/// The x64 package with Qualcomm's HTP simulator (manifest `qnn-ep-win-x64`, set
/// `qnn-x64`).
pub const QNN_X64: QnnPackage = QnnPackage {
    dir: "onnxruntime-qnn/win-x64-2.7.0",
    provider_sha256: "836ec9b4da396641aa06d052909198e1ef4549b3f541b849576bf14c5e20d40b",
    htp_sha256: "d336a0337eafc9180e6a2922bcc7a5bfdc5d735210e4fda95a584e1c8de4bb77",
    libraries: &[(
        "QnnSystem.dll",
        "5a41d85f78b76130af1bf9e9f662df2ec2ebfde6a326f8ed82b60d8f0a6d96c0",
    )],
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

/// Where `package`'s files are: beside the exe when `exe_dir` holds the plugin (a shipped
/// install, T8.4e), else its extraction under the models directory `models` (development and
/// the CLI). The same pins are checked either way ([`register_qnn_in`]).
#[must_use]
pub fn package_dir(models: &Path, package: &QnnPackage, exe_dir: Option<&Path>) -> PathBuf {
    match exe_dir {
        Some(dir) if dir.join(QNN_PROVIDER_LIB).is_file() => dir.to_path_buf(),
        _ => models.join(package.dir),
    }
}

/// The folder of the running executable, if it can be read.
#[must_use]
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

fn verify(path: &Path, expected: &str) -> Result<(), OnnxError> {
    if !path.is_file() {
        return Err(OnnxError::Library(format!(
            "{} not found; run `cargo xtask fetch-models --set {QNN_HOST_SET}` (an ARM64 \
             install holds it beside the exe)",
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

/// Registers the QNN plugin of `package` under the models directory `models`
/// ([`register_qnn_in`] on `models/<package.dir>`).
///
/// # Errors
/// As [`register_qnn_in`].
pub fn register_qnn(models: &Path, package: &QnnPackage) -> Result<&'static QnnDevice, OnnxError> {
    register_qnn_in(&models.join(package.dir), package)
}

/// Registers the QNN plugin of `package` from the folder `package_root`, which holds its files
/// directly (an extraction under the models directory, or an install's exe folder:
/// [`package_dir`]), into the ONNX Runtime loaded by [`crate::init`] (call that first) and picks
/// its device ([`choose_qnn_device`] for this architecture). Every library of the package must
/// match its pin before anything is loaded. Idempotent for the same folder.
///
/// # Errors
/// [`OnnxError::Library`] for a missing or mismatched file (with the fetch command);
/// [`OnnxError::NotInitialised`] before [`crate::init`]; [`OnnxError::ProviderFailed`] when
/// the plugin does not register, offers no usable device, or another package was registered
/// already.
pub fn register_qnn_in(
    package_root: &Path,
    package: &QnnPackage,
) -> Result<&'static QnnDevice, OnnxError> {
    // Absolute, not canonical: the plugin gets `backend_path` as a plain string, and a
    // verbatim `\\?\` path from `canonicalize` is not what the QNN SDK expects.
    let rel = package_root;
    let dir = std::path::absolute(rel)
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
    for (name, sha) in package.libraries {
        verify(&dir.join(name), sha)?;
    }
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

/// What the registered QNN device says of a context model compiled earlier (T12.12a): ONNX
/// Runtime's `GetModelCompatibilityForEpDevices` over the `ep_compatibility_info` the plugin
/// wrote into the model's metadata (the plugin's SDK and the chip it compiled for).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompiledFit {
    /// The device runs it as compiled.
    Optimal,
    /// The device runs it, but a new compile would run better.
    PreferRecompile,
    /// The device cannot run it.
    Unsupported,
    /// The plugin gives no answer (it does not implement the check).
    NotApplicable,
    /// The model carries no compatibility information for the QNN EP.
    NoInfo,
}

impl CompiledFit {
    /// Whether a load should use the compiled model as it is: anything but
    /// [`CompiledFit::Unsupported`]. [`CompiledFit::PreferRecompile`] is usable because plugin
    /// v2.7.0 always answers it for its own compiles: it writes the VTCM size as 0 ("unset") into
    /// the information (`v2:6:2.51.0:5.51.0:81:0:0:0` on the Surface) and calls only an equal VTCM
    /// optimal (`cache_compatibility/qnn_cache_compatibility_manager.cc`), so a new compile cannot
    /// change the answer (measured 2026-11-11, T12.12a). The caller's own record of what it
    /// compiled with (SDK, plugin, driver) decides the rest.
    #[must_use]
    pub fn usable(self) -> bool {
        self != CompiledFit::Unsupported
    }
}

/// Names the QNN EP to ort's compatibility API, whose metadata key is
/// `ep_compatibility_info.<NAME>`. Never registered on a session (the plugin is appended as a
/// device, [`with_qnn_device`]).
struct QnnEpName;

impl ort::ep::ExecutionProvider for QnnEpName {
    const NAME: &'static str = QNN_REGISTRATION;

    fn register(&self, _: &mut ort::session::builder::SessionBuilder) -> ort::Result<()> {
        Err(ort::Error::new(
            "names the QNN EP for compatibility information only",
        ))
    }
}

/// [`CompiledFit`] of the context model at `context_model` on the device [`register_qnn`] chose.
///
/// # Errors
/// [`OnnxError::ProviderFailed`] when the plugin is not registered or its device is gone;
/// [`OnnxError::Model`] when ONNX Runtime cannot read the model or answer.
pub fn compiled_model_fit(context_model: &Path) -> Result<CompiledFit, OnnxError> {
    use ort::device::DeviceCompatibility as D;
    let device = registered_qnn().ok_or_else(|| {
        failed("the QNN plugin is not registered; call earshot_onnx::qnn::register_qnn first")
    })?;
    let env = environment()?;
    let want = match device.hardware {
        QnnHardware::Npu => DeviceType::NPU,
        QnnHardware::Simulator => DeviceType::CPU,
    };
    let model_err = |e: ort::Error| OnnxError::Model(format!("{}: {e}", context_model.display()));
    let Some(info) =
        ort::device::CompatibilityInfo::<QnnEpName>::from_compiled_model_file(context_model)
            .map_err(model_err)?
    else {
        return Ok(CompiledFit::NoInfo);
    };
    let ep_device = env
        .devices()
        .find(|d| d.ep().ok() == Some(QNN_REGISTRATION) && d.hardware_device().ty() == want)
        .ok_or_else(|| failed("the registered QNN device has disappeared"))?;
    Ok(match ep_device.compatibility(&info).map_err(model_err)? {
        D::SupportedOptimal => CompiledFit::Optimal,
        D::SupportedPreferRecompilation => CompiledFit::PreferRecompile,
        D::Unsupported => CompiledFit::Unsupported,
        _ => CompiledFit::NotApplicable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qnn_compiled_fit_refuses_only_an_unsupported_model() {
        assert!(CompiledFit::Optimal.usable());
        assert!(CompiledFit::NotApplicable.usable());
        assert!(CompiledFit::NoInfo.usable());
        // Plugin v2.7.0's answer for its own compiles (it leaves the VTCM size unset).
        assert!(CompiledFit::PreferRecompile.usable());
        assert!(!CompiledFit::Unsupported.usable());
    }

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

    /// A package whose two loaded libraries are pinned to `provider` and `htp` and whose other
    /// library is pinned to a digest no file has.
    fn package_pinned_to(provider: &[u8], htp: &[u8]) -> QnnPackage {
        use sha2::Digest;
        let hex = |bytes: &[u8]| -> &'static str {
            let text: String = sha2::Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            Box::leak(text.into_boxed_str())
        };
        QnnPackage {
            dir: "pkg",
            provider_sha256: hex(provider),
            htp_sha256: hex(htp),
            libraries: &[(
                "QnnSystem.dll",
                "0000000000000000000000000000000000000000000000000000000000000000",
            )],
        }
    }

    #[test]
    fn qnn_registration_refuses_a_missing_or_mismatched_companion_library() {
        let root = std::env::temp_dir().join(format!("earshot-qnn-extra-{}", std::process::id()));
        let dir = root.join("pkg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(QNN_PROVIDER_LIB), b"plugin bytes").unwrap();
        std::fs::write(dir.join(QNN_HTP_LIB), b"htp bytes").unwrap();
        let pkg = package_pinned_to(b"plugin bytes", b"htp bytes");
        // The two loaded libraries match; the companion is missing, so nothing loads.
        match register_qnn_in(&dir, &pkg).unwrap_err() {
            OnnxError::Library(m) => assert!(m.contains("QnnSystem.dll"), "{m}"),
            other => panic!("{other:?}"),
        }
        std::fs::write(dir.join("QnnSystem.dll"), b"not the system library").unwrap();
        match register_qnn_in(&dir, &pkg).unwrap_err() {
            OnnxError::Library(m) => {
                assert!(
                    m.contains("QnnSystem.dll") && m.contains("refusing to load"),
                    "{m}"
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(registered_qnn().is_none(), "nothing was registered");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn qnn_package_dir_is_beside_the_exe_when_the_install_holds_it() {
        let root = std::env::temp_dir().join(format!("earshot-qnn-dir-{}", std::process::id()));
        let (models, install) = (root.join("models"), root.join("install"));
        std::fs::create_dir_all(&install).unwrap();
        // No plugin beside the exe: the models folder, as in development.
        assert_eq!(
            package_dir(&models, &QNN_ARM64, Some(&install)),
            models.join(QNN_ARM64.dir)
        );
        assert_eq!(
            package_dir(&models, &QNN_ARM64, None),
            models.join(QNN_ARM64.dir)
        );
        // A shipped install: the files beside the exe, never the models folder.
        std::fs::write(install.join(QNN_PROVIDER_LIB), b"x").unwrap();
        assert_eq!(package_dir(&models, &QNN_ARM64, Some(&install)), install);
        // A folder named like the plugin is not the plugin.
        let other = root.join("other");
        std::fs::create_dir_all(other.join(QNN_PROVIDER_LIB)).unwrap();
        assert_eq!(
            package_dir(&models, &QNN_ARM64, Some(&other)),
            models.join(QNN_ARM64.dir)
        );
        let _ = std::fs::remove_dir_all(&root);
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
