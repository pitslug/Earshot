use std::path::Path;

use super::*;
use crate::backends::{
    AsrModel, CAMPPLUS, PARAKEET_MODELS, ParakeetDecoder, SEGMENTATION, SILERO, WHISPER_MODELS,
    asr_model,
};
use crate::model_check::{LADDERS, MODELS};
use crate::summary_model::SUMMARY_MODELS;
use crate::testutil::TempDir;

fn catalogue() -> &'static Catalogue {
    Catalogue::embedded().expect("the embedded manifest is a valid catalogue")
}

#[test]
fn catalogue_embedded_manifest_parses_with_every_path_in_the_models_directory() {
    let c = catalogue();
    assert!(!c.artefacts().is_empty());
    let root = Path::new("models-root");
    for a in c.artefacts() {
        let file = jail_join(root, &a.path).unwrap();
        assert!(file.starts_with(root), "{}", a.path);
        assert!(a.url.starts_with("https://"), "{}", a.id);
        if let Some(x) = &a.extract {
            for m in &x.members {
                let inside = jail_join(&jail_join(root, &x.into).unwrap(), &m.path).unwrap();
                assert!(inside.starts_with(root));
            }
        }
    }
    // Models the user chooses are plain files: only the helpers need archive extraction.
    for m in c.models().iter().filter(|m| m.kind != ModelKind::Helpers) {
        for a in c.artefacts_of(&m.name).unwrap() {
            assert!(a.extract.is_none(), "{} needs extraction", m.name);
        }
    }
}

#[test]
fn catalogue_cuda_folder_rule_follows_the_shared_cases() {
    #[derive(Deserialize)]
    struct Cases {
        case: Vec<Case>,
    }
    #[derive(Deserialize)]
    struct Case {
        path: String,
        inside: bool,
    }
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/models/cuda_folder_cases.toml");
    let cases: Cases = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert!(!cases.case.is_empty());
    for c in cases.case {
        assert_eq!(in_cuda_folder(&c.path), c.inside, "{}", c.path);
    }
}

#[test]
fn catalogue_transcription_models_match_the_runtime_tables() {
    let c = catalogue();
    let listed: Vec<&str> = c
        .models_of(ModelKind::Transcription)
        .map(|m| m.name.as_str())
        .collect();
    let facts: Vec<&str> = MODELS.iter().map(|m| m.name).collect();
    assert_eq!(
        listed, facts,
        "model_check::MODELS lists the manifest's models in order"
    );
    let mut runtime: Vec<&str> = WHISPER_MODELS.iter().map(|(n, _)| *n).collect();
    runtime.extend(PARAKEET_MODELS.iter().map(|p| p.name));
    let mut sorted = listed.clone();
    sorted.sort_unstable();
    runtime.sort_unstable();
    assert_eq!(
        sorted, runtime,
        "backends' tables hold every manifest model"
    );
    for m in c.models_of(ModelKind::Transcription) {
        let f = crate::model_check::model_facts(&m.name).unwrap();
        assert_eq!(f.languages, m.languages.as_slice(), "{} languages", m.name);
        assert_eq!(f.size, c.download_size(&m.name).unwrap(), "{} size", m.name);
        // The runtime loads exactly the catalogue's files.
        let paths: Vec<String> = c
            .artefacts_of(&m.name)
            .unwrap()
            .iter()
            .map(|a| a.path.clone())
            .collect();
        let model = asr_model(&m.name).unwrap();
        assert_eq!(model.files(), paths, "{} files", m.name);
        // The fetch set the runtime names holds every file.
        let set = crate::backends::model_fetch_set(&m.name).unwrap();
        for a in c.artefacts_of(&m.name).unwrap() {
            assert!(a.sets.iter().any(|s| s == set), "{} not in {set}", a.id);
        }
        match (&model, m.engine) {
            (AsrModel::Whisper(_), Engine::Whisper) => {}
            (AsrModel::Parakeet(p), Engine::ParakeetCtc) => {
                assert_eq!(p.decoder, ParakeetDecoder::Ctc);
            }
            (AsrModel::Parakeet(p), Engine::ParakeetTdt) => {
                assert_eq!(p.decoder, ParakeetDecoder::Tdt);
            }
            other => panic!("{}: engine mismatch {other:?}", m.name),
        }
    }
}

#[test]
fn catalogue_ladders_match_the_model_check() {
    let c = catalogue();
    assert_eq!(c.ladders().len(), LADDERS.len());
    for (ours, theirs) in c.ladders().iter().zip(LADDERS) {
        assert_eq!(ours.backend, theirs.backend);
        assert_eq!(ours.scope, theirs.scope);
        assert_eq!(ours.models, theirs.models);
        assert_eq!(c.ladder(ours.backend, ours.scope), Some(ours));
    }
}

#[test]
fn catalogue_summary_models_are_the_shipped_table() {
    let c = catalogue();
    let listed: Vec<&Model> = c.models_of(ModelKind::Summary).collect();
    assert_eq!(listed.len(), SUMMARY_MODELS.len());
    for s in SUMMARY_MODELS {
        let m = c.model(s.id).unwrap_or_else(|| panic!("{} missing", s.id));
        assert_eq!(m.kind, ModelKind::Summary);
        assert_eq!(m.engine, Engine::Llama);
        let files = c.artefacts_of(s.id).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, s.file);
        assert_eq!(files[0].size, s.size);
        assert_eq!(c.download_size(s.id).unwrap(), s.size);
    }
}

#[test]
fn catalogue_helper_models_are_never_listed_one_by_one() {
    let c = catalogue();
    let model_paths: BTreeSet<String> = c
        .models()
        .iter()
        .filter(|m| m.kind != ModelKind::Helpers)
        .flat_map(|m| c.artefacts_of(&m.name).unwrap())
        .map(|a| a.path.clone())
        .collect();
    for helper in [SILERO, CAMPPLUS] {
        assert!(c.artefacts().iter().any(|a| a.path == helper), "{helper}");
        assert!(!model_paths.contains(helper), "{helper} is a model file");
    }
    // The segmentation model is an extracted archive member, not a model file.
    let seg = c
        .artefacts()
        .iter()
        .find_map(|a| {
            let x = a.extract.as_ref()?;
            x.members
                .iter()
                .any(|m| format!("{}/{}", x.into, m.path) == SEGMENTATION)
                .then_some(a)
        })
        .expect("segmentation is a manifest member");
    assert!(!model_paths.contains(&seg.path));
    for m in c.models() {
        assert!(c.model(&m.name).is_some());
        assert!(!["silero-vad", "campplus-en-voxceleb"].contains(&m.name.as_str()));
    }
}

#[test]
fn catalogue_jail_join_keeps_paths_in_the_models_directory() {
    let root = Path::new("cache");
    assert_eq!(
        jail_join(root, "a/b/c.bin").unwrap(),
        root.join("a").join("b").join("c.bin")
    );
    assert_eq!(jail_join(root, "./a").unwrap(), root.join("a"));
    // The cases of xtask's `jail_join_rejects_escapes`.
    for bad in [
        "",
        ".",
        "../x",
        "a/../../x",
        "a/..",
        "/etc/passwd",
        "\\x",
        "C:\\x",
        "C:x",
        "\\\\server\\share\\x",
        "a/b:stream",
    ] {
        assert!(
            matches!(jail_join(root, bad), Err(CatalogueError::Path { .. })),
            "{bad:?} must be rejected"
        );
    }
}

const SHA_A: &str = "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb";
const SHA_B: &str = "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009d";
const SHA_C: &str = "2e7d2c03a9507ae265ecf5b5356885a53393a2029d241394997265a1a25aefc6";

/// A small manifest: model `one` (files `a` and `b`), model `two` sharing `b` with `one`
/// plus its own `c`, a helper artefact and a summary model.
fn manifest(extra_artefact: &str, extra_model: &str) -> String {
    let artefact = |id: &str, path: &str, sha: &str, size: u64, sets: &str| {
        format!(
            "[[artefact]]\nid = \"{id}\"\nsets = [{sets}]\nlocation = \"cache\"\n\
             url = \"https://example.org/{id}\"\npath = \"{path}\"\nsha256 = \"{sha}\"\n\
             size = {size}\nlicence = \"MIT\"\nattribution = \"test\"\n\n"
        )
    };
    let mut text = String::from("sets = [\"ci\", \"summary\"]\nhosts = [\"example.org\"]\n\n");
    text += &artefact("a", "m/a.bin", SHA_A, 1, "\"ci\"");
    text += &artefact("b", "m/b.bin", SHA_B, 1, "\"ci\"");
    text += &artefact("c", "m/c.bin", SHA_C, 1, "\"ci\"");
    text += &artefact("helper", "helper/h.onnx", SHA_A, 1, "\"ci\"");
    text += &artefact("llm", "llm/s.gguf", SHA_A, 1, "\"summary\"");
    text += extra_artefact;
    text += "[[model]]\nname = \"one\"\nengine = \"whisper\"\nartefacts = [\"a\", \"b\"]\n\
             languages = [\"en\"]\ndetects_language = false\n\n";
    text += "[[model]]\nname = \"two\"\nengine = \"parakeet-tdt\"\nartefacts = [\"b\", \"c\"]\n\
             languages = [\"en\", \"de\"]\ndetects_language = true\n\n";
    text += extra_model;
    text += "[[ladder]]\nbackend = \"cpu\"\nscope = \"english\"\nmodels = [\"one\", \"two\"]\n\
             source = \"test\"\n";
    text
}

fn small() -> Catalogue {
    Catalogue::parse(&manifest("", "")).unwrap()
}

#[test]
fn catalogue_small_manifest_lists_models_sizes_and_ladders() {
    let c = small();
    let names: Vec<&str> = c.models().iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["one", "two", "llm"]);
    assert_eq!(c.model("llm").unwrap().kind, ModelKind::Summary);
    assert_eq!(c.download_size("one").unwrap(), 2);
    assert_eq!(
        c.download_size("helper"),
        Err(CatalogueError::UnknownModel("helper".into()))
    );
    let ladder = c.ladder(BackendKind::Cpu, Scope::English).unwrap();
    assert_eq!(ladder.models, ["one", "two"]);
    assert_eq!(c.ladder(BackendKind::Cpu, Scope::Multilingual), None);
}

const MANIFEST: &str = "Manifest";
const PATH: &str = "Path";

/// The variant of a parse error, for tests that pin it.
fn error_kind(e: &CatalogueError) -> &'static str {
    match e {
        CatalogueError::Manifest(_) => MANIFEST,
        CatalogueError::Path { .. } => PATH,
        CatalogueError::UnknownModel(_) => "UnknownModel",
        CatalogueError::Chosen(_) => "Chosen",
        CatalogueError::Io { .. } => "Io",
    }
}

#[test]
fn catalogue_broken_manifests_are_errors() {
    let artefact = |path: &str, url: &str, sha: &str, location: &str| {
        format!(
            "[[artefact]]\nid = \"x\"\nsets = [\"ci\"]\nlocation = \"{location}\"\n\
             url = \"{url}\"\npath = \"{path}\"\nsha256 = \"{sha}\"\nsize = 1\n\
             licence = \"MIT\"\nattribution = \"t\"\n\n"
        )
    };
    let ok_url = "https://example.org/x";
    let cases = [
        // Paths that leave the models directory.
        (artefact("../x", ok_url, SHA_A, "cache"), String::new()),
        (artefact("C:/x", ok_url, SHA_A, "cache"), String::new()),
        (artefact("/x", ok_url, SHA_A, "cache"), String::new()),
        // Plain http, a bad digest, a committed file.
        (
            artefact("x", "http://example.org/x", SHA_A, "cache"),
            String::new(),
        ),
        (artefact("x", ok_url, "ABC", "cache"), String::new()),
        (artefact("x", ok_url, SHA_A, "repo"), String::new()),
        // A model naming a missing artefact, an unknown engine.
        (
            String::new(),
            "[[model]]\nname = \"m\"\nengine = \"whisper\"\nartefacts = [\"nope\"]\n\
             languages = [\"en\"]\ndetects_language = false\n\n"
                .to_owned(),
        ),
        (
            String::new(),
            "[[model]]\nname = \"m\"\nengine = \"kaldi\"\nartefacts = [\"a\"]\n\
             languages = [\"en\"]\ndetects_language = false\n\n"
                .to_owned(),
        ),
        // A repeated model name.
        (
            String::new(),
            "[[model]]\nname = \"one\"\nengine = \"whisper\"\nartefacts = [\"a\"]\n\
             languages = [\"en\"]\ndetects_language = false\n\n"
                .to_owned(),
        ),
    ];
    // The variant each case gives, in order: a path that leaves the models directory is
    // `Path`, every other broken rule `Manifest`.
    let kinds = [
        PATH, PATH, PATH, MANIFEST, MANIFEST, MANIFEST, MANIFEST, MANIFEST, MANIFEST,
    ];
    assert_eq!(kinds.len(), cases.len());
    for ((extra_artefact, extra_model), want) in cases.iter().zip(kinds) {
        let got = Catalogue::parse(&manifest(extra_artefact, extra_model)).err();
        assert_eq!(
            got.as_ref().map(error_kind),
            Some(want),
            "{got:?} for: {extra_artefact}{extra_model}"
        );
    }
    // An unknown field (the xtask parser refuses it too).
    let unknown = manifest("", "").replace("sets = [\"ci\", \"summary\"]", "colour = 1\nsets = []");
    assert!(matches!(
        Catalogue::parse(&unknown),
        Err(CatalogueError::Manifest(_))
    ));
    // A ladder naming an unknown model.
    let ladder = manifest("", "").replace("models = [\"one\", \"two\"]", "models = [\"three\"]");
    assert_eq!(
        Catalogue::parse(&ladder).err().as_ref().map(error_kind),
        Some(MANIFEST)
    );
    // A model that is an archive.
    let zip = |into: &str| {
        format!(
            "[[artefact]]\nid = \"z\"\nsets = [\"ci\"]\nlocation = \"cache\"\n\
             url = \"https://example.org/z\"\npath = \"z.zip\"\nsha256 = \"{SHA_A}\"\n\
             size = 1\nlicence = \"MIT\"\nattribution = \"t\"\n\n[artefact.extract]\n\
             format = \"zip\"\ninto = \"{into}\"\n\n"
        )
    };
    assert!(Catalogue::parse(&manifest(&zip("z"), "")).is_ok());
    let archive = manifest(
        &zip("z"),
        "[[model]]\nname = \"zipped\"\nengine = \"whisper\"\nartefacts = [\"z\"]\n\
         languages = [\"en\"]\ndetects_language = false\n\n",
    );
    assert_eq!(
        Catalogue::parse(&archive).err().as_ref().map(error_kind),
        Some(MANIFEST)
    );
    // An extraction directory outside the models directory.
    assert!(matches!(
        Catalogue::parse(&manifest(&zip("../z"), "")),
        Err(CatalogueError::Path { .. })
    ));
}

fn put(models: &ModelDir, rel: &str, bytes: &[u8]) {
    let p = models.root().join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

#[test]
fn catalogue_install_state_comes_from_sizes() {
    let c = small();
    let dir = TempDir::new("catalogue");
    let models = ModelDir::new(dir.0.join("models"));
    let state = |name| c.install_state(&models, name).unwrap();
    let missing = state("one");
    assert_eq!(missing.state, InstallState::Missing);
    assert_eq!((missing.held, missing.total), (0, 2));
    assert!(!c.is_installed(&models, "one"));
    // A `.part` is a partial download.
    put(&models, "m/a.bin.part", b"");
    assert_eq!(
        state("one").state,
        InstallState::Missing,
        "an empty .part holds nothing"
    );
    put(&models, "m/a.bin.part", b"x");
    let partial = state("one");
    assert_eq!(partial.state, InstallState::Partial);
    assert_eq!(partial.held, 1);
    assert_eq!(partial.files[0], ("a".to_owned(), FileState::Partial(1)));
    // A file of another size holds nothing and is reported.
    put(&models, "m/b.bin", b"xx");
    let wrong = state("one");
    assert_eq!(wrong.files[1], ("b".to_owned(), FileState::WrongSize(2)));
    assert_eq!(wrong.held, 1);
    // Complete files: installed (a stale `.part` beside a complete file is ignored).
    put(&models, "m/a.bin", b"a");
    put(&models, "m/b.bin", b"b");
    let done = state("one");
    assert_eq!(done.state, InstallState::Installed);
    assert_eq!((done.held, done.total), (2, 2));
    assert!(c.is_installed(&models, "one"));
    // `two` shares `b`: partly there.
    assert_eq!(state("two").state, InstallState::Partial);
    assert!(matches!(
        c.install_state(&models, "nope"),
        Err(CatalogueError::UnknownModel(_))
    ));
}

#[test]
fn catalogue_verify_reads_every_byte_only_when_asked() {
    let c = small();
    let dir = TempDir::new("catalogue");
    let models = ModelDir::new(dir.0.join("models"));
    let problems = c.verify(&models, "one").unwrap();
    assert_eq!(problems.len(), 2);
    assert!(problems.iter().all(|p| p.what == "missing"));
    // Right sizes, one wrong byte: installed by size, refused by the digest.
    put(&models, "m/a.bin", b"a");
    put(&models, "m/b.bin", b"z");
    assert!(c.is_installed(&models, "one"));
    let problems = c.verify(&models, "one").unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].artefact, "b");
    assert!(
        problems[0].what.starts_with("sha256 "),
        "{}",
        problems[0].what
    );
    put(&models, "m/b.bin", b"b");
    assert_eq!(c.verify(&models, "one").unwrap(), []);
    put(&models, "m/b.bin", b"bb");
    assert!(
        c.verify(&models, "one").unwrap()[0]
            .what
            .contains("2 bytes")
    );
}

#[test]
fn catalogue_remove_frees_a_models_files_but_never_a_chosen_or_shared_one() {
    let c = small();
    let dir = TempDir::new("catalogue");
    let models = ModelDir::new(dir.0.join("models"));
    for (rel, bytes) in [
        ("m/a.bin", b"a"),
        ("m/b.bin", b"b"),
        ("m/c.bin", b"c"),
        ("helper/h.onnx", b"a"),
    ] {
        put(&models, rel, bytes);
    }
    put(&models, "m/c.bin.part", b"cc");
    assert!(c.is_installed(&models, "one") && c.is_installed(&models, "two"));
    // The chosen model is refused, and nothing is removed.
    assert_eq!(
        c.remove(&models, "one", &["one", "llm"]),
        Err(CatalogueError::Chosen("one".into()))
    );
    assert!(c.is_installed(&models, "one"));
    assert!(matches!(
        c.remove(&models, "helper", &[]),
        Err(CatalogueError::UnknownModel(_))
    ));
    // `two` shares `b` with the installed `one`: only `c` and its `.part` go.
    let removed = c.remove(&models, "two", &["one"]).unwrap();
    assert_eq!(removed.bytes_freed, 3);
    assert_eq!(removed.kept_shared, ["b"]);
    assert_eq!(removed.files.len(), 2);
    assert!(c.is_installed(&models, "one"));
    assert_eq!(
        c.install_state(&models, "two").unwrap().state,
        InstallState::Partial
    );
    // With `two` no longer installed, `one` frees both its files; the helper stays.
    let removed = c.remove(&models, "one", &["two"]).unwrap();
    assert_eq!(removed.bytes_freed, 2);
    assert!(removed.kept_shared.is_empty());
    assert!(models.root().join("helper/h.onnx").is_file());
    assert_eq!(
        c.install_state(&models, "one").unwrap().state,
        InstallState::Missing
    );
    // Removing again frees nothing.
    assert_eq!(c.remove(&models, "one", &[]).unwrap(), Removed::default());
}

#[test]
fn catalogue_remove_always_keeps_the_fallback_model() {
    let dir = TempDir::new("catalogue");
    let models = ModelDir::new(dir.0.join("models"));
    assert_eq!(
        catalogue().remove(&models, crate::host::FALLBACK_MODEL, &[]),
        Err(CatalogueError::Chosen(crate::host::FALLBACK_MODEL.into()))
    );
}

#[test]
fn catalogue_download_fetches_each_artefact_in_order_with_progress_over_the_model() {
    let c = small();
    let dir = TempDir::new("catalogue-dl");
    let models = ModelDir::new(dir.0.join("models"));
    let mut seen: Vec<std::path::PathBuf> = Vec::new();
    let mut steps: Vec<(u64, u64)> = Vec::new();
    let mut fetch = |url: &Url,
                     dest: &Path,
                     exp: &Expected<'_>,
                     _: &CancelToken,
                     p: &mut dyn FnMut(u64, u64)|
     -> Result<Outcome, DownloadError> {
        assert!(url.as_str().starts_with("https://example.org/"));
        seen.push(dest.to_path_buf());
        p(exp.size, exp.size);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest, vec![b'x'; usize::try_from(exp.size).unwrap()]).unwrap();
        Ok(Outcome::Downloaded { resumed_from: 0 })
    };
    c.download(
        &models,
        "two",
        &CancelToken::new(),
        &mut |d, t| steps.push((d, t)),
        &mut fetch,
    )
    .unwrap();
    assert!(matches!(
        c.download(
            &models,
            "helper",
            &CancelToken::new(),
            &mut |_, _| {},
            &mut fetch
        ),
        Err(ModelDownloadError::Catalogue(CatalogueError::UnknownModel(
            _
        )))
    ));
    let want = [
        models.root().join("m").join("b.bin"),
        models.root().join("m").join("c.bin"),
    ];
    assert_eq!(seen, want);
    assert_eq!(steps.first(), Some(&(0, 2)));
    assert_eq!(steps.last(), Some(&(2, 2)));
    assert!(steps.windows(2).all(|w| w[0].0 <= w[1].0), "{steps:?}");
    assert!(steps.iter().all(|(_, t)| *t == 2));
}

#[test]
fn catalogue_download_stops_at_a_cancel_and_keeps_what_finished() {
    let c = small();
    let dir = TempDir::new("catalogue-dl");
    let models = ModelDir::new(dir.0.join("models"));
    let mut calls = 0;
    let mut fetch = |_: &Url,
                     dest: &Path,
                     _: &Expected<'_>,
                     _: &CancelToken,
                     _: &mut dyn FnMut(u64, u64)|
     -> Result<Outcome, DownloadError> {
        calls += 1;
        if calls == 1 {
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(dest, b"a").unwrap();
            Ok(Outcome::Downloaded { resumed_from: 0 })
        } else {
            Err(DownloadError::Cancelled { done: 0, total: 1 })
        }
    };
    let e = c
        .download(
            &models,
            "one",
            &CancelToken::new(),
            &mut |_, _| {},
            &mut fetch,
        )
        .unwrap_err();
    assert!(e.cancelled());
    assert!(e.retryable());
    assert!(matches!(&e, ModelDownloadError::Download { artefact, .. } if artefact == "b"));
    assert_eq!(calls, 2);
    assert!(models.root().join("m/a.bin").is_file());
    let failed = ModelDownloadError::Download {
        artefact: "b".into(),
        error: DownloadError::Setup("no".into()),
    };
    assert!(!failed.cancelled() && !failed.retryable());
}

#[test]
fn catalogue_display_names_cover_every_model_and_nothing_else() {
    let mut ids: Vec<&str> = catalogue()
        .models()
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    ids.sort_unstable();
    let mut named: Vec<&str> = DISPLAY_NAMES.iter().map(|(id, _)| *id).collect();
    named.sort_unstable();
    assert_eq!(named, ids);
    for (id, name) in DISPLAY_NAMES {
        assert_eq!(display_name(id), *name);
        assert_ne!(id, name, "{id} needs a name of its own");
        assert_eq!(manifest_id_in(name), None, "{name} must not read as an id");
    }
    // An id this version does not know is shown as it is.
    assert_eq!(display_name("some-future-model"), "some-future-model");
}

#[test]
fn catalogue_manifest_id_in_finds_ids_outside_display_names_only() {
    assert_eq!(
        manifest_id_in("Whisper base.en and Whisper large-v3-turbo (compact)"),
        None
    );
    assert_eq!(manifest_id_in("base.en is too slow"), Some("base.en"));
    assert_eq!(
        manifest_id_in("Qwen 3.5 9B, then qwen3.5-4b-q4_k_m"),
        Some("qwen3.5-4b-q4_k_m")
    );
    assert_eq!(
        manifest_id_in(&CatalogueError::Chosen("parakeet-tdt-0.6b-v2".into()).to_string()),
        None
    );
}

/// The file `rel` (relative to the models directory) in the artefacts of a helpers `set`.
fn helper_provides(c: &Catalogue, set: &str, rel: &str) -> bool {
    c.artefacts()
        .iter()
        .filter(|a| a.sets.iter().any(|s| s == set))
        .any(|a| match &a.extract {
            None => a.path == rel,
            Some(x) => x
                .members
                .iter()
                .any(|m| format!("{}/{}", x.into, m.path) == rel),
        })
}

#[test]
fn catalogue_helpers_hold_what_a_meeting_loads_on_each_architecture() {
    let c = catalogue();
    for (set, library, sha) in [
        (
            HELPERS_SET_X64,
            earshot_onnx::ORT_CPU_DYLIB_X64,
            earshot_onnx::ORT_CPU_DYLIB_X64_SHA256,
        ),
        (
            HELPERS_SET_ARM64,
            earshot_onnx::ORT_CPU_DYLIB_ARM64,
            earshot_onnx::ORT_CPU_DYLIB_ARM64_SHA256,
        ),
    ] {
        for rel in [SILERO, SEGMENTATION, CAMPPLUS, library] {
            assert!(helper_provides(c, set, rel), "{set} lacks {rel}");
        }
        let files: Vec<&Artefact> = c
            .artefacts()
            .iter()
            .filter(|a| a.sets.iter().any(|s| s == set))
            .collect();
        assert_eq!(files.len(), 4, "{set}");
        // The DLL `earshot_onnx::init` checks is the one the manifest pins.
        let pinned = files.iter().find_map(|a| {
            let x = a.extract.as_ref()?;
            x.members
                .iter()
                .find(|m| format!("{}/{}", x.into, m.path) == library)
        });
        assert_eq!(pinned.unwrap().sha256, sha, "{set}");
        for a in &files {
            // Only that architecture's CPU build: the CUDA and QNN libraries stay out
            // (T8.4a, T8.4).
            for word in ["cuda", "qnn", "gpu"] {
                assert!(!a.id.contains(word), "{} in {set}", a.id);
            }
            if let Some(x) = &a.extract {
                assert!(archive::Format::parse(&x.format).is_some(), "{}", a.id);
                for m in &x.members {
                    let name = m.path.to_lowercase();
                    assert!(!name.contains("cuda") && !name.contains("qnn"), "{name}");
                }
            }
        }
    }
    assert!(!helper_provides(
        c,
        HELPERS_SET_X64,
        earshot_onnx::ORT_CUDA_DYLIB
    ));
    // This build's item is its architecture's set, and the helpers are not a choice.
    let model = c.model(HELPERS_MODEL).unwrap();
    assert_eq!(model.kind, ModelKind::Helpers);
    assert_eq!(model.engine, Engine::Helpers);
    assert!(helper_provides(c, HELPERS_SET, earshot_onnx::ORT_CPU_DYLIB));
    let total: u64 = c
        .artefacts()
        .iter()
        .filter(|a| a.sets.iter().any(|s| s == HELPERS_SET))
        .map(|a| a.size)
        .sum();
    assert_eq!(c.download_size(HELPERS_MODEL).unwrap(), total);
}

#[test]
fn catalogue_helpers_are_named_for_the_user_and_their_files_never_are() {
    assert_eq!(display_name(HELPERS_MODEL), "Speech helpers");
    assert_eq!(manifest_id_in("Speech helpers are ready"), None);
    assert_eq!(manifest_id_in("speech-helpers failed"), Some(HELPERS_MODEL));
    assert_eq!(
        manifest_id_in("onnxruntime-win-x64 failed"),
        Some("onnxruntime-win-x64")
    );
    assert_eq!(manifest_id_in("the silero-vad file"), Some("silero-vad"));
}

#[test]
fn catalogue_helpers_are_never_removed_whatever_the_caller_protects() {
    let dir = TempDir::new("catalogue-helpers");
    let models = ModelDir::new(dir.0.join("models"));
    put(&models, SILERO, b"x");
    assert_eq!(
        catalogue().remove(&models, HELPERS_MODEL, &[]),
        Err(CatalogueError::Chosen(HELPERS_MODEL.into()))
    );
    assert!(models.root().join(SILERO).is_file());
}

/// A manifest of this build's helpers with small files: `silero` plain, `seg` a tar.bz2 and
/// `ort` a zip, as the real ones are. The sizes and digests are of the bytes given.
struct SmallHelpers {
    catalogue: Catalogue,
    files: Vec<(&'static str, Vec<u8>)>,
}

fn sha_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut s = String::new();
    for b in Sha256::digest(bytes) {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
    let mut crc = flate2::Crc::new();
    crc.update(data);
    let (sum, len, n) = (
        crc.sum(),
        u32::try_from(data.len()).unwrap(),
        u16::try_from(name.len()).unwrap(),
    );
    let mut out = Vec::new();
    out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&sum.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(data);
    let cd_at = u32::try_from(out.len()).unwrap();
    let mut cd = Vec::new();
    cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    cd.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    cd.extend_from_slice(&sum.to_le_bytes());
    cd.extend_from_slice(&len.to_le_bytes());
    cd.extend_from_slice(&len.to_le_bytes());
    cd.extend_from_slice(&n.to_le_bytes());
    cd.extend_from_slice(&[0; 16]);
    cd.extend_from_slice(name.as_bytes());
    let cd_len = u32::try_from(cd.len()).unwrap();
    out.extend_from_slice(&cd);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&[1, 0, 1, 0]);
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_at.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

fn tar_bz2_of(name: &str, data: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut h = tar::Header::new_gnu();
    h.set_path(name).unwrap();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_cksum();
    let mut b = tar::Builder::new(Vec::new());
    b.append(&h, data).unwrap();
    let raw = b.into_inner().unwrap();
    let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    e.write_all(&raw).unwrap();
    e.finish().unwrap()
}

fn small_helpers() -> SmallHelpers {
    let silero = b"silero bytes".to_vec();
    let seg_member = b"segmentation bytes".to_vec();
    let ort_member = b"library bytes".to_vec();
    let seg = tar_bz2_of("seg/model.onnx", &seg_member);
    let ort = stored_zip("ort/lib/library.dll", &ort_member);
    let head = |id: &str, path: &str, bytes: &[u8]| {
        let (sha, size) = (sha_of(bytes), bytes.len());
        format!(
            "[[artefact]]\nid = \"{id}\"\nsets = [\"{HELPERS_SET}\"]\nlocation = \"cache\"\n\
             url = \"https://example.org/{id}\"\npath = \"{path}\"\nsha256 = \"{sha}\"\n\
             size = {size}\nlicence = \"MIT\"\nattribution = \"test\"\n\n"
        )
    };
    let extract = |format: &str, into: &str, member: &str, bytes: &[u8]| {
        let (sha, size) = (sha_of(bytes), bytes.len());
        format!(
            "[artefact.extract]\nformat = \"{format}\"\ninto = \"{into}\"\n\n\
             [[artefact.extract.member]]\npath = \"{member}\"\nsha256 = \"{sha}\"\nsize = {size}\n\n"
        )
    };
    let mut text = format!("sets = [\"{HELPERS_SET}\"]\nhosts = [\"example.org\"]\n\n");
    text += &head("silero", "silero/silero.onnx", &silero);
    text += &head("seg", "archives/seg.tar.bz2", &seg);
    text += &extract("tar.bz2", "seg-dir", "seg/model.onnx", &seg_member);
    text += &head("ort", "archives/ort.zip", &ort);
    text += &extract("zip", "ort-dir", "ort/lib/library.dll", &ort_member);
    SmallHelpers {
        catalogue: Catalogue::parse(&text).unwrap(),
        files: vec![("silero", silero), ("seg", seg), ("ort", ort)],
    }
}

/// What the fetch of `small_helpers` does: "downloads" the artefact the URL's last segment
/// names from `files` and records it; a real fetch would find a plain file already in place.
fn fake_fetch(
    files: &[(&'static str, Vec<u8>)],
    asked: &std::cell::RefCell<Vec<String>>,
    url: &Url,
    dest: &Path,
) -> Result<Outcome, DownloadError> {
    let id = url.as_str().rsplit('/').next().unwrap().to_owned();
    asked.borrow_mut().push(id.clone());
    let bytes = &files.iter().find(|(f, _)| *f == id).unwrap().1;
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(dest, bytes).unwrap();
    Ok(Outcome::Downloaded { resumed_from: 0 })
}

#[test]
fn catalogue_helpers_download_extract_delete_the_archives_and_read_as_installed() {
    let h = small_helpers();
    let dir = TempDir::new("catalogue-helpers");
    let models = ModelDir::new(dir.0.join("models"));
    let c = &h.catalogue;
    assert_eq!(c.model(HELPERS_MODEL).unwrap().artefacts.len(), 3);
    assert_eq!(
        c.install_state(&models, HELPERS_MODEL).unwrap().state,
        InstallState::Missing
    );
    assert!(!c.verify(&models, HELPERS_MODEL).unwrap().is_empty());

    let asked = std::cell::RefCell::new(Vec::new());
    let mut fetch =
        |url: &Url, dest: &Path, _: &Expected<'_>, _: &CancelToken, _: &mut dyn FnMut(u64, u64)| {
            fake_fetch(&h.files, &asked, url, dest)
        };
    let mut steps = Vec::new();
    c.download(
        &models,
        HELPERS_MODEL,
        &CancelToken::new(),
        &mut |d, t| steps.push((d, t)),
        &mut fetch,
    )
    .unwrap();
    let total = c.download_size(HELPERS_MODEL).unwrap();
    assert_eq!(steps.last(), Some(&(total, total)));
    assert!(steps.windows(2).all(|w| w[0].0 <= w[1].0), "{steps:?}");

    assert_eq!(c.verify(&models, HELPERS_MODEL).unwrap(), []);
    let state = c.install_state(&models, HELPERS_MODEL).unwrap();
    assert_eq!(state.state, InstallState::Installed);
    assert_eq!((state.held, state.total), (total, total));
    let root = models.root();
    assert!(root.join("seg-dir/seg/model.onnx").is_file());
    assert!(root.join("ort-dir/ort/lib/library.dll").is_file());
    assert!(root.join("ort-dir/.earshot-extracted").is_file());
    assert!(!root.join("archives/ort.zip").exists(), "the archive goes");
    assert!(!root.join("archives/seg.tar.bz2").exists());

    // Installed: a second run asks for no archive again.
    asked.borrow_mut().clear();
    c.download(
        &models,
        HELPERS_MODEL,
        &CancelToken::new(),
        &mut |_, _| {},
        &mut fetch,
    )
    .unwrap();
    assert_eq!(
        *asked.borrow(),
        ["silero"],
        "a plain file is the fetch's to check"
    );
}

#[test]
fn catalogue_helpers_are_not_installed_until_extracted_and_a_damaged_member_is_downloaded_again() {
    let h = small_helpers();
    let dir = TempDir::new("catalogue-helpers");
    let models = ModelDir::new(dir.0.join("models"));
    let c = &h.catalogue;
    let ort_zip = &h.files[2].1;
    // A whole archive that was never extracted holds its bytes but is not installed.
    put(&models, "archives/ort.zip", ort_zip);
    let state = c.install_state(&models, HELPERS_MODEL).unwrap();
    assert_eq!(state.state, InstallState::Partial);
    assert_eq!(state.held, ort_zip.len() as u64);
    assert_eq!(
        state.files[2],
        ("ort".to_owned(), FileState::Partial(ort_zip.len() as u64))
    );

    let asked = std::cell::RefCell::new(Vec::new());
    let mut fetch =
        |url: &Url, dest: &Path, _: &Expected<'_>, _: &CancelToken, _: &mut dyn FnMut(u64, u64)| {
            fake_fetch(&h.files, &asked, url, dest)
        };
    c.download(
        &models,
        HELPERS_MODEL,
        &CancelToken::new(),
        &mut |_, _| {},
        &mut fetch,
    )
    .unwrap();
    assert_eq!(c.verify(&models, HELPERS_MODEL).unwrap(), []);

    // A member of the right size with other bytes: installed by size, refused by the digest,
    // and the next download repairs it.
    let member = "ort-dir/ort/lib/library.dll";
    let size = std::fs::metadata(models.root().join(member)).unwrap().len();
    put(&models, member, &vec![b'!'; usize::try_from(size).unwrap()]);
    assert!(c.is_installed(&models, HELPERS_MODEL));
    let problems = c.verify(&models, HELPERS_MODEL).unwrap();
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].artefact, "ort");
    assert!(problems[0].what.contains("sha256"), "{}", problems[0].what);
    c.download(
        &models,
        HELPERS_MODEL,
        &CancelToken::new(),
        &mut |_, _| {},
        &mut fetch,
    )
    .unwrap();
    assert_eq!(c.verify(&models, HELPERS_MODEL).unwrap(), []);
}

/// A crash between `publish`'s two renames: the start, or the next download, puts the good copy
/// back with no request.
#[test]
fn catalogue_helpers_a_crash_while_replacing_them_recovers_without_a_download() {
    let h = small_helpers();
    let dir = TempDir::new("catalogue-helpers");
    let models = ModelDir::new(dir.0.join("models"));
    let asked = std::cell::RefCell::new(Vec::new());
    let mut fetch =
        |url: &Url, dest: &Path, _: &Expected<'_>, _: &CancelToken, _: &mut dyn FnMut(u64, u64)| {
            fake_fetch(&h.files, &asked, url, dest)
        };
    let download = |fetch: &mut FetchFn<'_>| {
        h.catalogue
            .download(
                &models,
                HELPERS_MODEL,
                &CancelToken::new(),
                &mut |_, _| {},
                fetch,
            )
            .unwrap();
    };
    download(&mut fetch);
    let (into, aside) = (
        models.root().join("ort-dir"),
        models.root().join("ort-dir.old"),
    );
    let installed = || h.catalogue.is_installed(&models, HELPERS_MODEL);
    assert!(installed());

    // At start.
    std::fs::rename(&into, &aside).unwrap();
    assert!(!installed());
    assert_eq!(h.catalogue.recover_extractions(&models), 1);
    assert!(installed());
    assert!(!aside.exists());
    assert_eq!(h.catalogue.recover_extractions(&models), 0, "nothing left");

    // At the next download, which then asks for nothing that was already there.
    std::fs::rename(&into, &aside).unwrap();
    asked.borrow_mut().clear();
    let mut fetch =
        |url: &Url, dest: &Path, _: &Expected<'_>, _: &CancelToken, _: &mut dyn FnMut(u64, u64)| {
            fake_fetch(&h.files, &asked, url, dest)
        };
    download(&mut fetch);
    assert!(!asked.borrow().iter().any(|id| id == "ort"), "{asked:?}");
    assert!(!aside.exists());
    assert!(installed());
}

/// The validator's list is what extraction really uses.
#[test]
fn catalogue_extract_cache_paths_are_where_archive_writes() {
    let root = Path::new("models");
    let into = jail_join(root, "ort-dir").unwrap();
    let joined = extract_cache_paths("ort-dir").map(|p| jail_join(root, &p).unwrap());
    assert_eq!(
        joined,
        [
            into.clone(),
            archive::tmp_dir(&into),
            archive::aside_dir(&into)
        ]
    );
}

#[test]
fn catalogue_helpers_a_refused_archive_stops_the_download_and_is_not_retryable() {
    let mut h = small_helpers();
    // Not the bytes the member pin expects: the extraction refuses it.
    h.files[2].1 = stored_zip("ort/lib/library.dll", b"other bytes!!");
    let dir = TempDir::new("catalogue-helpers");
    let models = ModelDir::new(dir.0.join("models"));
    let asked = std::cell::RefCell::new(Vec::new());
    let mut fetch =
        |url: &Url, dest: &Path, _: &Expected<'_>, _: &CancelToken, _: &mut dyn FnMut(u64, u64)| {
            fake_fetch(&h.files, &asked, url, dest)
        };
    let e = h
        .catalogue
        .download(
            &models,
            HELPERS_MODEL,
            &CancelToken::new(),
            &mut |_, _| {},
            &mut fetch,
        )
        .unwrap_err();
    assert!(matches!(&e, ModelDownloadError::Extract { artefact, .. } if artefact == "ort"));
    assert!(!e.retryable() && !e.cancelled());
    assert_ne!(
        h.catalogue
            .install_state(&models, HELPERS_MODEL)
            .unwrap()
            .state,
        InstallState::Installed
    );
    assert!(!models.root().join("ort-dir").exists());
}

#[test]
fn catalogue_helpers_cancelled_before_the_extraction_keep_the_archive_for_the_next_run() {
    let h = small_helpers();
    let dir = TempDir::new("catalogue-helpers");
    let models = ModelDir::new(dir.0.join("models"));
    let cancel = CancelToken::new();
    cancel.cancel();
    let asked = std::cell::RefCell::new(Vec::new());
    let mut fetch =
        |url: &Url, dest: &Path, _: &Expected<'_>, _: &CancelToken, _: &mut dyn FnMut(u64, u64)| {
            fake_fetch(&h.files, &asked, url, dest)
        };
    let e = h
        .catalogue
        .download(&models, HELPERS_MODEL, &cancel, &mut |_, _| {}, &mut fetch)
        .unwrap_err();
    assert!(e.cancelled() && e.retryable());
    assert!(models.root().join("archives/seg.tar.bz2").is_file());
    assert!(!models.root().join("seg-dir").exists());
}

#[test]
fn catalogue_refuses_a_helper_archive_it_cannot_extract() {
    let make = |extract: &str| {
        format!(
            "sets = [\"{HELPERS_SET}\"]\nhosts = [\"example.org\"]\n\n\
             [[artefact]]\nid = \"h\"\nsets = [\"{HELPERS_SET}\"]\nlocation = \"cache\"\n\
             url = \"https://example.org/h\"\npath = \"h.zip\"\nsha256 = \"{SHA_A}\"\n\
             size = 1\nlicence = \"MIT\"\nattribution = \"t\"\n\n{extract}"
        )
    };
    let member =
        format!("[[artefact.extract.member]]\npath = \"a\"\nsha256 = \"{SHA_A}\"\nsize = 1\n");
    let ok = format!("[artefact.extract]\nformat = \"zip\"\ninto = \"h\"\n\n{member}");
    assert!(Catalogue::parse(&make(&ok)).is_ok());
    for bad in [
        // Nothing pinned: nothing would be extracted.
        "[artefact.extract]\nformat = \"zip\"\ninto = \"h\"\n".to_owned(),
        // xtask reads tar.gz; the app has no helper in it and cannot extract it.
        format!("[artefact.extract]\nformat = \"tar.gz\"\ninto = \"h\"\n\n{member}"),
    ] {
        assert!(
            matches!(
                Catalogue::parse(&make(&bad)),
                Err(CatalogueError::Manifest(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn catalogue_model_of_file_tells_a_summary_model_from_a_helper_and_a_transcription_model() {
    let c = catalogue();
    let root = Path::new("C:\\some\\models");
    let kind = |rel: &str| c.model_of_file(&root.join(rel)).map(|m| m.kind);
    assert_eq!(kind(SILERO), Some(ModelKind::Helpers));
    assert_eq!(kind(SEGMENTATION), Some(ModelKind::Helpers));
    assert_eq!(kind(CAMPPLUS), Some(ModelKind::Helpers));
    assert_eq!(kind(earshot_onnx::ORT_CPU_DYLIB), Some(ModelKind::Helpers));
    assert_eq!(
        kind("whisper/ggml-base.en.bin"),
        Some(ModelKind::Transcription)
    );
    let summary = SUMMARY_MODELS[0];
    assert_eq!(kind(summary.file), Some(ModelKind::Summary));
    assert_eq!(
        c.model_of_file(&root.join(summary.file)).unwrap().name,
        summary.id
    );
    // Files no offered model holds.
    assert_eq!(kind(earshot_onnx::ORT_CUDA_DYLIB), None);
    assert_eq!(kind("parakeet-qnn/parakeet-tdt-110m/encoder.onnx"), None);
    assert_eq!(kind("somewhere/else.bin"), None);
}
