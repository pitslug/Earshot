//! Tests of the opt-in CUDA parts (T8.4ae). No network: a fake fetch writes zips built here, except
//! the one ignored test that downloads cuBLAS through the production path.

use std::cell::RefCell;
use std::io::Write as _;

use sha2::{Digest, Sha256};

use super::*;
use crate::download::Outcome;
use crate::testutil::TempDir;

fn sha(bytes: &[u8]) -> String {
    let mut s = String::new();
    for b in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A stored zip holding `(name, bytes)` entries (APPNOTE local headers, central directory, end
/// record).
fn zip(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let (mut out, mut cd) = (Vec::new(), Vec::new());
    for (name, data) in entries {
        let mut crc = flate2::Crc::new();
        crc.update(data);
        let offset = out.len() as u32;
        let size = data.len() as u32;
        let header = |sig: u32, out: &mut Vec<u8>, central: bool| {
            out.write_all(&sig.to_le_bytes()).unwrap();
            if central {
                out.write_all(&20u16.to_le_bytes()).unwrap();
            }
            for v in [20u16, 0, 0, 0, 0] {
                out.write_all(&v.to_le_bytes()).unwrap();
            }
            for v in [crc.sum(), size, size] {
                out.write_all(&v.to_le_bytes()).unwrap();
            }
            out.write_all(&(name.len() as u16).to_le_bytes()).unwrap();
            out.write_all(&0u16.to_le_bytes()).unwrap();
            if central {
                for v in [0u16, 0, 0] {
                    out.write_all(&v.to_le_bytes()).unwrap();
                }
                out.write_all(&0u32.to_le_bytes()).unwrap();
                out.write_all(&offset.to_le_bytes()).unwrap();
            }
            out.write_all(name.as_bytes()).unwrap();
        };
        header(0x0403_4b50, &mut out, false);
        out.write_all(data).unwrap();
        header(0x0201_4b50, &mut cd, true);
    }
    let cd_offset = out.len() as u32;
    out.write_all(&cd).unwrap();
    out.write_all(&0x0605_4b50u32.to_le_bytes()).unwrap();
    for v in [0u16, 0, entries.len() as u16, entries.len() as u16] {
        out.write_all(&v.to_le_bytes()).unwrap();
    }
    out.write_all(&(cd.len() as u32).to_le_bytes()).unwrap();
    out.write_all(&cd_offset.to_le_bytes()).unwrap();
    out.write_all(&0u16.to_le_bytes()).unwrap();
    out
}

/// Bytes of file `name`.
fn bytes_of(name: &str) -> Vec<u8> {
    format!("the bytes of {name}").into_bytes()
}

fn artefact_table(id: &str, bytes: &[u8]) -> String {
    format!(
        "[[artefact]]\nid = \"{id}\"\nsets = [\"s\"]\nlocation = \"cache\"\n\
         url = \"https://example.org/{id}\"\npath = \"archives/{id}.zip\"\n\
         sha256 = \"{}\"\nsize = {}\nlicence = \"x\"\nattribution = \"x\"\n\n",
        sha(bytes),
        bytes.len()
    )
}

fn archive_table(part: &str, id: &str, names: &[&str]) -> String {
    let mut s =
        format!("[[cuda_archive]]\npart = \"{part}\"\nartefact = \"{id}\"\nformat = \"zip\"\n\n");
    for name in names {
        let b = bytes_of(name);
        s.push_str(&format!(
            "[[cuda_archive.file]]\nname = \"{name}\"\nmember = \"bin/{name}\"\n\
             sha256 = \"{}\"\nsize = {}\n\n",
            sha(&b),
            b.len()
        ));
    }
    s
}

/// A fixture manifest with one archive per entry and the zips a fake fetch serves.
struct Fixture {
    catalogue: Catalogue,
    zips: Vec<(String, Vec<u8>)>,
}

fn fixture(archives: &[(&str, &str, &[&str])]) -> Fixture {
    let mut text = "sets = [\"s\"]\nhosts = [\"example.org\"]\n\n".to_owned();
    let mut zips = Vec::new();
    for (part, id, names) in archives {
        let entries: Vec<(String, Vec<u8>)> = names
            .iter()
            .map(|n| (format!("bin/{n}"), bytes_of(n)))
            .chain([("bin/not-wanted.lib".to_owned(), b"no".to_vec())])
            .collect();
        let z = zip(&entries);
        text.push_str(&artefact_table(id, &z));
        text.push_str(&archive_table(part, id, names));
        zips.push(((*id).to_owned(), z));
    }
    Fixture {
        catalogue: Catalogue::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}")),
        zips,
    }
}

/// Both parts complete.
fn complete() -> Fixture {
    fixture(&[
        ("summaries", "s", CudaPart::Summaries.required_files()),
        (
            "transcription",
            "t",
            CudaPart::Transcription.required_files(),
        ),
    ])
}

/// A manifest that pins only cuBLAS of Summaries: llama.cpp's set is not in it.
fn without_the_llama_set() -> Fixture {
    fixture(&[
        ("summaries", "s", &["cublas64_13.dll", "cublasLt64_13.dll"]),
        (
            "transcription",
            "t",
            CudaPart::Transcription.required_files(),
        ),
    ])
}

impl Fixture {
    fn download(&self, models: &ModelDir, part: CudaPart) -> Result<(), ModelDownloadError> {
        let asked = RefCell::new(Vec::new());
        self.download_counting(models, part, &asked)
    }

    fn download_counting(
        &self,
        models: &ModelDir,
        part: CudaPart,
        asked: &RefCell<Vec<String>>,
    ) -> Result<(), ModelDownloadError> {
        download_part(
            &self.catalogue,
            models,
            part,
            &CancelToken::new(),
            &mut |_, _| {},
            &mut |url, dest, _, _, _| {
                let id = url.as_str().rsplit('/').next().unwrap().to_owned();
                asked.borrow_mut().push(id.clone());
                let z = &self.zips.iter().find(|(z, _)| *z == id).unwrap().1;
                std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
                std::fs::write(dest, z).unwrap();
                Ok(Outcome::Downloaded { resumed_from: 0 })
            },
        )
    }
}

/// The artefact of llama.cpp's published CUDA set.
const LLAMA_SET: &str = "earshot-cuda-set-llama-0.1.160-win-x64";

fn models_in(dir: &TempDir) -> ModelDir {
    ModelDir::new(dir.0.join("models"))
}

#[test]
fn cuda_parts_share_one_folder_under_the_models_folder() {
    let models = ModelDir::new("C:/m");
    assert_eq!(folder(&models), PathBuf::from("C:/m").join("cuda"));
    assert_eq!(CudaPart::Summaries.needs(), None);
    assert_eq!(CudaPart::Transcription.needs(), Some(CudaPart::Summaries));
    assert_eq!(
        CudaPart::Summaries.needed_by(),
        Some(CudaPart::Transcription)
    );
    assert_eq!(CudaPart::Transcription.needed_by(), None);
}

#[test]
fn cuda_embedded_manifest_pins_nvidia_at_the_agreed_versions_with_licences() {
    let c = Catalogue::embedded().unwrap();
    // The versions docs/cuda-libraries.md picks: CUDA 13.4.x (cuBLAS 13.7), cuDNN 9.28.
    let cublas = c.artefact("nvidia-cublas-13.7.0.27-win-x64").unwrap();
    assert!(
        cublas
            .url
            .starts_with("https://developer.download.nvidia.com/compute/cuda/redist/")
    );
    assert!(cublas.url.contains("13.7.0.27"), "{}", cublas.url);
    let cudnn = c.artefact("nvidia-cudnn-9.28.0.42-cuda13-win-x64").unwrap();
    assert!(
        cudnn
            .url
            .starts_with("https://developer.download.nvidia.com/compute/cudnn/redist/")
    );
    assert!(cudnn.url.contains("9.28.0.42_cuda13"), "{}", cudnn.url);
    // ONNX Runtime's zip is the one already pinned, reused.
    assert!(c.cuda_archives().iter().any(
        |a| a.part == CudaPart::Transcription && a.artefact == "onnxruntime-win-x64-gpu-cuda13"
    ));
    // Each part's archives keep a licence for what they bring.
    let names =
        |part| -> Vec<String> { part_files(c, part).iter().map(|f| f.name.clone()).collect() };
    let (s, t) = (names(CudaPart::Summaries), names(CudaPart::Transcription));
    assert!(s.iter().any(|n| n == "LICENSE-cublas.txt"), "{s:?}");
    for want in [
        "LICENSE-cudnn.txt",
        "LICENSE-onnxruntime.txt",
        "ThirdPartyNotices-onnxruntime.txt",
    ] {
        assert!(t.iter().any(|n| n == want), "{want}: {t:?}");
    }
    assert!(s.iter().any(|n| n == "LICENSE-llama.cpp"), "{s:?}");
    assert!(s.iter().any(|n| n == "NOTICE.md"), "{s:?}");
    // Every required file of both parts is pinned.
    for part in CudaPart::ALL {
        assert_eq!(not_available_yet(c, part), Vec::<&str>::new(), "{part:?}");
    }
    // cudart is linked statically (docs/cuda-libraries.md): it is not a file of either part.
    assert!(s.iter().chain(&t).all(|n| !n.starts_with("cudart")));
}

#[test]
fn cuda_a_part_missing_required_files_says_they_are_not_available_yet() {
    let f = without_the_llama_set();
    let c = &f.catalogue;
    assert_eq!(
        not_available_yet(c, CudaPart::Summaries),
        [
            "llama.dll",
            "ggml.dll",
            "ggml-base.dll",
            "ggml-cpu.dll",
            "ggml-cuda.dll"
        ]
    );
    let dir = TempDir::new("cuda-gap");
    let models = models_in(&dir);
    let state = install_state(c, &models, CudaPart::Summaries).unwrap();
    assert_eq!(state.state, InstallState::Missing);
    assert!(!state.looks_usable());
    let err = verify_part_in(c, &models, CudaPart::Summaries).unwrap_err();
    assert!(
        matches!(&err, CudaError::NotAvailableYet { part: CudaPart::Summaries, files } if files.contains(&"llama.dll")),
        "{err}"
    );
    assert!(err.to_string().contains("not available yet"), "{err}");
    assert!(matches!(
        RuntimeError::from(err),
        RuntimeError::Unavailable(NOT_READY)
    ));
}

/// The published set is exactly what `Summaries` lists beside cuBLAS, and nothing is outstanding.
#[test]
fn cuda_summaries_lists_exactly_cublas_and_the_published_llama_set() {
    let c = Catalogue::embedded().unwrap();
    assert_eq!(
        not_available_yet(c, CudaPart::Summaries),
        Vec::<&str>::new()
    );
    let set = c
        .cuda_archives()
        .iter()
        .find(|a| a.artefact == LLAMA_SET)
        .unwrap();
    assert_eq!(set.part, CudaPart::Summaries);
    assert_eq!(set.format, "zip");
    let got: Vec<(&str, &str, &str, u64)> = set
        .files
        .iter()
        .map(|f| {
            (
                f.name.as_str(),
                f.member.as_str(),
                f.sha256.as_str(),
                f.size,
            )
        })
        .collect();
    let want: [(&str, u64, &str); 7] = [
        (
            "ggml-base.dll",
            677_376,
            "cd087b8fd775f13a8eeca0ff5b149894de41d6c5d0fa4d379820aa98a8bea481",
        ),
        (
            "ggml-cpu.dll",
            1_101_824,
            "e790db39ba15b8b4980348cf694be29cfc67403e133a0a1c9910e57be04ad429",
        ),
        (
            "ggml-cuda.dll",
            70_161_920,
            "d86092328c8c34b7672c61b2667e261f59b7640ba352a5fd3a4b36ccd4933d78",
        ),
        (
            "ggml.dll",
            59_904,
            "616a6d0eb21d2aa588092abdd85e1b34cfb4b21a71d451f1ad99cd2b66e5303c",
        ),
        (
            "llama.dll",
            2_704_384,
            "7ab448ca2ba8c0c2cd08632ce26ccb111f15eac04630dca624ed03e6c695dd77",
        ),
        (
            "NOTICE.md",
            927,
            "375ee8527c98d585f4fcaedb295addcbd314ef5000929bacf0dd20b2a7a1ce38",
        ),
        (
            "LICENSE-llama.cpp",
            1_078,
            "94f29bbed6a22c35b992c5c6ebf0e7c92f13b836b90f36f461c9cf2f0f1d010d",
        ),
    ];
    let want: Vec<(&str, &str, &str, u64)> =
        want.iter().map(|(n, s, h)| (*n, *n, *h, *s)).collect();
    assert_eq!(got, want, "the zip is flat: a member's name is its path");
    let others: Vec<&str> = c
        .cuda_archives()
        .iter()
        .filter(|a| a.part == CudaPart::Summaries && a.artefact != LLAMA_SET)
        .flat_map(|a| a.files.iter().map(|f| f.name.as_str()))
        .collect();
    assert_eq!(
        others,
        ["LICENSE-cublas.txt", "cublas64_13.dll", "cublasLt64_13.dll"]
    );
    // Hosts the manifest already listed: the set brought no new one.
    let artefact = c.artefact(LLAMA_SET).unwrap();
    assert!(
        artefact.url.starts_with("https://github.com/"),
        "{}",
        artefact.url
    );
    assert_eq!(artefact.size, 57_693_080);
    assert_eq!(
        artefact.sha256,
        "d62d12ea8a425116e4ca81a7a1476b361bd168a684ad06caf1cfa4ab9d4cd290"
    );
}

/// llama.dll is loaded through the exe's delay-load with the symbols of the pinned crate, so the
/// published set must be built from that pin. This compares the version the release names with
/// the pin; it cannot look inside the DLLs. Upgrading the pin fails here until a new release of
/// the set is in the manifest.
#[test]
fn cuda_llama_set_matches_the_llama_cpp_sys_pin() {
    let cargo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("earshot-llm-local")
        .join("Cargo.toml");
    let text = std::fs::read_to_string(&cargo).unwrap();
    let table: toml::Table = text.parse().unwrap();
    let pin = table["dependencies"]["llama-cpp-sys-2"]["version"]
        .as_str()
        .unwrap();
    let version = pin.strip_prefix('=').expect("pinned with =");
    let c = Catalogue::embedded().unwrap();
    let artefact = c.artefact(LLAMA_SET).unwrap();
    let named = format!("llama-{version}-");
    assert!(artefact.id.contains(&named), "{}", artefact.id);
    assert!(artefact.url.contains(&named), "{}", artefact.url);
}

#[test]
fn cuda_a_part_with_every_download_held_is_still_not_installed_while_files_are_unpublished() {
    let f = without_the_llama_set();
    let dir = TempDir::new("cuda-gap-held");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let state = install_state(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert_eq!(state.state, InstallState::Partial);
    assert_eq!(state.held, state.total);
    assert_eq!(state.not_available_yet.len(), 5);
    assert!(!state.looks_usable());
    let mut ran = false;
    let out = load_verified_in(&f.catalogue, &models, CudaPart::Summaries, |_| ran = true);
    assert!(matches!(out, Err(CudaError::NotAvailableYet { .. })));
    assert!(!ran);
}

#[test]
fn cuda_download_lands_every_pinned_file_flat_and_nothing_else() {
    let f = complete();
    let dir = TempDir::new("cuda-dl");
    let models = models_in(&dir);
    assert_eq!(
        install_state(&f.catalogue, &models, CudaPart::Summaries)
            .unwrap()
            .state,
        InstallState::Missing
    );
    let mut seen = Vec::new();
    download_part(
        &f.catalogue,
        &models,
        CudaPart::Summaries,
        &CancelToken::new(),
        &mut |done, total| seen.push((done, total)),
        &mut |_, dest, _, _, _| {
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(dest, &f.zips[0].1).unwrap();
            Ok(Outcome::Downloaded { resumed_from: 0 })
        },
    )
    .unwrap();
    let size = f.zips[0].1.len() as u64;
    assert_eq!(seen.first(), Some(&(0, size)));
    assert_eq!(seen.last(), Some(&(size, size)));
    let mut names: Vec<String> = std::fs::read_dir(folder(&models))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut want: Vec<String> = CudaPart::Summaries
        .required_files()
        .iter()
        .map(|n| (*n).to_owned())
        .collect();
    want.sort();
    assert_eq!(names, want, "flat, no staging folder, no unpinned file");
    // The archive is gone once its files are in place.
    let artefact = f.catalogue.artefact("s").unwrap();
    assert!(!artefact.file(&models).unwrap().exists());
    let state = install_state(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert_eq!(state.state, InstallState::Installed);
    assert!(state.looks_usable());
    verify_part_in(&f.catalogue, &models, CudaPart::Summaries).unwrap();
}

#[test]
fn cuda_a_second_download_costs_no_request_and_a_damaged_file_is_repaired() {
    let f = complete();
    let dir = TempDir::new("cuda-dl2");
    let models = models_in(&dir);
    let asked = RefCell::new(Vec::new());
    f.download_counting(&models, CudaPart::Summaries, &asked)
        .unwrap();
    f.download_counting(&models, CudaPart::Summaries, &asked)
        .unwrap();
    assert_eq!(asked.borrow().len(), 1, "already right: no request");
    let file = folder(&models).join("cublas64_13.dll");
    let mut damaged = std::fs::read(&file).unwrap();
    damaged[0] ^= 0xff;
    std::fs::write(&file, &damaged).unwrap();
    f.download_counting(&models, CudaPart::Summaries, &asked)
        .unwrap();
    assert_eq!(asked.borrow().len(), 2);
    verify_part_in(&f.catalogue, &models, CudaPart::Summaries).unwrap();
}

#[test]
fn cuda_a_cancelled_or_refused_archive_is_kept_and_publishes_nothing() {
    let f = complete();
    let dir = TempDir::new("cuda-cancel");
    let models = models_in(&dir);
    let cancel = CancelToken::new();
    let out = download_part(
        &f.catalogue,
        &models,
        CudaPart::Summaries,
        &cancel,
        &mut |_, _| {},
        &mut |_, dest, _, _, _| {
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(dest, &f.zips[0].1).unwrap();
            cancel.cancel();
            Ok(Outcome::Downloaded { resumed_from: 0 })
        },
    );
    assert!(out.is_err_and(|e| e.cancelled()));
    let archive = f.catalogue.artefact("s").unwrap().file(&models).unwrap();
    assert!(archive.exists(), "kept for the next run");
    assert_eq!(
        install_state(&f.catalogue, &models, CudaPart::Summaries)
            .unwrap()
            .state,
        InstallState::Partial
    );
    // An archive that does not hold what the manifest pins publishes nothing.
    let hollow = zip(&[("bin/other.dll".to_owned(), b"x".to_vec())]);
    let out = download_part(
        &f.catalogue,
        &models,
        CudaPart::Summaries,
        &CancelToken::new(),
        &mut |_, _| {},
        &mut |_, dest, _, _, _| {
            std::fs::write(dest, &hollow).unwrap();
            Ok(Outcome::Downloaded { resumed_from: 0 })
        },
    );
    let err = out.unwrap_err();
    assert!(matches!(err, ModelDownloadError::Extract { .. }), "{err}");
    assert!(!err.retryable(), "the same bytes give the same answer");
    assert!(!folder(&models).join("cublas64_13.dll").exists());
    assert!(!folder(&models).join(".staging-s").exists());
}

/// The accept case of T8.4ae: a library failing its SHA-256 is not loaded.
#[test]
fn cuda_a_library_failing_its_sha256_is_not_loaded() {
    let f = complete();
    let dir = TempDir::new("cuda-load");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let mut loaded = Vec::new();
    load_verified_in(&f.catalogue, &models, CudaPart::Summaries, |dir| {
        loaded.push(dir.to_path_buf());
    })
    .unwrap();
    assert_eq!(loaded, [folder(&models)], "right files: the load runs");
    // The same size, one byte different: sizes say installed, the load refuses it by name.
    let file = folder(&models).join("ggml-cuda.dll");
    let mut changed = std::fs::read(&file).unwrap();
    changed[3] ^= 0x01;
    std::fs::write(&file, &changed).unwrap();
    assert_eq!(
        install_state(&f.catalogue, &models, CudaPart::Summaries)
            .unwrap()
            .state,
        InstallState::Installed,
        "install state reads sizes only"
    );
    let out = load_verified_in(&f.catalogue, &models, CudaPart::Summaries, |dir| {
        loaded.push(dir.to_path_buf());
    });
    let err = out.unwrap_err();
    assert!(
        matches!(&err, CudaError::FailedCheck { name, .. } if name == "ggml-cuda.dll"),
        "{err}"
    );
    assert!(err.to_string().contains("ggml-cuda.dll"), "{err}");
    assert_eq!(loaded.len(), 1, "the changed file never reached a load");
    // A missing file is named too.
    std::fs::remove_file(&file).unwrap();
    let err = verify_part_in(&f.catalogue, &models, CudaPart::Summaries).unwrap_err();
    assert!(
        matches!(&err, CudaError::MissingFile { name, .. } if name == "ggml-cuda.dll"),
        "{err}"
    );
}

#[test]
fn cuda_a_part_whose_needed_part_is_missing_or_damaged_is_not_usable() {
    let f = complete();
    let dir = TempDir::new("cuda-needs");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Transcription).unwrap();
    let state = install_state(&f.catalogue, &models, CudaPart::Transcription).unwrap();
    assert_eq!(state.state, InstallState::Installed);
    assert!(state.needs_missing);
    assert!(!state.looks_usable());
    let mut ran = false;
    let out = load_verified_in(&f.catalogue, &models, CudaPart::Transcription, |_| {
        ran = true
    });
    assert!(
        matches!(
            out,
            Err(CudaError::MissingFile {
                part: CudaPart::Summaries,
                ..
            })
        ),
        "{out:?}"
    );
    assert!(!ran);
    f.download(&models, CudaPart::Summaries).unwrap();
    assert!(
        install_state(&f.catalogue, &models, CudaPart::Transcription)
            .unwrap()
            .looks_usable()
    );
    verify_part_in(&f.catalogue, &models, CudaPart::Transcription).unwrap();
    // cuBLAS changed: Transcription, which imports it, is refused.
    let file = folder(&models).join("cublasLt64_13.dll");
    let mut changed = std::fs::read(&file).unwrap();
    changed[0] ^= 0x01;
    std::fs::write(&file, &changed).unwrap();
    let err = verify_part_in(&f.catalogue, &models, CudaPart::Transcription).unwrap_err();
    assert!(
        matches!(&err, CudaError::FailedCheck { part: CudaPart::Summaries, name, .. } if name == "cublasLt64_13.dll"),
        "{err}"
    );
}

/// A file the manifest does not pin is not what ONNX Runtime imports: Transcription runs on the
/// files of Summaries that exist (module docs).
#[test]
fn cuda_transcription_needs_the_pinned_files_of_summaries_not_the_unpublished_ones() {
    let f = without_the_llama_set();
    let dir = TempDir::new("cuda-needs-gap");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Transcription).unwrap();
    assert!(verify_part_in(&f.catalogue, &models, CudaPart::Transcription).is_err());
    f.download(&models, CudaPart::Summaries).unwrap();
    verify_part_in(&f.catalogue, &models, CudaPart::Transcription).unwrap();
    assert!(
        install_state(&f.catalogue, &models, CudaPart::Transcription)
            .unwrap()
            .looks_usable()
    );
    assert!(
        !install_state(&f.catalogue, &models, CudaPart::Summaries)
            .unwrap()
            .looks_usable()
    );
}

#[test]
fn cuda_removing_summaries_is_refused_while_transcription_is_installed() {
    let f = complete();
    let dir = TempDir::new("cuda-remove");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    f.download(&models, CudaPart::Transcription).unwrap();
    let err = remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap_err();
    assert!(
        matches!(
            err,
            CudaError::NeededBy {
                part: CudaPart::Summaries,
                by: CudaPart::Transcription
            }
        ),
        "{err}"
    );
    assert!(
        folder(&models).join("cublas64_13.dll").exists(),
        "nothing removed"
    );
    let removed = remove_part(&f.catalogue, &models, CudaPart::Transcription).unwrap();
    assert_eq!(
        removed.files.len(),
        CudaPart::Transcription.required_files().len()
    );
    assert!(removed.bytes_freed > 0);
    assert!(
        folder(&models).join("cublas64_13.dll").exists(),
        "the other part stays"
    );
    assert!(!folder(&models).join("cudnn64_9.dll").exists());
    remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert!(
        !folder(&models).exists(),
        "the folder goes with its last file"
    );
    assert_eq!(
        install_state(&f.catalogue, &models, CudaPart::Summaries)
            .unwrap()
            .state,
        InstallState::Missing
    );
}

#[test]
fn cuda_removal_also_clears_an_interrupted_download() {
    let f = complete();
    let dir = TempDir::new("cuda-remove-part");
    let models = models_in(&dir);
    let archive = f.catalogue.artefact("s").unwrap().file(&models).unwrap();
    std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
    std::fs::write(part_path(&archive), b"some").unwrap();
    assert_eq!(
        install_state(&f.catalogue, &models, CudaPart::Summaries)
            .unwrap()
            .state,
        InstallState::Partial
    );
    let removed = remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert_eq!(removed.bytes_freed, 4);
    assert!(!part_path(&archive).exists());
}

/// A file held open: nothing goes, the holder is named, and a retry after it lets go works.
#[cfg(windows)]
#[test]
fn cuda_a_file_held_open_stops_the_removal_whole_and_a_retry_works() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let f = complete();
    let dir = TempDir::new("cuda-held");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let held = folder(&models).join("ggml.dll");
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&held)
        .unwrap();
    let err = remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap_err();
    assert!(
        matches!(&err, CudaError::InUse { name, .. } if name == "ggml.dll"),
        "{err}"
    );
    assert!(err.to_string().contains("ggml.dll"), "{err}");
    assert!(
        folder(&models).join("cublas64_13.dll").exists(),
        "nothing removed"
    );
    drop(handle);
    remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert!(!held.exists());
}

/// What a loaded DLL does, without loading one: a running program is mapped as an image the same
/// way, and Windows lets neither be written or deleted.
#[cfg(windows)]
#[test]
fn cuda_a_file_mapped_as_a_running_image_is_reported_in_use_and_a_retry_works() {
    let exe = "held.exe";
    let f = {
        let mut required: Vec<&str> = CudaPart::Summaries.required_files().to_vec();
        required.push(exe);
        fixture(&[("summaries", "s", &required)])
    };
    let dir = TempDir::new("cuda-mapped");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let held = folder(&models).join(exe);
    // Any program that waits: Windows' own ping, copied under the manifest's name.
    let system = std::env::var_os("SystemRoot").expect("SystemRoot");
    std::fs::copy(
        PathBuf::from(system).join("System32").join("ping.exe"),
        &held,
    )
    .unwrap();
    let mut child = std::process::Command::new(&held)
        .args(["-n", "60", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let err = remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap_err();
    assert!(
        matches!(&err, CudaError::InUse { name, .. } if name == exe),
        "{err}"
    );
    assert!(
        folder(&models).join("llama.dll").exists(),
        "nothing removed"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    // The system lets go of the image shortly after the process ends.
    let mut out = remove_part(&f.catalogue, &models, CudaPart::Summaries);
    for _ in 0..50 {
        if out.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        out = remove_part(&f.catalogue, &models, CudaPart::Summaries);
    }
    out.unwrap();
    assert!(!held.exists());
}

#[test]
fn cuda_broken_manifests_are_errors() {
    let z = zip(&[("bin/a.dll".to_owned(), bytes_of("a.dll"))]);
    let base = format!(
        "sets = [\"s\"]\nhosts = [\"example.org\"]\n\n{}",
        artefact_table("x", &z)
    );
    let parse = |extra: &str| Catalogue::parse(&format!("{base}{extra}"));
    assert!(parse(&archive_table("summaries", "x", &["a.dll"])).is_ok());
    let cases = [
        (archive_table("other", "x", &["a.dll"]), "unknown part"),
        (
            archive_table("summaries", "y", &["a.dll"]),
            "unknown artefact",
        ),
        (archive_table("summaries", "x", &[]), "names no file"),
        (
            archive_table("summaries", "x", &["a.dll"])
                .replace("format = \"zip\"", "format = \"7z\""),
            "cannot extract",
        ),
        (
            archive_table("summaries", "x", &["sub/a.dll"]),
            "not a plain name",
        ),
        (
            archive_table("summaries", "x", &["c:a.dll"]),
            "not a plain name",
        ),
        (
            archive_table("summaries", "x", &["a.dll", "A.DLL"]),
            "repeats",
        ),
        (
            format!(
                "{}{}",
                archive_table("summaries", "x", &["a.dll"]),
                archive_table("transcription", "x", &["b.dll"])
            ),
            "listed twice",
        ),
    ];
    for (extra, why) in cases {
        let err = parse(&extra).unwrap_err().to_string();
        assert!(err.contains(why), "{why}: {err}");
    }
    // No name is in both parts.
    let z2 = artefact_table("y", b"zip").replace("archives/y.zip", "archives/y2.zip");
    let two = format!(
        "{z2}{}{}",
        archive_table("summaries", "x", &["a.dll"]),
        archive_table("transcription", "y", &["a.dll"])
    );
    let err = parse(&two).unwrap_err().to_string();
    assert!(err.contains("repeats"), "{err}");
    // An unknown field is refused, as by xtask's parser.
    let unknown =
        archive_table("summaries", "x", &["a.dll"]).replace("format =", "extra = 1\nformat =");
    assert!(parse(&unknown).is_err());
    // A file's pin must equal the artefact's own pin of that member.
    let pinned = base.clone()
        + "[artefact.extract]\nformat = \"zip\"\ninto = \"x\"\n\n[[artefact.extract.member]]\n\
           path = \"bin/a.dll\"\nsize = 1\n"
        + &format!("sha256 = \"{}\"\n\n", "0".repeat(64));
    let err = Catalogue::parse(&format!(
        "{pinned}{}",
        archive_table("summaries", "x", &["a.dll"])
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("own pin"), "{err}");
    // Nothing else may store a file in the folder.
    let inside = base.replace("archives/x.zip", "cuda/x.zip");
    let err = Catalogue::parse(&inside).unwrap_err().to_string();
    assert!(err.contains("inside the CUDA folder"), "{err}");
}

#[test]
fn cuda_files_are_never_listed_as_models() {
    let c = Catalogue::embedded().unwrap();
    for a in c.cuda_archives() {
        assert!(
            c.models()
                .iter()
                .all(|m| !m.artefacts.contains(&a.artefact)),
            "{} is a model's artefact",
            a.artefact
        );
    }
}

/// Windows resolves imports from the folder, so a stray library must stop the load.
#[test]
fn cuda_an_unexpected_library_in_the_folder_stops_the_load_and_other_files_do_not() {
    let f = complete();
    let dir = TempDir::new("cuda-unexpected");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let folder = folder(&models);
    // Neither a file of another kind nor a library inside a staging folder can be imported.
    std::fs::write(folder.join("notes.txt"), b"mine").unwrap();
    let staging = folder.join(".staging-other");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("version.dll"), b"unpacking").unwrap();
    std::fs::create_dir_all(folder.join("subfolder.dll")).unwrap();
    verify_part_in(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    // A file another part pins is expected, wherever its part stands.
    std::fs::write(folder.join("cudnn64_9.dll"), b"other part").unwrap();
    verify_part_in(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    std::fs::remove_file(folder.join("cudnn64_9.dll")).unwrap();

    let stray = folder.join("Version.DLL");
    std::fs::write(&stray, b"not pinned").unwrap();
    let mut ran = false;
    let out = load_verified_in(&f.catalogue, &models, CudaPart::Summaries, |_| ran = true);
    let err = out.unwrap_err();
    assert!(
        matches!(&err, CudaError::UnexpectedLibrary { name, .. } if name == "Version.DLL"),
        "{err}"
    );
    assert!(err.to_string().contains("Version.DLL"), "{err}");
    assert!(!ran);
    let state = install_state(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert_eq!(state.unexpected_libraries, ["Version.DLL"]);
    assert_eq!(state.state, InstallState::Installed);
    assert!(!state.looks_usable());
    // Transcription, which loads from the same folder, is stopped too.
    f.download(&models, CudaPart::Transcription).unwrap();
    assert!(matches!(
        verify_part_in(&f.catalogue, &models, CudaPart::Transcription),
        Err(CudaError::UnexpectedLibrary { .. })
    ));
    std::fs::remove_file(&stray).unwrap();
    verify_part_in(&f.catalogue, &models, CudaPart::Transcription).unwrap();
}

/// An orphan never makes removal fail: it stays, named, and later removals still work.
#[test]
fn cuda_removal_reports_what_it_left_and_never_fails_on_it() {
    let f = complete();
    let dir = TempDir::new("cuda-orphan");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    f.download(&models, CudaPart::Transcription).unwrap();
    let cuda = folder(&models);
    std::fs::write(cuda.join("old-version.dll"), b"orphan").unwrap();
    std::fs::write(cuda.join("notes.txt"), b"mine").unwrap();
    // The other part's files are not orphans.
    let removed = remove_part(&f.catalogue, &models, CudaPart::Transcription).unwrap();
    assert_eq!(removed.left_in_folder, ["notes.txt", "old-version.dll"]);
    // A crash's staging folder of another part: Earshot's own, and its name holds a manifest id.
    std::fs::create_dir_all(cuda.join(".staging-an-artefact-id")).unwrap();
    let removed = remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert_eq!(removed.left_in_folder, ["notes.txt", "old-version.dll"]);
    std::fs::remove_dir_all(cuda.join(".staging-an-artefact-id")).unwrap();
    assert!(cuda.join("old-version.dll").exists(), "never deleted");
    assert!(!cuda.join("llama.dll").exists());
    assert!(
        remove_part(&f.catalogue, &models, CudaPart::Summaries).is_ok(),
        "again, with nothing of ours left"
    );
    std::fs::remove_file(cuda.join("old-version.dll")).unwrap();
    std::fs::remove_file(cuda.join("notes.txt")).unwrap();
    let removed = remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert_eq!(removed.left_in_folder, Vec::<String>::new());
    assert!(!cuda.exists(), "the folder goes once it is empty");
}

fn set_read_only(path: &Path) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions).unwrap();
}

/// A restart never fixes a read-only file, so it is never called in use.
#[test]
fn cuda_a_read_only_file_is_removed_not_reported_in_use() {
    let f = complete();
    let dir = TempDir::new("cuda-read-only");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let file = folder(&models).join("ggml.dll");
    set_read_only(&file);
    remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert!(!file.exists());
}

/// Read-only and held: that one is in use, and says so.
#[cfg(windows)]
#[test]
fn cuda_a_read_only_file_held_open_is_in_use() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let f = complete();
    let dir = TempDir::new("cuda-read-only-held");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let file = folder(&models).join("ggml.dll");
    set_read_only(&file);
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&file)
        .unwrap();
    let err = remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap_err();
    assert!(
        matches!(&err, CudaError::InUse { name, .. } if name == "ggml.dll"),
        "{err}"
    );
    assert!(
        folder(&models).join("llama.dll").exists(),
        "nothing removed"
    );
    assert!(err.to_string().contains("in use or not writable"), "{err}");
    drop(handle);
    remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap();
}

/// TODO 40: a link in the CUDA folder named like a pinned file is removed as a link: the file it
/// points at, held open elsewhere and read-only, neither stops the removal nor changes.
#[cfg(windows)]
#[test]
#[ignore = "creates a file symlink: run with `-- --ignored` from a shell with Developer Mode on or elevated"]
fn cuda_a_link_in_the_folder_is_removed_without_touching_its_target() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let f = complete();
    let dir = TempDir::new("cuda-link");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let link = folder(&models).join("ggml.dll");
    let target = dir.0.join("elsewhere.dll");
    std::fs::rename(&link, &target).unwrap();
    std::os::windows::fs::symlink_file(&target, &link).unwrap();
    set_read_only(&target);
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&target)
        .unwrap();
    remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert!(std::fs::symlink_metadata(&link).is_err(), "the link went");
    drop(handle);
    let kept = std::fs::metadata(&target).unwrap();
    assert!(kept.permissions().readonly(), "the target is untouched");
}

/// A read-only file whose removal is stopped by a later held file keeps its attribute: the check
/// that asked changes nothing about a file it then leaves.
#[cfg(windows)]
#[test]
fn cuda_a_removal_stopped_by_a_held_file_leaves_a_read_only_file_read_only() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let f = complete();
    let dir = TempDir::new("cuda-read-only-left");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let names: Vec<String> = part_files(&f.catalogue, CudaPart::Summaries)
        .iter()
        .map(|file| file.name.clone())
        .collect();
    let (first, last) = (&names[0], names.last().unwrap());
    assert_ne!(first, last, "the fixture has two files");
    let read_only = folder(&models).join(first);
    set_read_only(&read_only);
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(folder(&models).join(last))
        .unwrap();
    let err = remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap_err();
    assert!(
        matches!(&err, CudaError::InUse { name, .. } if name == last),
        "{err}"
    );
    assert!(
        std::fs::metadata(&read_only)
            .unwrap()
            .permissions()
            .readonly()
    );
    drop(handle);
    remove_part(&f.catalogue, &models, CudaPart::Summaries).unwrap();
    assert!(!read_only.exists());
}

/// A damaged file that is held: the download says it is in use, never that the disk is full, and
/// the retry after it is let go repairs it.
#[cfg(windows)]
#[test]
fn cuda_a_download_that_cannot_replace_a_held_file_says_it_is_in_use() {
    use std::os::windows::fs::OpenOptionsExt as _;

    let f = complete();
    let dir = TempDir::new("cuda-replace-held");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let file = folder(&models).join("ggml.dll");
    std::fs::write(&file, b"damaged").unwrap();
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&file)
        .unwrap();
    let err = f.download(&models, CudaPart::Summaries).unwrap_err();
    assert!(
        matches!(
            &err,
            ModelDownloadError::Extract {
                error: archive::ExtractError::InUse { .. },
                ..
            }
        ),
        "{err}"
    );
    assert!(err.retryable());
    assert!(err.to_string().contains("restart Earshot"), "{err}");
    assert!(!folder(&models).join(".staging-s").exists());
    drop(handle);
    f.download(&models, CudaPart::Summaries).unwrap();
    verify_part_in(&f.catalogue, &models, CudaPart::Summaries).unwrap();
}

/// A damaged file marked read-only is repaired by the next download.
#[test]
fn cuda_a_download_replaces_a_damaged_read_only_file() {
    let f = complete();
    let dir = TempDir::new("cuda-replace-read-only");
    let models = models_in(&dir);
    f.download(&models, CudaPart::Summaries).unwrap();
    let file = folder(&models).join("ggml.dll");
    std::fs::write(&file, b"damaged").unwrap();
    set_read_only(&file);
    f.download(&models, CudaPart::Summaries).unwrap();
    verify_part_in(&f.catalogue, &models, CudaPart::Summaries).unwrap();
}

/// The real download: all of `Summaries` (NVIDIA's cuBLAS and the published llama.cpp set) through
/// the production path, into the folder `EARSHOT_CUDA_TEST_MODELS` names (never the user's models
/// folder), fully verified, sizes and time printed. About 481 MB.
#[test]
#[ignore = "downloads 481 MB from NVIDIA and GitHub: set EARSHOT_CUDA_TEST_MODELS to an empty folder and run cargo test -p earshot-runtime cuda_real_download -- --ignored --nocapture"]
fn cuda_real_download_of_the_summaries_part() {
    let root = std::env::var_os("EARSHOT_CUDA_TEST_MODELS").expect("EARSHOT_CUDA_TEST_MODELS");
    let models = ModelDir::new(root);
    let c = Catalogue::embedded().unwrap();
    let clock = std::sync::Arc::new(earshot_core::identity::ManualClock::new(0));
    let downloader = crate::download::Downloader::for_manifest(clock).unwrap();
    let started = std::time::Instant::now();
    crate::host::download_cuda_part(
        &downloader,
        &models,
        CudaPart::Summaries,
        &CancelToken::new(),
        &mut |_, _| {},
    )
    .unwrap();
    let took = started.elapsed();
    for f in part_files(c, CudaPart::Summaries) {
        let path = folder(&models).join(&f.name);
        assert_eq!(
            file_problem(&path, &f.sha256, f.size).unwrap(),
            None,
            "{}",
            f.name
        );
    }
    let state = install_state(c, &models, CudaPart::Summaries).unwrap();
    assert_eq!(state.state, InstallState::Installed);
    assert!(state.looks_usable());
    verify_part(&models, CudaPart::Summaries).unwrap();
    println!(
        "downloaded {} bytes, {} bytes on disk, in {took:?}; state {:?}, not available yet: {:?}",
        download_size(c, CudaPart::Summaries).unwrap(),
        disk_size(c, CudaPart::Summaries),
        state.state,
        state.not_available_yet,
    );
    for e in downloader.audit_log().entries() {
        println!("audit: {e:?}");
    }
}

// Parakeet on CUDA (T8.4ad).
#[test]
fn cuda_transcription_set_names_each_file_once() {
    let mut files = transcription_files();
    assert_eq!(files.len(), 15);
    files.sort_unstable();
    files.dedup();
    assert_eq!(files.len(), 15);
    assert!(files.contains(&ONNX_RUNTIME_DLL));
}

#[test]
fn cuda_a_missing_transcription_file_is_named() {
    let dir = std::env::temp_dir().join(format!("earshot-cuda-set-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(missing_transcription_files(&dir).len(), 15);
    for f in transcription_files() {
        std::fs::write(dir.join(f), b"x").unwrap();
    }
    assert!(missing_transcription_files(&dir).is_empty());
    std::fs::remove_file(dir.join("cudnn_ops64_9.dll")).unwrap();
    assert_eq!(missing_transcription_files(&dir), ["cudnn_ops64_9.dll"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Being there is not enough: every byte counts, and nothing it says names a file.
#[test]
fn cuda_the_check_before_a_transcription_load_reads_every_byte() {
    let dir = std::env::temp_dir().join(format!("earshot-cuda-verify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let models = ModelDir::new(&dir);
    let refused = |models: &ModelDir| {
        let e = verify_transcription_folder(models).unwrap_err();
        assert!(matches!(e, RuntimeError::Unavailable(NOT_READY)), "{e}");
        assert!(!e.to_string().contains(".dll"), "{e}");
    };
    refused(&models);
    // Every pinned file there with its right size and the wrong bytes: the sizes of a page's poll
    // would call this installed.
    let c = Catalogue::embedded().unwrap();
    let folder = folder(&models);
    std::fs::create_dir_all(&folder).unwrap();
    for part in CudaPart::ALL {
        for f in part_files(c, part) {
            let file = std::fs::File::create(folder.join(&f.name)).unwrap();
            file.set_len(f.size).unwrap();
        }
    }
    assert!(
        install_state(c, &models, CudaPart::Transcription)
            .unwrap()
            .looks_usable()
    );
    refused(&models);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cuda_saying_no_changes_nothing() {
    assert!(use_for_transcription(&ModelDir::new("does/not/exist"), false).is_ok());
    assert!(!transcription_from_folder());
}

/// The choice is one decision, asked any number of times; a library that failed ends it.
#[test]
fn cuda_the_choice_is_one_decision() {
    let choice = Choice::new();
    assert!(choice.is_open());
    assert!(!choice.chosen());
    assert!(choice.choose());
    assert!(choice.choose(), "one choice, asked twice");
    assert!(choice.chosen());
    assert!(!choice.is_open());

    let failed = Choice::new();
    assert!(failed.choose());
    failed.load_failed();
    assert!(failed.has_load_failed());
    assert!(!failed.choose(), "off for the rest of the process");
    assert!(!failed.chosen());
}

/// Everything that loads ONNX Runtime in a host reads the order after the choice
/// (the app's two paths are pinned in its own tests).
#[test]
fn cuda_every_runtime_path_to_onnx_runtime_waits_for_the_choice() {
    for (name, text, wait) in [
        (
            "benchmark.rs",
            include_str!("benchmark.rs"),
            "host::settled_order()",
        ),
        (
            "my_voice.rs",
            include_str!("my_voice.rs"),
            "crate::host::settled_order()",
        ),
    ] {
        let at = text.find(wait);
        assert!(at.is_some(), "{name}");
        assert!(at < text.find("init_onnx_for("), "{name}");
    }
}
