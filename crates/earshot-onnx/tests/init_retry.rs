//! A library that passes its pin and still will not load leaves the process free to load another:
//! what `host::init_onnx_for`'s retry on the CPU library needs. Its own binary, because ONNX
//! Runtime is one library per process. The bad library is a file of invented bytes pinned to its
//! own digest.

use sha2::{Digest, Sha256};

mod common;

#[test]
fn vad_runtime_failed_library_does_not_stop_the_next_one_loading() {
    let dir = std::env::temp_dir().join(format!("earshot-onnx-retry-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("onnxruntime.dll");
    let bytes = b"these bytes are no library";
    std::fs::write(&bad, bytes).unwrap();
    let pin: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let err = earshot_onnx::init(&bad, &pin).map(|_| ()).unwrap_err();
    assert!(matches!(err, earshot_onnx::OnnxError::Library(_)), "{err}");
    assert!(earshot_onnx::loaded_library().is_none());
    assert_eq!(
        earshot_onnx::environment().map(|_| ()),
        Err(earshot_onnx::OnnxError::NotInitialised)
    );

    common::init_runtime();
    assert!(earshot_onnx::loaded_library().is_some());
    assert!(earshot_onnx::environment().is_ok());

    // A success is still the one runtime of the process.
    let again = earshot_onnx::init(&bad, &pin).map(|_| ()).unwrap_err();
    assert!(again.to_string().contains("already loaded"), "{again}");
    std::fs::remove_dir_all(&dir).unwrap();
}
