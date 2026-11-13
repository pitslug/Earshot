use super::*;
use crate::build::ARM64_WINDOWS;
use crate::manifest::Location;

const X64: &str = crate::accept::m7::X64_WINDOWS;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("earshot-bundle-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// T8.4ad: ONNX Runtime's CUDA provider adds no import to the exe, read from a real build.
#[test]
#[ignore = "reads a built app: cargo xtask build --app --target-dir C:\\eb\\<name>, then set EARSHOT_SHIPPED_EXE to its earshot.exe and run cargo test -p xtask bundle_shipped_exe -- --ignored"]
fn bundle_shipped_exe_imports_no_cuda_dll() {
    let exe = std::env::var_os("EARSHOT_SHIPPED_EXE").expect("set EARSHOT_SHIPPED_EXE");
    let pe = read_pe(Path::new(&exe)).unwrap();
    // The reader found the table: a negative control for the empty answer below.
    assert!(
        pe.imports
            .iter()
            .any(|i| i.eq_ignore_ascii_case("KERNEL32.dll")),
        "{:?}",
        pe.imports
    );
    let cuda: Vec<&String> = pe
        .imports
        .iter()
        .filter(|i| is_vendor_accelerator_library(i) || i.to_ascii_lowercase().contains("cuda"))
        .collect();
    assert!(cuda.is_empty(), "{cuda:?}");
    println!("imports of {}: {:?}", exe.display(), pe.imports);
    check_binary(APP_EXE, &pe, X64, &build::shipped_features(Some(X64), &[])).unwrap();
}

#[test]
fn build_bundle_a_failed_check_keeps_a_failed_restore_in_its_error() {
    let restore = anyhow::anyhow!("the Run value differs");
    let both =
        with_restore_error(Err(anyhow::anyhow!("the install failed")), Some(&restore)).unwrap_err();
    let text = format!("{both:#}");
    assert!(text.contains("the install failed"), "{text}");
    assert!(text.contains("the Run value differs"), "{text}");
    // Nothing is added when there is nothing to add; a passed check goes on to its verdict.
    let alone = with_restore_error(Err(anyhow::anyhow!("the install failed")), None).unwrap_err();
    assert_eq!(format!("{alone:#}"), "the install failed");
    assert!(with_restore_error(Ok(true), Some(&restore)).unwrap());
    assert!(!with_restore_error(Ok(false), None).unwrap());
}

#[test]
fn build_bundle_makes_only_nsis_on_both_architectures() {
    assert_eq!(formats(X64).unwrap(), ["nsis"]);
    assert_eq!(formats(ARM64_WINDOWS).unwrap(), ["nsis"]);
    assert!(formats("x86_64-unknown-linux-gnu").is_err());
}

#[test]
fn build_bundle_targets_the_host_unless_told_otherwise() {
    assert_eq!(effective_target(Some(ARM64_WINDOWS)), ARM64_WINDOWS);
    assert_eq!(effective_target(Some(X64)), X64);
    let host = if cfg!(target_arch = "aarch64") {
        ARM64_WINDOWS
    } else {
        X64
    };
    assert_eq!(effective_target(None), host);
}

#[test]
fn build_bundle_installer_names_carry_the_version_and_architecture() {
    assert_eq!(
        installer_path("1.2.3", X64).unwrap(),
        PathBuf::from("nsis").join("Earshot_1.2.3_x64-setup.exe")
    );
    assert_eq!(
        installer_path("1.2.3", ARM64_WINDOWS).unwrap(),
        PathBuf::from("nsis").join("Earshot_1.2.3_arm64-setup.exe")
    );
    assert!(installer_path("1.2.3", "x86_64-unknown-linux-gnu").is_err());
}

#[test]
fn build_bundle_version_has_one_source() {
    let root = crate::repo_root();
    let version = workspace_version(&root).unwrap();
    let cargo: toml::Table = std::fs::read_to_string(root.join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        cargo["workspace"]["package"]["version"].as_str(),
        Some(version.as_str())
    );
    // Without `version` the CLI reads the app's Cargo.toml, which inherits the workspace's.
    let conf: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("apps/desktop/src-tauri/tauri.conf.json")).unwrap(),
    )
    .unwrap();
    assert!(
        conf.get("version").is_none(),
        "tauri.conf.json has a version"
    );
    let app: toml::Table = std::fs::read_to_string(root.join("apps/desktop/src-tauri/Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(app["package"]["version"]["workspace"].as_bool(), Some(true));
}

#[test]
fn build_bundle_installs_exactly_llama_dlls_and_the_runtime_and_never_an_accelerator_library() {
    let plain: Vec<&str> = crate::llama_dlls::REQUIRED
        .iter()
        .copied()
        .chain(VC_RUNTIME_DLLS)
        .collect();
    assert_eq!(resource_dlls(X64, &[]), plain);
    // A GPU build's own llama.cpp backend: code that can use CUDA, not NVIDIA's libraries (D23).
    let cuda = resource_dlls(X64, &["cuda".to_owned()]);
    assert!(cuda.contains(&"ggml-cuda.dll"));
    assert_eq!(cuda.len(), plain.len() + 1);
    for name in cuda {
        assert!(!is_vendor_accelerator_library(name), "{name}");
    }
    for bad in [
        "cublas64_13.dll",
        "CUBLASLT64_13.DLL",
        "cudnn64_9.dll",
        "cudart64_13.dll",
        "onnxruntime_providers_cuda.dll",
        "QnnHtp.dll",
        "onnxruntime_providers_qnn.dll",
        "libQnnHtpV73Skel.so",
    ] {
        assert!(is_vendor_accelerator_library(bad), "{bad}");
    }
    for fine in ["llama.dll", "ggml.dll", "kernel32.dll", "earshot.exe"] {
        assert!(!is_vendor_accelerator_library(fine), "{fine}");
    }
    assert!(refused_in_install("ggml-cuda.dll", X64, &[]));
    assert!(!refused_in_install(
        "ggml-cuda.dll",
        X64,
        &["cuda".to_owned()]
    ));
    assert!(refused_in_install(
        "cublas64_13.dll",
        X64,
        &["cuda".to_owned()]
    ));
    assert!(!refused_in_install("llama.dll", X64, &[]));
}

/// T8.4e: the QNN files the owner approved for the ARM64 install, by name (the catalogs: T12.12e).
const APPROVED_QNN_LIBRARIES: [&str; 10] = [
    "onnxruntime_providers_qnn.dll",
    "QnnHtp.dll",
    "QnnHtpPrepare.dll",
    "QnnSystem.dll",
    "QnnHtpV73Stub.dll",
    "libQnnHtpV73Skel.so",
    "QnnHtpV81Stub.dll",
    "libQnnHtpV81Skel.so",
    "libqnnhtpv73.cat",
    "libqnnhtpv81.cat",
];

#[test]
fn build_bundle_qnn_files_are_refused_on_x64_and_only_the_approved_ones_allowed_on_arm64() {
    let shipped = build::shipped_features(Some(ARM64_WINDOWS), &[]);
    for name in APPROVED_QNN_LIBRARIES {
        // Still Qualcomm's library, and never in an x64 install.
        assert!(is_vendor_accelerator_library(name), "{name}");
        assert!(refused_in_install(name, X64, &[]), "{name} on x64");
        assert!(
            refused_in_install(name, X64, &build::shipped_features(Some(X64), &[])),
            "{name} on x64"
        );
        assert!(!refused_in_install(name, ARM64_WINDOWS, &shipped), "{name}");
        assert!(
            !refused_in_install(&name.to_ascii_uppercase(), ARM64_WINDOWS, &shipped),
            "{name}: case does not count"
        );
    }
    // A QNN library the owner did not approve, and NVIDIA's libraries, stay out of an ARM64
    // install too.
    for name in [
        "QnnGpu.dll",
        "QnnSaver.dll",
        "QnnHtpV68Stub.dll",
        "libQnnHtpV68Skel.so",
        "cublas64_13.dll",
        "onnxruntime_providers_cuda.dll",
    ] {
        assert!(refused_in_install(name, ARM64_WINDOWS, &shipped), "{name}");
    }
}

#[test]
fn build_bundle_qnn_imports_are_accepted_only_on_arm64_and_never_by_the_exe() {
    let pe = |machine: u16, imports: &[&str]| Pe {
        machine,
        imports: imports.iter().map(|s| (*s).to_owned()).collect(),
    };
    let arm = machine_of(ARM64_WINDOWS).unwrap();
    let x64 = machine_of(X64).unwrap();
    // One QNN library loading another beside it.
    check_binary(
        "QnnHtp.dll",
        &pe(arm, &["KERNEL32.dll", "QnnSystem.dll"]),
        ARM64_WINDOWS,
        &[],
    )
    .unwrap();
    assert!(
        check_binary(
            "QnnHtp.dll",
            &pe(x64, &["KERNEL32.dll", "QnnSystem.dll"]),
            X64,
            &[]
        )
        .is_err()
    );
    // The exe loads the provider at run time: a static import of it would be a different design.
    for import in ["onnxruntime_providers_qnn.dll", "QnnHtp.dll"] {
        let e = check_binary(APP_EXE, &pe(arm, &[import]), ARM64_WINDOWS, &[]).unwrap_err();
        assert!(e.to_string().contains(import), "{e}");
        assert!(check_binary(APP_EXE, &pe(x64, &[import]), X64, &[]).is_err());
    }
    // An unapproved QNN library is refused everywhere.
    assert!(check_binary("x.dll", &pe(arm, &["QnnGpu.dll"]), ARM64_WINDOWS, &[]).is_err());
}

#[test]
fn build_bundle_arm64_install_holds_exactly_the_qnn_files_and_their_licences() {
    assert_eq!(QNN_LIBRARIES, APPROVED_QNN_LIBRARIES);
    let arm = expected_files(ARM64_WINDOWS);
    let x64 = expected_files(X64);
    let names: Vec<&str> = qnn_install_files().iter().map(|(_, to)| *to).collect();
    for lib in APPROVED_QNN_LIBRARIES {
        assert!(arm.contains(&PathBuf::from(lib)), "{lib}");
        assert!(names.contains(&lib), "{lib}");
    }
    for licence in [
        "Qualcomm_LICENSE.pdf",
        QNN_LICENSE_IN_INSTALL,
        QNN_NOTICES_IN_INSTALL,
        QNN_NOTICE,
    ] {
        assert!(arm.contains(&PathBuf::from(licence)), "{licence}");
    }
    // Obvious names beside the exe, never a bare `LICENSE`.
    assert_eq!(QNN_LICENSE_IN_INSTALL, "QNN-LICENSE.txt");
    assert_eq!(QNN_NOTICES_IN_INSTALL, "QNN-ThirdPartyNotices.txt");
    assert!(!arm.contains(&PathBuf::from("LICENSE")));
    // Exactly these on top of what x64 and ARM64 share, the skels' catalogs among them (T12.12e).
    assert_eq!(qnn_install_files().len(), APPROVED_QNN_LIBRARIES.len() + 3);
    // The shared part comes from the x64 list, less what only x64 holds: the Vulkan summaries'
    // files and the C++ runtime library ARM64 does not have.
    let x64_only: Vec<&str> = ["ggml-vulkan.dll", VULKAN_LOADER, VULKAN_LICENCE]
        .into_iter()
        .chain(
            VC_RUNTIME_DLLS
                .into_iter()
                .filter(|d| !vc_runtime_dlls(ARM64_WINDOWS).contains(d)),
        )
        .collect();
    let base: Vec<PathBuf> = x64
        .iter()
        .filter(|f| !x64_only.contains(&f.to_str().unwrap()))
        .cloned()
        .collect();
    assert_eq!(base.len(), x64.len() - x64_only.len(), "{x64_only:?}");
    let mut want: Vec<PathBuf> = base
        .into_iter()
        .chain(names.iter().map(PathBuf::from))
        .chain([PathBuf::from(QNN_NOTICE)])
        .collect();
    let mut have = arm.clone();
    want.sort();
    have.sort();
    assert_eq!(have, want);
    assert_eq!(arm.len(), want.len(), "no duplicates");
    let catalogs: Vec<&PathBuf> = arm
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e.eq_ignore_ascii_case("cat")))
        .collect();
    assert_eq!(
        catalogs,
        [
            &PathBuf::from("libqnnhtpv73.cat"),
            &PathBuf::from("libqnnhtpv81.cat")
        ]
    );
    // x64 holds none of it.
    for name in names.iter().chain([&QNN_NOTICE]) {
        assert!(!x64.contains(&PathBuf::from(name)), "{name} on x64");
    }
    assert!(x64.iter().all(|f| !refused_in_install(
        &f.to_string_lossy(),
        X64,
        &build::shipped_features(Some(X64), &[])
    )));
}

#[test]
fn build_bundle_qnn_files_come_from_pinned_members_of_the_manifest() {
    let m = Manifest::load(&crate::models::manifest_path(&crate::repo_root())).unwrap();
    let art = m
        .artefacts
        .iter()
        .find(|a| a.id == QNN_ARTEFACT)
        .expect("the ARM64 QNN artefact");
    assert!(art.sets.iter().any(|s| s == QNN_SET), "{:?}", art.sets);
    let x = art.extract.as_ref().expect("extracted");
    for (from, _) in qnn_install_files() {
        assert!(
            x.members.iter().any(|mm| mm.path == *from),
            "{from} must be a pinned member of {QNN_ARTEFACT}"
        );
    }
    // Every pinned library and catalog ships, so the install and the runtime's pins agree.
    for mm in &x.members {
        let library =
            mm.path.ends_with(".dll") || mm.path.ends_with(".so") || mm.path.ends_with(".cat");
        assert_eq!(
            library,
            QNN_LIBRARIES.contains(&mm.path.as_str()),
            "{}",
            mm.path
        );
    }
}

#[test]
fn build_bundle_config_carries_the_qnn_files_only_on_arm64() {
    let exe_dir = Path::new(r"C:\eb\a\release");
    let arm = bundle_config(
        exe_dir,
        Path::new("n.md"),
        Path::new("h.nsh"),
        &["nsis"],
        ARM64_WINDOWS,
        &build::shipped_features(Some(ARM64_WINDOWS), &[]),
    );
    let res = arm["bundle"]["resources"].as_object().unwrap();
    for (_, to) in qnn_install_files() {
        assert_eq!(res[exe_dir.join(to).to_str().unwrap()], *to, "{to}");
    }
    assert_eq!(res[exe_dir.join(QNN_NOTICE).to_str().unwrap()], QNN_NOTICE);
    let x64 = bundle_config(
        exe_dir,
        Path::new("n.md"),
        Path::new("h.nsh"),
        &["nsis"],
        X64,
        &[],
    );
    let res = x64["bundle"]["resources"].as_object().unwrap();
    assert!(!res.values().any(|v| {
        v.as_str()
            .is_some_and(|v| v.to_ascii_lowercase().contains("qnn") || v == QNN_NOTICE)
    }));
}

#[test]
fn build_bundle_qnn_notice_names_every_file_and_the_qualcomm_licence() {
    let text = qnn_notice_text();
    for (_, to) in qnn_install_files() {
        assert!(text.contains(to), "{to}");
    }
    assert!(text.contains("Qualcomm_LICENSE.pdf"));
    assert!(text.contains("AI Stack License"));
    assert!(text.contains("MIT"));
    assert!(!text.contains('\u{2014}'), "no em dash");
    assert!(text.is_ascii());
}

#[test]
fn build_bundle_config_turns_the_bundler_on_with_no_download_at_install_time() {
    let exe_dir = Path::new(r"C:\eb\x\release");
    let notice = Path::new(r"C:\repo\NOTICE.md");
    let c = bundle_config(exe_dir, notice, Path::new("h.nsh"), &["nsis"], X64, &[]);
    let b = &c["bundle"];
    assert_eq!(b["active"], true);
    assert_eq!(b["useLocalToolsDir"], true);
    assert_eq!(b["createUpdaterArtifacts"], false);
    assert_eq!(b["targets"], serde_json::json!(["nsis"]));
    // No admin prompt, no "everyone" page, and `/D=` honoured.
    assert_eq!(b["windows"]["nsis"]["installMode"], "currentUser");
    // WebView2's other modes fetch from Microsoft while installing or bundling (PLAN 3.9).
    assert_eq!(
        b["windows"]["webviewInstallMode"],
        serde_json::json!({ "type": "skip" })
    );
    let resources = b["resources"].as_object().unwrap();
    assert_eq!(
        resources.len(),
        crate::llama_dlls::REQUIRED.len() + VC_RUNTIME_DLLS.len() + 2
    );
    for dll in VC_RUNTIME_DLLS {
        let from = exe_dir.join(dll);
        assert_eq!(resources[from.to_str().unwrap()], dll, "beside the exe");
    }
    // The public symbols, under the name the exe's debug record gives; never the full database.
    assert_eq!(
        resources[exe_dir.join(APP_SYMBOLS_BUILT).to_str().unwrap()],
        APP_SYMBOLS
    );
    assert!(!resources.contains_key(exe_dir.join(APP_SYMBOLS).to_str().unwrap()));
    for dll in crate::llama_dlls::REQUIRED {
        let from = exe_dir.join(dll);
        assert_eq!(resources[from.to_str().unwrap()], *dll, "beside the exe");
    }
    assert_eq!(
        resources[notice.to_str().unwrap()],
        "resources/benchmark/NOTICE.md"
    );
}

/// `tauri.conf.json`, as `bundle-check` reads it.
fn tauri_conf() -> serde_json::Value {
    let path = crate::repo_root().join("apps/desktop/src-tauri/tauri.conf.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn build_bundle_config_names_the_uninstall_hook_only_in_the_overlay() {
    // The bundler resolves a relative path against its own working folder, so a whole path goes in.
    let hooks = Path::new(r"C:\repo\apps\desktop\src-tauri\nsis-hooks.nsh");
    let c = bundle_config(
        Path::new(r"C:\eb\x\release"),
        Path::new(r"C:\repo\NOTICE.md"),
        hooks,
        &["nsis"],
        X64,
        &[],
    );
    assert_eq!(
        c["bundle"]["windows"]["nsis"]["installerHooks"],
        hooks.to_str().unwrap()
    );
    // Beside the option it joins, not in its place.
    assert_eq!(c["bundle"]["windows"]["nsis"]["installMode"], "currentUser");
    assert_eq!(
        tauri_conf()["bundle"]["windows"]["nsis"]["installerHooks"],
        serde_json::Value::Null,
        "tauri.conf.json keeps no build path"
    );
    assert!(
        crate::repo_root().join(NSIS_HOOKS).is_file(),
        "{NSIS_HOOKS}"
    );
}

/// The hook file without its comments, each line trimmed.
fn hook_lines() -> Vec<String> {
    std::fs::read_to_string(crate::repo_root().join(NSIS_HOOKS))
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(';'))
        .map(str::to_owned)
        .collect()
}

#[test]
fn build_bundle_hook_path_is_whole_with_one_kind_of_separator() {
    // makensis cannot find an `!include` with both kinds, as `root.join(NSIS_HOOKS)` would give.
    let path = nsis_hooks_path(Path::new(r"C:\repo"));
    assert_eq!(
        path.to_str().unwrap(),
        r"C:\repo\apps\desktop\src-tauri\nsis-hooks.nsh"
    );
    assert!(nsis_hooks_path(&crate::repo_root()).is_file());
}

#[test]
fn build_bundle_hook_removes_one_value_after_the_uninstall_except_on_an_update() {
    let key = registry::WER_EXCLUDED_KEY.strip_prefix(r"HKCU\").unwrap();
    // Exactly this: one value of one key, with nothing that could touch the key or its other
    // values, last in the uninstall, and nothing else hooked.
    assert_eq!(
        hook_lines(),
        [
            "!macro NSIS_HOOK_POSTUNINSTALL".to_owned(),
            "${If} $UpdateMode <> 1".to_owned(),
            format!(r#"DeleteRegValue HKCU "{key}" "${{MAINBINARYNAME}}.exe""#),
            "ClearErrors".to_owned(),
            "${EndIf}".to_owned(),
            "!macroend".to_owned(),
        ]
    );
}

#[test]
fn build_bundle_hook_names_the_value_the_app_writes() {
    // `${MAINBINARYNAME}` is `mainBinaryName`, which also names the file the installer writes.
    let binary = tauri_conf()["mainBinaryName"].as_str().unwrap().to_owned();
    assert_eq!(format!("{binary}.exe"), APP_EXE);
    // `wer_win.rs` proves against the real registry that this key is where the list lives.
    let wer =
        std::fs::read_to_string(crate::repo_root().join("apps/desktop/src-tauri/src/wer_win.rs"))
            .unwrap();
    assert!(wer.contains(registry::WER_EXCLUDED_KEY));
    // The text of a real NSIS script is read in the system code page without a byte order mark.
    let text = std::fs::read(crate::repo_root().join(NSIS_HOOKS)).unwrap();
    assert!(text.is_ascii());
}

#[test]
fn build_bundle_update_runs_put_their_options_before_the_folder_unquoted() {
    assert_eq!(
        nsis_update_install_args(Path::new(r"C:\a b\install")),
        r"/S /NS /UPDATE /D=C:\a b\install"
    );
    // `_?=` runs the uninstaller in place, so its process ends when it has finished.
    assert_eq!(
        nsis_update_uninstall_args(Path::new(r"C:\a b\install")),
        r"/S /UPDATE _?=C:\a b\install"
    );
}

#[test]
fn build_bundle_refuses_a_binary_importing_a_runtime_library_the_install_lacks() {
    let pe = |imports: &[&str]| Pe {
        machine: machine_of(X64).unwrap(),
        imports: imports.iter().map(|s| (*s).to_owned()).collect(),
    };
    check_binary(
        "x.dll",
        &pe(&["MSVCP140.dll", "VCRUNTIME140_1.dll", "KERNEL32.dll"]),
        X64,
        &[],
    )
    .unwrap();
    for missing in ["MSVCP140_2.dll", "VCOMP140.DLL", "concrt140.dll"] {
        let e = check_binary("x.dll", &pe(&[missing]), X64, &[]).unwrap_err();
        assert!(e.to_string().contains(missing), "{e}");
    }
    // Windows provides these.
    check_binary(
        "x.dll",
        &pe(&["api-ms-win-crt-runtime-l1-1-0.dll", "ucrtbase.dll"]),
        X64,
        &[],
    )
    .unwrap();
}

#[test]
fn build_bundle_vc_runtime_follows_the_target_and_refuses_the_x64_only_helper_on_arm64() {
    assert_eq!(vc_runtime_dlls(X64), VC_RUNTIME_DLLS);
    let arm = vc_runtime_dlls(ARM64_WINDOWS);
    assert_eq!(arm, ["msvcp140.dll", "msvcp140_1.dll", "vcruntime140.dll"]);
    let pe = Pe {
        machine: machine_of(ARM64_WINDOWS).unwrap(),
        imports: vec!["VCRUNTIME140.dll".into(), "MSVCP140.dll".into()],
    };
    check_binary("x.dll", &pe, ARM64_WINDOWS, &[]).unwrap();
    // An ARM64 binary importing the x64-only helper would find no ARM64 copy.
    let x64_helper = Pe {
        imports: vec!["VCRUNTIME140_1.dll".into()],
        ..pe
    };
    assert!(check_binary("x.dll", &x64_helper, ARM64_WINDOWS, &[]).is_err());
    assert_eq!(
        resource_dlls(ARM64_WINDOWS, &[]),
        crate::llama_dlls::REQUIRED
            .iter()
            .copied()
            .chain(arm.iter().copied())
            .collect::<Vec<_>>()
    );
}

#[test]
fn build_bundle_finds_the_newest_runtime_for_the_architecture() {
    let root = tmp("vc-redist");
    let crt = |version: &str, arch: &str| {
        let dir = root.join(version).join(arch).join("Microsoft.VC143.CRT");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    };
    let old = crt("14.9.1", "x64");
    let newest = crt("14.44.35112", "x64");
    let arm = crt("14.9.1", "arm64");
    // Beside the versions in a real folder: not a version, and a debug folder under one.
    crt("v143", "x64");
    std::fs::create_dir_all(root.join("14.50.1").join("x64").join("Other")).unwrap();
    assert_eq!(newest_vc_runtime_folder(&root, "x64"), Some(newest));
    assert_eq!(newest_vc_runtime_folder(&root, "arm64"), Some(arm));
    assert_eq!(newest_vc_runtime_folder(&root, "riscv"), None);
    assert_eq!(newest_vc_runtime_folder(&root.join("missing"), "x64"), None);
    assert_ne!(old, root);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn build_bundle_cli_runs_with_no_binary_patching_and_no_signing() {
    let args = cli_args(None, &["nsis"], Path::new(r"C:\t\c.json"));
    let text = args.join(" ");
    assert!(
        text.starts_with("--filter @earshot/desktop exec tauri bundle --bundles nsis --"),
        "{text}"
    );
    assert!(text.contains(r"--config C:\t\c.json"), "{text}");
    // The exe in the installer is byte for byte the one `build::app` built.
    assert!(args.iter().any(|a| a == "--no-binary-patching"), "{text}");
    assert!(args.iter().any(|a| a == "--no-sign"), "{text}");
    assert!(!args.iter().any(|a| a == "--target"), "{text}");
    let arm = cli_args(Some(ARM64_WINDOWS), &["nsis"], Path::new("c"));
    assert!(arm.windows(2).any(|w| w == ["--target", ARM64_WINDOWS]));
}

#[test]
fn build_bundle_child_has_no_route_to_the_network() {
    let vars = no_network_env();
    for name in ["ALL_PROXY", "HTTPS_PROXY", "HTTP_PROXY"] {
        assert!(
            vars.iter()
                .any(|(k, v)| *k == name && v.contains("127.0.0.1")),
            "{name}"
        );
    }
    assert!(NO_PROXY_VARS.contains(&"NO_PROXY") && NO_PROXY_VARS.contains(&"no_proxy"));
}

#[test]
fn build_bundle_output_that_mentions_a_download_fails() {
    assert!(bundler_downloaded(
        "    Downloading https://github.com/x/y.zip\n"
    ));
    assert!(bundler_downloaded("info Verifying NSIS package\n"));
    assert!(bundler_downloaded("Verifying wix package"));
    assert!(bundler_downloaded(
        "NSIS directory is missing some files. Recreating it."
    ));
    assert!(!bundler_downloaded(
        "    Finished 2 bundles at:\n  C:\\x\\Earshot_0.1.0_x64-setup.exe\n"
    ));
}

#[test]
fn build_bundle_tools_are_pinned_with_licences_and_the_cli_version_matches() {
    let root = crate::repo_root();
    let m = crate::manifest::Manifest::load(&root.join(TOOLS_MANIFEST)).unwrap();
    assert_eq!(m.hosts, ["github.com", "sdk.lunarg.com"]);
    let reviewed = [
        ("nsis", "Zlib AND bzip2-1.0.6 AND CPL-1.0"),
        ("nsis-tauri-utils", "Apache-2.0 OR MIT"),
        ("vulkan-runtime", "MIT AND Apache-2.0"),
    ];
    assert_eq!(m.artefacts.len(), reviewed.len());
    for (id, licence) in reviewed {
        let a = m.artefacts.iter().find(|a| a.id == id).unwrap();
        assert_eq!(a.licence, licence, "{id}");
        assert_eq!(a.location, Location::Cache, "{id}");
        // The bundler checks its own tools; the Vulkan runtime is ours to ship, not its.
        assert_eq!(
            a.publisher_checksum.is_some(),
            id != "vulkan-runtime",
            "{id}"
        );
    }
    // A CLI upgrade brings another bundler with other tools: it must come here first.
    let package: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("apps/desktop/package.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(package["devDependencies"]["@tauri-apps/cli"], CLI_VERSION);
    // The shipped manifest stays free of build tools.
    let shipped = std::fs::read_to_string(root.join("models/manifest.toml")).unwrap();
    assert!(!shipped.contains("wix") && !shipped.contains("nsis"));
}

#[test]
fn build_bundle_placed_tools_must_hold_what_the_bundler_checks() {
    let tools = tmp("required");
    assert!(missing_tools(&tools).contains(&format!("NSIS/{}", NSIS_REQUIRED[0])));
    for f in NSIS_REQUIRED {
        write(&tools.join("NSIS").join(f), "x");
    }
    assert!(missing_tools(&tools).is_empty());
    std::fs::remove_file(tools.join("NSIS").join("makensis.exe")).unwrap();
    assert_eq!(missing_tools(&tools), ["NSIS/makensis.exe"]);
    let _ = std::fs::remove_dir_all(&tools);
}

#[test]
fn build_bundle_sync_makes_the_folder_equal_to_its_source() {
    let root = tmp("sync");
    let (src, dst) = (root.join("src"), root.join("dst"));
    write(&src.join("Bin/makensis.exe"), "new");
    write(&src.join("Include/MUI2.nsh"), "same");
    write(&dst.join("Include/MUI2.nsh"), "same");
    write(&dst.join("Bin/makensis.exe"), "old");
    write(&dst.join("Plugins/planted.dll"), "not from the archive");
    sync_tree(&src, &dst, &[]).unwrap();
    assert_eq!(
        std::fs::read_to_string(dst.join("Bin/makensis.exe")).unwrap(),
        "new"
    );
    assert_eq!(
        std::fs::read_to_string(dst.join("Include/MUI2.nsh")).unwrap(),
        "same"
    );
    assert!(!dst.join("Plugins/planted.dll").exists());
    assert!(!dst.join("Plugins").exists(), "an emptied folder goes too");
    // A file added beside the archive's own (the plugin) is kept in place.
    let extra = root.join("plugin.dll");
    write(&extra, "plugin");
    let at = PathBuf::from("Plugins/x86-unicode/additional/plugin.dll");
    sync_tree(&src, &dst, &[(extra.clone(), at.clone())]).unwrap();
    assert_eq!(std::fs::read_to_string(dst.join(&at)).unwrap(), "plugin");
    sync_tree(&src, &dst, &[(extra, at.clone())]).unwrap();
    assert!(dst.join(&at).is_file());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn build_bundle_pe_reader_sees_machine_and_imports_of_a_real_exe() {
    let me = std::env::current_exe().unwrap();
    let pe = read_pe(&me).unwrap();
    assert_eq!(pe.machine, machine_of(effective_target(None)).unwrap());
    assert!(
        pe.imports
            .iter()
            .any(|d| d.eq_ignore_ascii_case("kernel32.dll")),
        "{:?}",
        pe.imports
    );
    assert_eq!(machine_of(X64).unwrap(), 0x8664);
    assert_eq!(machine_of(ARM64_WINDOWS).unwrap(), 0xaa64);
}

#[test]
fn build_bundle_pe_reader_refuses_a_file_that_is_not_a_pe() {
    let root = tmp("notpe");
    for (name, bytes) in [
        ("empty.dll", &b""[..]),
        (
            "text.dll",
            &b"just text, long enough to pass the first look at it"[..],
        ),
        ("mz.dll", &b"MZ\0\0\xff\xff\xff\xff"[..]),
    ] {
        let p = root.join(name);
        std::fs::write(&p, bytes).unwrap();
        assert!(read_pe(&p).is_err(), "{name}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn build_bundle_refuses_a_binary_for_the_wrong_machine_or_with_a_vendor_import() {
    let good = Pe {
        machine: 0x8664,
        imports: vec!["KERNEL32.dll".into(), "llama.dll".into()],
    };
    check_binary("a.dll", &good, X64, &[]).unwrap();
    let e = check_binary("a.dll", &good, ARM64_WINDOWS, &[]).unwrap_err();
    assert!(e.to_string().contains("a.dll"), "{e}");
    let vendor = Pe {
        machine: 0x8664,
        imports: vec!["cublas64_13.dll".into()],
    };
    // Even in a CUDA build: the exe must start with no NVIDIA library present (T8.4a).
    assert!(check_binary("x.exe", &vendor, X64, &["cuda".to_owned()]).is_err());
    let backend = Pe {
        machine: 0x8664,
        imports: vec!["ggml-cuda.dll".into()],
    };
    assert!(check_binary("ggml.dll", &backend, X64, &[]).is_err());
    check_binary("ggml.dll", &backend, X64, &["cuda".to_owned()]).unwrap();
}

#[test]
fn build_bundle_finds_every_file_of_an_install() {
    let root = tmp("install");
    write(&root.join("earshot.exe"), "exe");
    write(&root.join("llama.dll"), "dll");
    write(&root.join("resources/benchmark/NOTICE.md"), "n");
    assert_eq!(files_under(&root).unwrap().len(), 3);
    assert!(files_under(&root.join("missing")).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn build_bundle_manufacturer_comes_from_the_config_as_the_bundler_reads_it() {
    let root = crate::repo_root();
    let conf: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("apps/desktop/src-tauri/tauri.conf.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manufacturer(&conf).unwrap(), "Earshot");
    let with = |v: serde_json::Value| manufacturer(&v);
    assert_eq!(
        with(serde_json::json!({"identifier": "a.b.c", "bundle": {"publisher": "Pub Co"}}))
            .unwrap(),
        "Pub Co"
    );
    assert_eq!(
        with(serde_json::json!({"identifier": "a.b.c"})).unwrap(),
        "b"
    );
    assert_eq!(
        with(serde_json::json!({"identifier": "alone"})).unwrap(),
        "alone"
    );
    assert!(with(serde_json::json!({})).is_err());
}

#[test]
fn build_bundle_tool_mirror_settings_never_reach_the_bundler() {
    for name in [
        "TAURI_BUNDLER_TOOLS_GITHUB_MIRROR",
        "TAURI_BUNDLER_TOOLS_GITHUB_MIRROR_TEMPLATE",
        "tauri_bundler_tools_github_mirror",
    ] {
        assert!(is_bundler_override(name), "{name}");
    }
    for name in ["TAURI_SIGNING_PRIVATE_KEY", "CARGO_TARGET_DIR", "PATH"] {
        assert!(!is_bundler_override(name), "{name}");
    }
}

#[test]
fn build_bundle_an_install_holds_exactly_the_expected_files() {
    let rel = |parts: &[&str]| parts.iter().collect::<PathBuf>();
    let nsis = expected_files(X64);
    // The Vulkan backend, its loader and the loader's licence, where x64 ships with them.
    let vulkan = if cfg!(target_arch = "x86_64") { 3 } else { 0 };
    assert_eq!(
        nsis.len(),
        crate::llama_dlls::REQUIRED.len() + VC_RUNTIME_DLLS.len() + 4 + vulkan
    );
    for dll in VC_RUNTIME_DLLS {
        assert!(nsis.contains(&rel(&[dll])), "{dll}");
    }
    for dll in vc_runtime_dlls(ARM64_WINDOWS) {
        assert!(
            expected_files(ARM64_WINDOWS)
                .iter()
                .any(|f| f.ends_with(dll)),
            "{dll} on ARM64"
        );
    }
    // x64-only: Visual Studio's ARM64 CRT folder holds an x64 copy, which no ARM64 binary imports.
    assert!(
        !expected_files(ARM64_WINDOWS)
            .iter()
            .any(|f| f.ends_with("vcruntime140_1.dll"))
    );
    assert!(nsis.contains(&rel(&["earshot.pdb"])));
    assert_eq!(
        nsis.contains(&rel(&["vulkan-1.dll"])),
        cfg!(target_arch = "x86_64")
    );
    assert_eq!(
        nsis.contains(&rel(&["VulkanRT-License.txt"])),
        cfg!(target_arch = "x86_64")
    );
    // ARM64 stays without them.
    assert!(
        !expected_files(ARM64_WINDOWS)
            .iter()
            .any(|f| f.ends_with("vulkan-1.dll"))
    );
    assert!(nsis.contains(&rel(&["earshot.exe"])));
    assert!(nsis.contains(&rel(&["llama.dll"])));
    assert!(nsis.contains(&rel(&["uninstall.exe"])));
    assert!(nsis.contains(&rel(&["resources", "benchmark", "NOTICE.md"])));

    assert_eq!(file_set_differences(&nsis, &nsis), (vec![], vec![]));
    // Case does not count, as on Windows.
    let shouted: Vec<PathBuf> = nsis
        .iter()
        .map(|p| PathBuf::from(p.to_string_lossy().to_uppercase()))
        .collect();
    assert_eq!(file_set_differences(&shouted, &nsis), (vec![], vec![]));
    for stray in [
        "model.gguf",
        "silero.onnx",
        "onnxruntime.dll",
        "cublas64_13.dll",
    ] {
        let mut found = nsis.clone();
        found.push(PathBuf::from(stray));
        assert_eq!(
            file_set_differences(&found, &nsis),
            (vec![], vec![PathBuf::from(stray)]),
            "{stray}"
        );
    }
    let mut short = nsis.clone();
    let gone = short.pop().unwrap();
    assert_eq!(file_set_differences(&short, &nsis), (vec![gone], vec![]));
}

#[test]
fn build_bundle_silent_install_arguments_put_the_folder_last_unquoted() {
    assert_eq!(
        nsis_install_args(Path::new(r"C:\a b\install")),
        r"/S /NS /D=C:\a b\install"
    );
}

#[test]
fn build_bundle_vulkan_summaries_ship_their_backend_loader_and_licence() {
    let vulkan = vec!["vulkan-summaries".to_owned()];
    let dlls = resource_dlls(X64, &vulkan);
    assert!(dlls.contains(&"ggml-vulkan.dll"));
    assert!(dlls.contains(&VULKAN_LOADER));
    assert_eq!(
        dlls.len(),
        crate::llama_dlls::REQUIRED.len() + VC_RUNTIME_DLLS.len() + 2
    );
    for name in &dlls {
        assert!(!refused_in_install(name, X64, &vulkan), "{name}");
    }
    let hooks = Path::new("h.nsh");
    let conf = bundle_config(
        Path::new("C:/x"),
        Path::new("n.md"),
        hooks,
        &["nsis"],
        X64,
        &vulkan,
    );
    let res = conf["bundle"]["resources"].as_object().unwrap();
    assert!(res.values().any(|v| v == VULKAN_LOADER));
    assert!(res.values().any(|v| v == VULKAN_LICENCE));
    // Without the feature none of it is in the install.
    let plain = bundle_config(
        Path::new("C:/x"),
        Path::new("n.md"),
        hooks,
        &["nsis"],
        X64,
        &[],
    );
    let res = plain["bundle"]["resources"].as_object().unwrap();
    assert!(
        !res.values()
            .any(|v| v == VULKAN_LOADER || v == VULKAN_LICENCE)
    );
}

#[test]
fn build_bundle_an_exe_needing_the_vulkan_loader_needs_it_in_the_install() {
    let pe = Pe {
        machine: machine_of(X64).unwrap(),
        imports: vec!["KERNEL32.dll".to_owned(), "VULKAN-1.DLL".to_owned()],
    };
    assert!(check_binary("x.exe", &pe, X64, &["vulkan-summaries".to_owned()]).is_ok());
    // A whisper-only Vulkan build, or none: the app would not start without a driver's loader.
    let refused = check_binary("x.exe", &pe, X64, &["vulkan".to_owned()]).unwrap_err();
    assert!(refused.to_string().contains("vulkan-1.dll"), "{refused}");
    assert!(check_binary("x.exe", &pe, X64, &[]).is_err());
}

#[test]
fn build_bundle_the_vulkan_runtime_is_pinned_where_the_bundle_reads_it() {
    let root = crate::repo_root();
    let text = std::fs::read_to_string(root.join(TOOLS_MANIFEST)).unwrap();
    assert!(text.contains("into = \"downloads/vulkan-runtime\""));
    assert!(VULKAN_RUNTIME_DIR.starts_with("downloads/vulkan-runtime/"));
    let dir = VULKAN_RUNTIME_DIR
        .strip_prefix("downloads/vulkan-runtime/")
        .unwrap();
    assert!(text.contains(&format!("path = \"{dir}/x64/{VULKAN_LOADER}\"")));
    assert!(text.contains(&format!("path = \"{dir}/{VULKAN_LICENCE}\"")));
}

#[test]
fn bundle_check_refuses_a_target_this_host_cannot_run() {
    // The host's own architecture is checked.
    assert!(refuse_unless_runnable(X64, X64).is_ok());
    assert!(refuse_unless_runnable(ARM64_WINDOWS, ARM64_WINDOWS).is_ok());
    // ARM64 on x64: refused, saying where to run it and how.
    let e = refuse_unless_runnable(ARM64_WINDOWS, X64)
        .unwrap_err()
        .to_string();
    assert!(e.contains("ARM64 Windows machine"), "{e}");
    assert!(e.contains("--installer"), "{e}");
    assert!(e.contains("only the host's own architecture"), "{e}");
    // x64 on ARM64 could run under emulation, but the rule stays simple and says so.
    let e = refuse_unless_runnable(X64, ARM64_WINDOWS)
        .unwrap_err()
        .to_string();
    assert!(e.contains("emulation"), "{e}");
    assert!(e.contains(X64), "{e}");
    // A target with no installer at all.
    assert!(refuse_unless_runnable("x86_64-unknown-linux-gnu", X64).is_err());
    assert!(!e.contains('\u{2014}'), "no em dash");
}

#[test]
fn bundle_check_reads_the_target_and_version_from_an_installer_name() {
    for target in [X64, ARM64_WINDOWS] {
        let path = Path::new(r"D:\copied").join(installer_path("1.2.3", target).unwrap());
        assert_eq!(
            installer_name_target(&path).unwrap(),
            ("1.2.3".to_owned(), target)
        );
    }
    for bad in [
        "Earshot_1.2.3_x86-setup.exe",
        "Earshot_1.2.3_arm64.exe",
        "Other_1.2.3_arm64-setup.exe",
        "Earshot_arm64-setup.exe",
        "Earshot__arm64-setup.exe",
    ] {
        assert!(installer_name_target(Path::new(bad)).is_err(), "{bad}");
    }
}

#[test]
fn bundle_check_takes_the_target_from_the_installed_exe_machine() {
    assert_eq!(target_of_machine(machine_of(X64).unwrap()), Some(X64));
    assert_eq!(
        target_of_machine(machine_of(ARM64_WINDOWS).unwrap()),
        Some(ARM64_WINDOWS)
    );
    assert_eq!(target_of_machine(0x014c), None);
}

#[test]
fn bundle_check_finds_a_named_targets_build_under_its_own_folder() {
    let root = Path::new(r"C:\r");
    let drive = || Some(std::ffi::OsString::from("C:"));
    // The host, unnamed: today's place, unchanged.
    assert_eq!(
        built_release_dirs(root, None, drive(), None, X64),
        [build::short_target_dir(root, None, drive(), None).join("release")]
    );
    // A named target: where cargo puts a `--target` build, with the short folder `build` chose.
    let arm = built_release_dirs(root, None, drive(), Some(ARM64_WINDOWS), X64);
    assert_eq!(
        arm,
        [
            build::short_target_dir(root, None, drive(), Some(ARM64_WINDOWS))
                .join(ARM64_WINDOWS)
                .join("release")
        ]
    );
    // The host named: its `--target` folder first, then a plain build's.
    let explicit = Path::new(r"C:\eb\x");
    assert_eq!(
        built_release_dirs(root, Some(explicit), drive(), Some(X64), X64),
        [explicit.join(X64).join("release"), explicit.join("release")]
    );
}

#[test]
fn bundle_check_without_a_build_says_how_each_comparison_is_replaced() {
    let arm = built_comparisons(ARM64_WINDOWS);
    let how = |name: &str| instead_of_build(name, ARM64_WINDOWS);
    for (name, _) in &arm {
        let pinned = qnn_install_files().iter().any(|(_, to)| to == name);
        if pinned {
            assert_eq!(how(name), Instead::Pin, "{name}");
        }
    }
    assert!(arm.iter().any(|(n, _)| *n == QNN_NOTICE));
    assert_eq!(how(QNN_NOTICE), Instead::Notice);
    assert_eq!(how(APP_EXE), Instead::Binary);
    assert_eq!(how("llama.dll"), Instead::Binary);
    assert_eq!(how("msvcp140.dll"), Instead::Binary);
    assert_eq!(how(APP_SYMBOLS), Instead::Skipped);
    // On x64 nothing is pinned, the notice is not there, and the licence text has no pin.
    assert_eq!(instead_of_build("QnnHtp.dll", X64), Instead::Binary);
    assert_eq!(instead_of_build(VULKAN_LICENCE, X64), Instead::Skipped);
    // The listing names exactly the skipped ones, each with its reason.
    let skipped = skipped_without_build(ARM64_WINDOWS);
    let names: Vec<&str> = skipped.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, [APP_SYMBOLS]);
    let lines = skipped_lines(ARM64_WINDOWS);
    assert!(
        lines
            .iter()
            .any(|l| l.contains(APP_SYMBOLS) && l.contains("no pin")),
        "{lines:?}"
    );
    // Binaries are listed too: their byte comparison is replaced, not dropped silently.
    assert!(
        lines
            .iter()
            .any(|l| l.contains(APP_EXE) && l.contains("machine type")),
        "{lines:?}"
    );
    assert!(lines.iter().all(|l| !l.contains('\u{2014}')));
    let x64: Vec<&str> = skipped_without_build(X64).iter().map(|(n, _)| *n).collect();
    assert!(x64.contains(&APP_SYMBOLS));
}

#[test]
fn bundle_check_prints_the_installer_digest_so_the_signed_file_can_be_matched() {
    let dir = tmp("digest");
    let file = dir.join("Earshot_1.2.3_arm64-setup.exe");
    std::fs::write(&file, b"abc").unwrap();
    let digest = sha256_file(&file).unwrap();
    // SHA-256 of "abc".
    assert_eq!(
        digest,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let start = installer_digest_line(&file, &digest);
    assert!(start.contains(&digest), "{start}");
    assert!(start.contains("Earshot_1.2.3_arm64-setup.exe"), "{start}");
    let pass = installer_summary(true, &file, &digest, 1);
    assert!(pass.contains("PASS") && pass.contains(&digest), "{pass}");
    assert!(pass.contains("1 comparison"), "{pass}");
    let fail = installer_summary(false, &file, &digest, 0);
    assert!(fail.contains("FAIL") && fail.contains(&digest), "{fail}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bundle_check_qnn_pins_cover_every_shipped_qnn_file() {
    let pins = qnn_pins(&crate::repo_root()).unwrap();
    let names: Vec<&str> = pins.iter().map(|p| p.name).collect();
    let wanted: Vec<&str> = qnn_install_files().iter().map(|(_, to)| *to).collect();
    assert_eq!(names, wanted);
    assert!(pins.iter().all(|p| p.sha256.len() == 64 && p.size > 0));
}

#[test]
fn bundle_check_judges_qnn_files_and_the_notice_against_pins() {
    let dir = tmp("qnn-pins");
    let body = b"stub library";
    let digest = {
        let p = dir.join("probe");
        std::fs::write(&p, body).unwrap();
        sha256_file(&p).unwrap()
    };
    let pins = vec![
        QnnPin {
            name: "QnnHtp.dll",
            sha256: digest.clone(),
            size: body.len() as u64,
        },
        QnnPin {
            name: "QnnSystem.dll",
            sha256: digest,
            size: body.len() as u64,
        },
    ];
    std::fs::write(dir.join("QnnHtp.dll"), body).unwrap();
    std::fs::write(dir.join("QnnSystem.dll"), b"something else").unwrap();
    let results = qnn_pin_results(&dir, &pins);
    assert_eq!(results.len(), 2);
    assert!(results[0].1.is_ok(), "{:?}", results[0].1);
    assert!(results[1].1.is_err(), "a changed file fails its pin");
    // A missing file fails too.
    std::fs::remove_file(dir.join("QnnHtp.dll")).unwrap();
    assert!(qnn_pin_results(&dir, &pins)[0].1.is_err());

    // The notice names every QNN file: the bundle's own text does, one missing a name does not.
    assert!(notice_missing_names(&qnn_notice_text()).is_empty());
    let cut = qnn_notice_text().replace("QnnHtpPrepare.dll", "");
    assert_eq!(notice_missing_names(&cut), ["QnnHtpPrepare.dll"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T12.15ee: the install's notice credits the common words compiled into the exe, from the core
/// `.NOTICE` beside the list, after the benchmark meeting's own notice.
#[test]
fn build_bundle_install_notice_credits_the_common_words() {
    let root = crate::repo_root();
    let text = install_notice_text(&root).unwrap();
    let meeting = std::fs::read_to_string(root.join(MEETING_NOTICE)).unwrap();
    assert!(
        text.starts_with(&meeting),
        "the meeting's notice comes first, unchanged"
    );
    let core = std::fs::read_to_string(
        root.join(crate::common_words::OUT_DIR)
            .join(crate::common_words::NOTICE_FILE),
    )
    .unwrap();
    // One source: the source, licence and changes are the core NOTICE's own lines.
    for field in ["Source:", "Licence:", "Changes:"] {
        let line = core
            .lines()
            .find_map(|l| l.strip_prefix(field))
            .unwrap()
            .trim();
        assert!(text.contains(line), "{field} {line}");
    }
    assert!(text.contains("5,000 most frequent words"), "{text}");
    assert!(notice_missing_credits(&text).is_empty(), "{text}");
    assert!(!text.contains('\u{2014}'), "no em dash");
    assert!(text.is_ascii());
}

/// T12.15ee: the notice the bundle run writes beside the exe is this checkout's assembled text,
/// and it is the file the overlay installs as the meeting's notice.
#[test]
fn build_bundle_install_notice_is_written_beside_the_exe_and_installed_by_the_overlay() {
    let root = crate::repo_root();
    let exe_dir = tmp("notice");
    let notice = write_install_notice(&root, &exe_dir).unwrap();
    assert_eq!(notice, exe_dir.join(INSTALL_NOTICE_BUILT));
    let text = std::fs::read_to_string(&notice).unwrap();
    assert_eq!(text, install_notice_text(&root).unwrap());
    assert!(notice_missing_credits(&text).is_empty());
    let c = bundle_config(&exe_dir, &notice, Path::new("h.nsh"), &["nsis"], X64, &[]);
    assert_eq!(
        c["bundle"]["resources"][notice.display().to_string()],
        MEETING_NOTICE_IN_INSTALL
    );
    let _ = std::fs::remove_dir_all(&exe_dir);
}

#[test]
fn bundle_check_refuses_a_notice_without_the_common_words() {
    let root = crate::repo_root();
    let text = install_notice_text(&root).unwrap();
    let meeting = std::fs::read_to_string(root.join(MEETING_NOTICE)).unwrap();
    // The meeting's notice alone, as installs before T12.15ee held it, fails.
    let missing = notice_missing_credits(&meeting);
    assert!(missing.contains(&COMMON_WORDS_HEADING), "{missing:?}");
    assert!(!notice_check(&meeting, &text).0);
    // So does a section with its licence cut, and an empty or missing file.
    let cut = text.replace(COMMON_WORDS_LICENCE_URL, "");
    assert_eq!(notice_missing_credits(&cut), [COMMON_WORDS_LICENCE_URL]);
    assert!(!notice_check(&cut, &text).0);
    assert!(!notice_check("", &text).0);
    // The bundle's own text passes.
    let (pass, detail) = notice_check(&text, &text);
    assert!(pass, "{detail}");
    // A notice that differs from what this checkout assembles fails even with every credit.
    assert!(!notice_check(&format!("{text}\nmore"), &text).0);
}

#[test]
fn bundle_check_the_common_words_credit_needs_every_field_of_the_core_notice() {
    let core = "Source:   S\nLicence: L\nChanges: C\n";
    let credit = common_words_credit(core).unwrap();
    assert!(credit.contains("- Source: S\n"), "{credit}");
    assert!(credit.contains("- Licence: L\n"), "{credit}");
    assert!(credit.contains("- Changes: C\n"), "{credit}");
    for drop in ["Source:", "Licence:", "Changes:"] {
        let without: String = core
            .lines()
            .filter(|l| !l.starts_with(drop))
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(common_words_credit(&without).is_err(), "{drop}");
    }
}
