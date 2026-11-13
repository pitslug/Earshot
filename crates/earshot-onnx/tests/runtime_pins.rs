//! The ONNX Runtime library pins in `earshot_onnx` equal the member pins of
//! `models/manifest.toml`, and a loaded library is identified by its canonical path.

mod common;

use earshot_onnx::qnn::{QNN_ARM64, QNN_HTP_LIB, QNN_PROVIDER_LIB, QNN_X64};
use earshot_onnx::{
    ORT_CPU_DYLIB, ORT_CPU_DYLIB_ARM64, ORT_CPU_DYLIB_ARM64_SHA256, ORT_CPU_DYLIB_SHA256,
    ORT_CPU_DYLIB_X64, ORT_CPU_DYLIB_X64_SHA256, ORT_CUDA_DYLIB, ORT_CUDA_DYLIB_SHA256,
};

fn member_sha(manifest: &toml::Value, rel: &str) -> String {
    for a in manifest["artefact"].as_array().unwrap() {
        let Some(x) = a.get("extract") else { continue };
        let into = x["into"].as_str().unwrap();
        for m in x
            .get("member")
            .and_then(|m| m.as_array())
            .into_iter()
            .flatten()
        {
            if format!("{into}/{}", m["path"].as_str().unwrap()) == rel {
                return m["sha256"].as_str().unwrap().to_owned();
            }
        }
    }
    panic!("{rel} is not a pinned member of models/manifest.toml");
}

#[test]
fn runtime_dylib_pins_match_the_manifest() {
    let text = std::fs::read_to_string(common::repo_root().join("models/manifest.toml")).unwrap();
    let manifest: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(member_sha(&manifest, ORT_CPU_DYLIB), ORT_CPU_DYLIB_SHA256);
    assert_eq!(
        member_sha(&manifest, ORT_CPU_DYLIB_X64),
        ORT_CPU_DYLIB_X64_SHA256
    );
    assert_eq!(
        member_sha(&manifest, ORT_CPU_DYLIB_ARM64),
        ORT_CPU_DYLIB_ARM64_SHA256
    );
    assert_eq!(member_sha(&manifest, ORT_CUDA_DYLIB), ORT_CUDA_DYLIB_SHA256);
}

#[test]
fn runtime_qnn_plugin_pins_match_the_manifest() {
    let text = std::fs::read_to_string(common::repo_root().join("models/manifest.toml")).unwrap();
    let manifest: toml::Value = toml::from_str(&text).unwrap();
    for pkg in [QNN_ARM64, QNN_X64] {
        assert_eq!(
            member_sha(&manifest, &format!("{}/{QNN_PROVIDER_LIB}", pkg.dir)),
            pkg.provider_sha256
        );
        assert_eq!(
            member_sha(&manifest, &format!("{}/{QNN_HTP_LIB}", pkg.dir)),
            pkg.htp_sha256
        );
        for (name, sha) in pkg.libraries {
            assert_eq!(
                member_sha(&manifest, &format!("{}/{name}", pkg.dir)),
                *sha,
                "{name}"
            );
        }
        // Every library the manifest pins for the package is checked before loading (T8.4e), so
        // nothing unpinned sits beside the plugin unchecked.
        let mut pinned: Vec<String> = pkg.libraries.iter().map(|(n, _)| (*n).to_owned()).collect();
        pinned.extend([QNN_PROVIDER_LIB.to_owned(), QNN_HTP_LIB.to_owned()]);
        pinned.sort();
        assert_eq!(member_libraries(&manifest, pkg.dir), pinned, "{}", pkg.dir);
    }
}

/// The `.dll`, `.so` and `.cat` members the manifest pins under the extraction folder `into`, sorted.
fn member_libraries(manifest: &toml::Value, into: &str) -> Vec<String> {
    let mut out = Vec::new();
    for a in manifest["artefact"].as_array().unwrap() {
        let Some(x) = a.get("extract") else { continue };
        if x["into"].as_str() != Some(into) {
            continue;
        }
        for m in x
            .get("member")
            .and_then(|m| m.as_array())
            .into_iter()
            .flatten()
        {
            let path = m["path"].as_str().unwrap();
            if path.ends_with(".dll") || path.ends_with(".so") || path.ends_with(".cat") {
                out.push(path.to_owned());
            }
        }
    }
    out.sort();
    out
}

#[test]
fn runtime_init_is_idempotent_for_the_same_canonical_path() {
    common::init_runtime();
    let first = earshot_onnx::loaded_library().unwrap().to_path_buf();
    // The same file through a non-canonical spelling is the same library.
    let dir = first.parent().unwrap();
    let detour = dir.join("..").join("lib").join(first.file_name().unwrap());
    let sha = if cfg!(feature = "cuda") {
        ORT_CUDA_DYLIB_SHA256
    } else {
        ORT_CPU_DYLIB_SHA256
    };
    earshot_onnx::init(&detour, sha).expect("same library");
    assert_eq!(earshot_onnx::loaded_library().unwrap(), first);
}
