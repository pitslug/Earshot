//! T8.4af: the choice was made before the meeting read its order, so the meeting runs on it, with
//! the start's hold still on and no wait. Its own binary, because the choice is one per process.
//! The other order is `meeting_queue_cuda.rs`. Needs the provider's feature:
//! `cargo test -p earshot-runtime --features onnx-cuda --test cuda_choice_first`

#![cfg(all(feature = "onnx-cuda", not(feature = "cuda")))]

use std::sync::mpsc;
use std::time::Duration;

use earshot_runtime::backends::{Accelerator, ModelDir};
use earshot_runtime::catalogue::Catalogue;
use earshot_runtime::cuda::{
    CudaPart, folder, hold_onnx, onnx_held, transcription_from_folder, use_for_transcription_in,
};
use earshot_runtime::host::settled_order;

fn bytes_of(name: &str) -> Vec<u8> {
    format!("the bytes of {name}").into_bytes()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn test_manifest() -> Catalogue {
    let mut text = "sets = [\"s\"]\nhosts = [\"example.org\"]\n\n".to_owned();
    for (i, (id, part)) in [("a", "summaries"), ("b", "transcription")]
        .into_iter()
        .enumerate()
    {
        text.push_str(&format!(
            "[[artefact]]\nid = \"{id}\"\nsets = [\"s\"]\nlocation = \"cache\"\n\
             url = \"https://example.org/{id}\"\npath = \"archives/{id}.zip\"\n\
             sha256 = \"{}\"\nsize = 1\nlicence = \"x\"\nattribution = \"x\"\n\n",
            "0".repeat(64)
        ));
        text.push_str(&format!(
            "[[cuda_archive]]\npart = \"{part}\"\nartefact = \"{id}\"\nformat = \"zip\"\n\n"
        ));
        for name in CudaPart::ALL[i].required_files() {
            let bytes = bytes_of(name);
            text.push_str(&format!(
                "[[cuda_archive.file]]\nname = \"{name}\"\nmember = \"bin/{name}\"\n\
                 sha256 = \"{}\"\nsize = {}\n\n",
                sha256_hex(&bytes),
                bytes.len()
            ));
        }
    }
    Catalogue::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

#[test]
fn cuda_choice_first_a_later_meeting_runs_on_it_with_no_wait_and_no_note() {
    let root = std::env::temp_dir().join(format!("earshot-choice-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let models = ModelDir::new(&root);
    let dir = folder(&models);
    std::fs::create_dir_all(&dir).unwrap();
    for part in CudaPart::ALL {
        for name in part.required_files() {
            std::fs::write(dir.join(name), bytes_of(name)).unwrap();
        }
    }
    let hold = hold_onnx();
    assert!(onnx_held());
    use_for_transcription_in(&test_manifest(), &models, true).unwrap();
    assert!(transcription_from_folder());
    assert!(!onnx_held(), "decided: nothing left to wait for");

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || tx.send(settled_order()));
    let order = rx.recv_timeout(Duration::from_secs(60)).unwrap();
    assert_eq!(order.first(), Some(&Accelerator::Cuda));
    assert!(transcription_from_folder());
    drop(hold);
    std::fs::remove_dir_all(&root).unwrap();
}
