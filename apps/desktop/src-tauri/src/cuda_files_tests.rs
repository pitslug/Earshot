//! The real `CudaFiles` over a temp models folder. Only the ignored test touches the network.

use std::path::PathBuf;

use earshot_core::identity::ManualClock;

use super::*;
use crate::testutil::TempDir;

fn files() -> Arc<ManifestCudaFiles> {
    ManifestCudaFiles::new(Arc::new(ManualClock::new(0)))
}

/// Text a window may show: no file name, path or address.
fn assert_plain(text: &str) {
    for word in [".dll", ".zip", "http", "\\", "cuda/"] {
        assert!(!text.contains(word), "{word} in {text}");
    }
}

fn message_of(e: &CudaFilesError) -> &str {
    match e {
        CudaFilesError::Failed { message, .. } => message,
        CudaFilesError::Cancelled => panic!("cancelled"),
    }
}

#[test]
fn cuda_files_say_what_the_manifest_pins_and_what_is_held() {
    let dir = TempDir::new("cuda-files");
    let models = ModelDir::new(dir.0.join("models"));
    let c = Catalogue::embedded().unwrap();
    let files = files();
    for part in CudaPart::ALL {
        let f = files.part_files(&models, part);
        assert_eq!(f.not_available, None, "both parts are pinned");
        assert_eq!(f.state, PartState::Missing);
        assert_eq!(f.held, 0);
        assert_eq!(f.download_size, cuda::download_size(c, part).unwrap());
        assert_eq!(f.disk_size, cuda::disk_size(c, part));
    }
    // A file with its pinned size counts, by size alone (the page polls this).
    let first = &cuda::part_files(c, CudaPart::Summaries)[0];
    let folder = cuda::folder(&models);
    std::fs::create_dir_all(&folder).unwrap();
    let file = std::fs::File::create(folder.join(&first.name)).unwrap();
    file.set_len(first.size).unwrap();
    assert_eq!(
        files.part_files(&models, CudaPart::Summaries).state,
        PartState::Partial
    );
}

#[test]
fn cuda_files_verify_refuses_in_plain_words() {
    let dir = TempDir::new("cuda-files");
    let models = ModelDir::new(dir.0.join("models"));
    let c = Catalogue::embedded().unwrap();
    let files = files();
    let e = files.verify(&models, CudaPart::Summaries).unwrap_err();
    assert_plain(message_of(&e));
    // Every file there with its right size and the wrong bytes.
    let folder = cuda::folder(&models);
    std::fs::create_dir_all(&folder).unwrap();
    for f in cuda::part_files(c, CudaPart::Summaries) {
        let file = std::fs::File::create(folder.join(&f.name)).unwrap();
        file.set_len(f.size).unwrap();
    }
    assert_eq!(
        files.part_files(&models, CudaPart::Summaries).state,
        PartState::Installed
    );
    let e = files.verify(&models, CudaPart::Summaries).unwrap_err();
    assert_eq!(
        e,
        CudaFilesError::Failed {
            message: FAILED_CHECK.to_owned(),
            retryable: false
        }
    );
}

#[test]
fn cuda_files_remove_frees_the_part_and_names_only_what_is_not_ours() {
    let dir = TempDir::new("cuda-files");
    let models = ModelDir::new(dir.0.join("models"));
    let c = Catalogue::embedded().unwrap();
    let folder = cuda::folder(&models);
    std::fs::create_dir_all(&folder).unwrap();
    let f = &cuda::part_files(c, CudaPart::Transcription)[0];
    std::fs::write(folder.join(&f.name), b"part of it").unwrap();
    std::fs::write(folder.join("the-users-own.txt"), b"not ours").unwrap();
    let removed = files().remove(&models, CudaPart::Transcription).unwrap();
    assert_eq!(removed.bytes_freed, 10);
    assert_eq!(removed.left_in_folder, ["the-users-own.txt"]);
    assert!(folder.join("the-users-own.txt").is_file());
}

/// A file in use names no file, and a retry can work; what depends on another part cannot.
#[test]
fn cuda_files_remove_errors_say_what_a_retry_can_do() {
    let in_use = remove_failure(&CudaError::InUse {
        name: "ggml.dll".into(),
        error: "held at C:\\models\\cuda\\ggml.dll".into(),
    });
    assert_eq!(in_use, failed(IN_USE, true));
    assert_plain(message_of(&in_use));
    let needed = remove_failure(&CudaError::NeededBy {
        part: CudaPart::Summaries,
        by: CudaPart::Transcription,
    });
    assert_eq!(
        needed,
        failed(RemovalRefused::NeededByTranscription.copy(), false)
    );
    let stopped = remove_failure(&CudaError::Remove {
        path: PathBuf::from("C:\\models\\cuda\\ggml.dll"),
        error: "denied".into(),
    });
    assert_eq!(stopped, failed(REMOVE_FAILED, true));
    assert_plain(message_of(&stopped));
}

/// A part held open by this process: the real removal refuses with nothing removed.
#[test]
fn cuda_files_a_held_file_is_refused_with_nothing_removed() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = TempDir::new("cuda-files");
    let models = ModelDir::new(dir.0.join("models"));
    let c = Catalogue::embedded().unwrap();
    let folder = cuda::folder(&models);
    std::fs::create_dir_all(&folder).unwrap();
    let pinned = cuda::part_files(c, CudaPart::Transcription);
    for f in &pinned[..2] {
        std::fs::write(folder.join(&f.name), b"x").unwrap();
    }
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(folder.join(&pinned[1].name))
        .unwrap();
    let e = files()
        .remove(&models, CudaPart::Transcription)
        .unwrap_err();
    assert_eq!(e, failed(IN_USE, true));
    assert!(folder.join(&pinned[0].name).is_file());
    drop(held);
}

/// The real download of both parts through this implementation, into the folder
/// `EARSHOT_CUDA_TEST_MODELS` names (never the user's models folder), checked by this
/// implementation's own `verify`. About 2 GB.
#[test]
#[ignore = "downloads about 2 GB from NVIDIA and GitHub: set EARSHOT_CUDA_TEST_MODELS to a scratch folder and run cargo test -p earshot-desktop cuda_files_real_download -- --ignored --nocapture"]
fn cuda_files_real_download_of_both_parts() {
    let root = std::env::var_os("EARSHOT_CUDA_TEST_MODELS").expect("EARSHOT_CUDA_TEST_MODELS");
    let models = ModelDir::new(root);
    let files = ManifestCudaFiles::new(Arc::new(earshot_runtime::SystemClock));
    for part in CudaPart::ALL {
        let started = std::time::Instant::now();
        let mut last = (0, 0);
        files
            .download(&models, part, &CancelToken::new(), &mut |done, total| {
                last = (done, total);
            })
            .unwrap_or_else(|e| panic!("{part:?}: {e:?}"));
        println!(
            "{part:?}: downloaded {last:?} in {:?}; state {:?}",
            started.elapsed(),
            files.part_files(&models, part).state
        );
        assert_eq!(last.0, last.1);
        let started = std::time::Instant::now();
        files.verify(&models, part).unwrap();
        println!("{part:?}: verified every byte in {:?}", started.elapsed());
        assert_eq!(
            files.part_files(&models, part).state,
            PartState::Installed,
            "{part:?}"
        );
    }
}
