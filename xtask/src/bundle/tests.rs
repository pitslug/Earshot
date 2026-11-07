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
    assert_eq!(resource_dlls(&[]), plain);
    // A GPU build's own llama.cpp backend: code that can use CUDA, not NVIDIA's libraries (D23).
    let cuda = resource_dlls(&["cuda".to_owned()]);
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
    assert!(refused_in_install("ggml-cuda.dll", &[]));
    assert!(!refused_in_install("ggml-cuda.dll", &["cuda".to_owned()]));
    assert!(refused_in_install("cublas64_13.dll", &["cuda".to_owned()]));
    assert!(!refused_in_install("llama.dll", &[]));
}

#[test]
fn build_bundle_config_turns_the_bundler_on_with_no_download_at_install_time() {
    let exe_dir = Path::new(r"C:\eb\x\release");
    let notice = Path::new(r"C:\repo\NOTICE.md");
    let c = bundle_config(exe_dir, notice, &["nsis"], &[]);
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
        assert!(
            expected_files(ARM64_WINDOWS)
                .iter()
                .any(|f| f.ends_with(dll)),
            "{dll} on ARM64"
        );
    }
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
    let dlls = resource_dlls(&vulkan);
    assert!(dlls.contains(&"ggml-vulkan.dll"));
    assert!(dlls.contains(&VULKAN_LOADER));
    assert_eq!(
        dlls.len(),
        crate::llama_dlls::REQUIRED.len() + VC_RUNTIME_DLLS.len() + 2
    );
    for name in &dlls {
        assert!(!refused_in_install(name, &vulkan), "{name}");
    }
    let conf = bundle_config(Path::new("C:/x"), Path::new("n.md"), &["nsis"], &vulkan);
    let res = conf["bundle"]["resources"].as_object().unwrap();
    assert!(res.values().any(|v| v == VULKAN_LOADER));
    assert!(res.values().any(|v| v == VULKAN_LICENCE));
    // Without the feature none of it is in the install.
    let plain = bundle_config(Path::new("C:/x"), Path::new("n.md"), &["nsis"], &[]);
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
