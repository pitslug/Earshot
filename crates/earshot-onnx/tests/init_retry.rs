//! A library that passes its pin and still will not load leaves the process free to load another:
//! what `host::init_onnx_for`'s retry on the CPU library needs. Its own binary, because ONNX
//! Runtime is one library per process. The first bad library is a file of invented bytes pinned
//! to its own digest (no load at all); the second is ONNX Runtime's own
//! `onnxruntime_providers_shared.dll` from the CPU set, a real library with no `OrtGetApiBase`,
//! so ort's load succeeds and only its check refuses it. Since T12.47 (ort 2.0.0-rc.15) both go
//! straight to `ort::init_from` with no probe first, so this is the check that ort's library
//! slot is free again after either failure.

use sha2::{Digest, Sha256};

mod common;

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn assert_nothing_loaded() {
    assert!(earshot_onnx::loaded_library().is_none());
    assert_eq!(
        earshot_onnx::environment().map(|_| ()),
        Err(earshot_onnx::OnnxError::NotInitialised)
    );
}

#[test]
fn vad_runtime_failed_library_does_not_stop_the_next_one_loading() {
    let dir = std::env::temp_dir().join(format!("earshot-onnx-retry-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("onnxruntime.dll");
    let bytes = b"these bytes are no library";
    std::fs::write(&bad, bytes).unwrap();
    let pin = digest(bytes);

    let err = earshot_onnx::init(&bad, &pin).map(|_| ()).unwrap_err();
    assert!(matches!(err, earshot_onnx::OnnxError::Library(_)), "{err}");
    assert_nothing_loaded();

    // A real library that loads but is no ONNX Runtime: refused after the load, slot still free.
    let shared = common::fetched(
        &std::path::Path::new(earshot_onnx::ORT_CPU_DYLIB)
            .with_file_name("onnxruntime_providers_shared.dll")
            .to_string_lossy(),
        earshot_onnx::ORT_CPU_SET,
    );
    let shared_pin = digest(&std::fs::read(&shared).unwrap());
    let err = earshot_onnx::init(&shared, &shared_pin)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(err, earshot_onnx::OnnxError::Library(_)), "{err}");
    assert!(err.to_string().contains("OrtGetApiBase"), "{err}");
    assert_nothing_loaded();

    common::init_runtime();
    assert!(earshot_onnx::loaded_library().is_some());
    assert!(earshot_onnx::environment().is_ok());

    // A success is still the one runtime of the process.
    let again = earshot_onnx::init(&bad, &pin).map(|_| ()).unwrap_err();
    assert!(again.to_string().contains("already loaded"), "{again}");
    std::fs::remove_dir_all(&dir).unwrap();
}
