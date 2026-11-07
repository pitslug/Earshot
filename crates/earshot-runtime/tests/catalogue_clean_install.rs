//! T9.1f accept: from an empty models folder, the speech helpers and ONNX Runtime download
//! through `earshot_runtime::download`, verify, read as installed and load, with the real
//! pinned files (the manifest's bytes) served from a loopback server, so no network and no
//! test double of the files. Its own binary because ONNX Runtime is one library per process
//! and this test loads the copy it just downloaded.
//!
//! The bytes come from the models folder, which `cargo xtask fetch-models` fills; a missing
//! file fails with that instruction.

#![cfg(not(feature = "cuda"))]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use earshot_core::asr::{CancelToken, Vad};
use earshot_core::diar::{SpeakerEmbedder, SpeakerSegmenter};
use earshot_llm_replay::tcp;
use earshot_net::{Allowlist, AuditOutcome, Url};
use earshot_runtime::backends::{
    Accelerator, CAMPPLUS, ModelDir, SEGMENTATION, SILERO, init_onnx, load_diarisation, load_silero,
};
use earshot_runtime::catalogue::{Catalogue, HELPERS_MODEL, InstallState};
use earshot_runtime::download::Downloader;
use earshot_runtime::{RuntimeError, SystemClock, host};
use earshot_store::DataDir;

/// The production models folder (`EARSHOT_MODELS`, else `%LOCALAPPDATA%\\Earshot\\models`):
/// where the pinned files are read from.
fn source_models() -> ModelDir {
    let data = DataDir::from_local_app_data()
        .expect("LOCALAPPDATA is not set; set EARSHOT_MODELS to the models directory");
    ModelDir::from_env(&data)
}

/// Serves each file at `/<id>`, whole, one connection at a time; answers a `Range` with a 206.
fn serve(files: BTreeMap<String, PathBuf>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Some((path, from)) = read_request(&stream) else {
                tcp::lingering_close(stream, &tcp::error_response(400, "Bad Request", "unread"));
                continue;
            };
            let Some(file) = files.get(path.trim_start_matches('/')) else {
                tcp::lingering_close(stream, &tcp::error_response(404, "Not Found", "no file"));
                continue;
            };
            let bytes = std::fs::read(file).unwrap();
            let body = &bytes[from.min(bytes.len())..];
            let head = if from > 0 {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\nConnection: close\r\n\r\n",
                    body.len(),
                    bytes.len() - 1,
                    bytes.len()
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
            };
            let mut response = head.into_bytes();
            response.extend_from_slice(body);
            tcp::lingering_close_within(stream, &response, std::time::Duration::from_secs(120));
        }
    });
    port
}

/// The request's path and the start of its `Range`.
fn read_request(stream: &TcpStream) -> Option<(String, usize)> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let path = first.split_whitespace().nth(1)?.to_owned();
    let mut from = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            return Some((path, from));
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("range")
        {
            from = v
                .trim()
                .trim_start_matches("bytes=")
                .trim_end_matches('-')
                .parse()
                .ok()?;
        }
    }
}

fn sum_of_files(dir: &Path) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let kind = e.file_type().unwrap();
        if kind.is_dir() {
            total += sum_of_files(&e.path());
        } else {
            total += e.metadata().unwrap().len();
        }
    }
    total
}

#[test]
fn catalogue_clean_install_helpers_download_verify_and_load_from_an_empty_folder() {
    let catalogue = Catalogue::embedded().expect("the embedded manifest");
    let source = source_models();
    let model = catalogue.model(HELPERS_MODEL).expect("the speech helpers");

    // The pinned bytes: each artefact's file as `cargo xtask fetch-models` left it.
    let mut files = BTreeMap::new();
    for id in &model.artefacts {
        let a = catalogue.artefact(id).unwrap();
        let file = a.file(&source).unwrap();
        assert!(
            file.is_file(),
            "{} is missing; run `cargo xtask fetch-models --set {}`",
            file.display(),
            earshot_onnx::ORT_CPU_SET
        );
        files.insert(id.clone(), file);
    }
    let port = serve(files);

    // Earlier runs' folders go now: the DLL a run loads stays mapped until that run ends. They live
    // in this target directory's scratch folder, so another lane's run is never swept.
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"));
    for old in std::fs::read_dir(scratch).unwrap().flatten() {
        if old
            .file_name()
            .to_string_lossy()
            .starts_with("earshot-clean-install-")
        {
            let _ = std::fs::remove_dir_all(old.path());
        }
    }
    let empty = scratch.join(format!("earshot-clean-install-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();
    let models = ModelDir::new(&empty);

    // An empty folder: not installed, and a meeting is refused (the CLI's text is for
    // developers and stays; the app's copy is tested in earshot-desktop).
    let state = catalogue.install_state(&models, HELPERS_MODEL).unwrap();
    assert_eq!(state.state, InstallState::Missing);
    assert!(matches!(
        host::require_models(&models, "base.en", true),
        Err(RuntimeError::ModelMissing { .. })
    ));
    assert!(matches!(
        init_onnx(&models, Accelerator::Cpu),
        Err(RuntimeError::ModelMissing { .. })
    ));

    // The production download with only the URL's host changed: the fetch that
    // `host::download_model` runs, through an allowlist of this loopback server.
    let mut list = Allowlist::new();
    list.allow_url(&Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap())
        .unwrap();
    let downloader = Downloader::new(list, Arc::new(SystemClock)).unwrap();
    let mut last = (0, 0);
    catalogue
        .download(
            &models,
            HELPERS_MODEL,
            &CancelToken::new(),
            &mut |done, total| {
                assert!(done >= last.0, "progress never goes back");
                last = (done, total);
            },
            &mut |url, dest, exp, cancel, progress| {
                let id = model
                    .artefacts
                    .iter()
                    .find(|id| catalogue.artefact(id).unwrap().url == url.as_str())
                    .expect("a manifest URL");
                let local = Url::parse(&format!("http://127.0.0.1:{port}/{id}")).unwrap();
                downloader.fetch(&local, dest, exp, cancel, progress)
            },
        )
        .expect("the download");
    let total = catalogue.download_size(HELPERS_MODEL).unwrap();
    assert_eq!(last, (total, total));
    println!(
        "downloaded {total} bytes ({} files) in {} requests; {} bytes on disk",
        model.artefacts.len(),
        downloader.audit_log().len(),
        sum_of_files(&empty)
    );
    for entry in downloader.audit_log().entries() {
        assert!(
            matches!(entry.outcome, AuditOutcome::Completed),
            "{entry:?}"
        );
    }

    // Verified, recognised as present, and the archives are gone.
    assert_eq!(catalogue.verify(&models, HELPERS_MODEL).unwrap(), []);
    let state = catalogue.install_state(&models, HELPERS_MODEL).unwrap();
    assert_eq!(state.state, InstallState::Installed);
    assert_eq!((state.held, state.total), (total, total));
    assert!(
        !empty
            .join("archives/onnxruntime-win-x64-1.31.0.zip")
            .exists()
    );
    assert!(
        !empty
            .join("archives/onnxruntime-win-arm64-1.31.0.zip")
            .exists()
    );
    for rel in [SILERO, SEGMENTATION, CAMPPLUS, earshot_onnx::ORT_CPU_DYLIB] {
        assert!(empty.join(rel).is_file(), "{rel}");
    }
    // No debug symbols or headers came along: only what the manifest pins.
    let pdb = Path::new(earshot_onnx::ORT_CPU_DYLIB).with_extension("pdb");
    assert!(!empty.join(pdb).exists());
    let ort = sum_of_files(&empty.join("onnxruntime"));
    assert!(ort < 20_000_000, "ONNX Runtime takes {ort} bytes");

    // A meeting now lacks only the transcription model, which is the user's choice.
    let missing = host::require_models(&models, "base.en", true).unwrap_err();
    match &missing {
        RuntimeError::ModelMissing { path, .. } => {
            assert!(path.ends_with("whisper/ggml-base.en.bin"), "{path:?}");
        }
        other => panic!("{other:?}"),
    }

    // The real load: `earshot_onnx::init` checks the DLL's SHA-256, then the models run.
    let library = init_onnx(&models, Accelerator::Cpu).expect("the downloaded ONNX Runtime loads");
    assert!(library.starts_with(&empty), "{library:?}");
    let mut vad = load_silero(&models, Accelerator::Cpu).expect("Silero loads");
    let p = vad.speech_probability(&[0.0; 512]).unwrap();
    assert!((0.0..=1.0).contains(&p), "{p}");
    let (mut segmenter, mut embedder) =
        load_diarisation(&models, Accelerator::Cpu).expect("segmentation and CAM++ load");
    let window = vec![0.01f32; segmenter.info().window];
    assert_eq!(
        segmenter
            .segment(&[&window], &CancelToken::new())
            .unwrap()
            .len(),
        1
    );
    let speech: Vec<f32> = (0..32_000u16)
        .map(|i| (f32::from(i) * 0.05).sin() * 0.1)
        .collect();
    assert_eq!(
        embedder
            .embed(&[&speech], &CancelToken::new())
            .unwrap()
            .len(),
        1
    );
    // The DLL stays mapped until this process ends, so the temp folder stays too.
    println!(
        "loaded {} and ran Silero, segmentation and CAM++",
        library.display()
    );
}
