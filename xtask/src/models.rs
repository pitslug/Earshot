//! `cargo xtask fetch-models`: downloads the models in `models/manifest.toml` into
//! `EARSHOT_MODELS` (default `%LOCALAPPDATA%\Earshot\models`), reusing the checksum-verified
//! downloader of the fixture pipeline (PLAN.md 3.4 default models, 3.6 storage layout).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::manifest::{self, Artefact, Location, Manifest};

/// Environment variable overriding the model directory.
pub const ENV: &str = "EARSHOT_MODELS";
/// Default model directory below `%LOCALAPPDATA%`.
pub const DEFAULT_REL: &str = "Earshot\\models";

/// Model directory (`EARSHOT_MODELS` or `%LOCALAPPDATA%\Earshot\models`).
pub fn cache_dir() -> Result<PathBuf> {
    manifest::cache_dir(ENV, DEFAULT_REL)
}

/// `models/manifest.toml`.
pub fn manifest_path(repo: &Path) -> PathBuf {
    repo.join("models").join("manifest.toml")
}

/// The NPU summary spike's set (T12.7), kept in its own manifest.
pub const NPU_LLM_EVAL_SET: &str = "npu-llm-eval";
/// Its manifest: outside `models/manifest.toml`, whose hosts become the app's allowlist.
pub const NPU_LLM_EVAL_MANIFEST: &str = "xtask/npu-llm-eval.toml";

/// The manifest that holds `sets`: [`NPU_LLM_EVAL_MANIFEST`] for exactly that set, else
/// `models/manifest.toml`.
fn manifest_for(repo: &Path, sets: &[String]) -> PathBuf {
    if sets == [NPU_LLM_EVAL_SET] {
        repo.join(NPU_LLM_EVAL_MANIFEST)
    } else {
        manifest_path(repo)
    }
}

/// `arts` narrowed to the artefacts named in `ids` (all of `arts` when `ids` is empty).
///
/// # Errors
/// An id that is not among `arts` (unknown, or outside the selected sets).
pub fn only<'a>(arts: Vec<&'a Artefact>, ids: &[String]) -> Result<Vec<&'a Artefact>> {
    if ids.is_empty() {
        return Ok(arts);
    }
    for id in ids {
        anyhow::ensure!(
            arts.iter().any(|a| &a.id == id),
            "--model {id}: no such model in the selected sets"
        );
    }
    Ok(arts.into_iter().filter(|a| ids.contains(&a.id)).collect())
}

/// `cargo xtask fetch-models`: the artefacts of `sets`, or only those named in `ids`.
pub fn run(repo: &Path, sets: &[String], ids: &[String], verify: bool) -> Result<()> {
    let m = Manifest::load(&manifest_for(repo, sets))?;
    let arts = only(m.select(sets)?, ids)?;
    anyhow::ensure!(
        arts.iter().all(|a| a.location == Location::Cache),
        "models are never committed; every model artefact must have location = \"cache\""
    );
    let dir = cache_dir()?;
    let reports = manifest::fetch_all(&arts, repo, &dir, verify, &m.policy())?;
    manifest::print_reports("fetch-models", &dir, &reports);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn load() -> Manifest {
        Manifest::load(&manifest_path(&crate::repo_root())).unwrap()
    }

    fn ids(m: &Manifest, set: &str) -> BTreeSet<String> {
        m.select(&[set.to_string()])
            .unwrap()
            .iter()
            .map(|a| a.id.clone())
            .collect()
    }

    #[test]
    fn npu_llm_eval_manifest_stays_out_of_the_app() {
        let root = crate::repo_root();
        let eval = Manifest::load(&root.join(NPU_LLM_EVAL_MANIFEST)).unwrap();
        assert_eq!(eval.sets, [NPU_LLM_EVAL_SET]);
        assert_eq!(
            manifest_for(&root, &[NPU_LLM_EVAL_SET.to_string()]),
            root.join(NPU_LLM_EVAL_MANIFEST)
        );
        assert_eq!(
            manifest_for(&root, &[NPU_LLM_EVAL_SET.to_string(), "ci".to_string()]),
            manifest_path(&root)
        );
        let app = load();
        // The embedded manifest's hosts are the app's download allowlist: none of the
        // spike's hosts, and no artefact of its set, may appear there.
        for host in eval.hosts.iter().chain(&eval.redirect_hosts) {
            assert!(
                !app.hosts.contains(host) && !app.redirect_hosts.contains(host),
                "{host} is in models/manifest.toml"
            );
        }
        assert!(!app.sets.iter().any(|s| s == NPU_LLM_EVAL_SET));
        for a in &eval.artefacts {
            assert_eq!(a.sets, [NPU_LLM_EVAL_SET], "{}", a.id);
            assert_eq!(a.location, Location::Cache, "{}", a.id);
            assert!(a.extract.is_none(), "{}", a.id);
            // The Qualcomm AI Stack License (development use and benchmarking), as for the
            // `qnn` set's libraries, only in this evaluation set (the owner, 2026-11-11).
            assert!(
                ["Apache-2.0", "LicenseRef-Qualcomm-AI-Stack"].contains(&a.licence.as_str()),
                "{}: unexpected licence {}",
                a.id,
                a.licence
            );
            assert!(
                !app.artefacts.iter().any(|b| b.id == a.id),
                "{} is also in models/manifest.toml",
                a.id
            );
        }
    }

    #[test]
    fn models_only_narrows_to_named_ids_and_refuses_one_outside_the_sets() {
        let m = load();
        let eval = m.select(&["llm-eval".to_string()]).unwrap();
        let n = eval.len();
        assert_eq!(only(eval.clone(), &[]).unwrap().len(), n);
        let one = only(eval.clone(), &["qwen2.5-1.5b-instruct-q4_k_m".to_string()]).unwrap();
        assert_eq!(
            one.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["qwen2.5-1.5b-instruct-q4_k_m"]
        );
        // In the manifest but not in llm-eval, and not in the manifest at all.
        assert!(only(eval.clone(), &["whisper-base.en".to_string()]).is_err());
        assert!(only(eval, &["no-such-model".to_string()]).is_err());
    }

    #[test]
    fn models_manifest_has_the_planned_models_and_sets() {
        let m = load();
        assert_eq!(
            m.sets,
            [
                "ci",
                "desktop",
                "choice",
                "cuda",
                "eval",
                "arm64",
                "qnn",
                "qnn-x64",
                "llm-eval",
                "summary",
                "helpers-x64",
                "helpers-arm64",
                "cuda-libraries",
                "qnn-models"
            ]
        );
        // NVIDIA's libraries for the app's opt-in download (T8.4ae) join no other set: never ci
        // or desktop, whatever `--set all` fetches.
        assert!(!ids(&m, "cuda-libraries").is_empty());
        for a in m
            .artefacts
            .iter()
            .filter(|a| a.sets.iter().any(|s| s == "cuda-libraries"))
        {
            assert_eq!(a.sets, ["cuda-libraries"], "{}", a.id);
        }
        // What the app downloads as "Speech helpers" (T9.1f): no CUDA or QNN library.
        for (set, library) in [
            ("helpers-x64", "onnxruntime-win-x64"),
            ("helpers-arm64", "onnxruntime-win-arm64"),
        ] {
            let held: std::collections::BTreeSet<&str> = m
                .artefacts
                .iter()
                .filter(|a| a.sets.iter().any(|s| s == set))
                .map(|a| a.id.as_str())
                .collect();
            let want: std::collections::BTreeSet<&str> = [
                "silero-vad",
                "pyannote-segmentation-3.0",
                "campplus-en-voxceleb",
                library,
            ]
            .into_iter()
            .collect();
            assert_eq!(held, want, "{set}");
        }
        let all = ids(&m, "all");
        for id in [
            "whisper-tiny.en",
            "whisper-base.en",
            "whisper-small.en",
            "whisper-small.en-q5_1",
            "whisper-large-v3-turbo",
            "whisper-large-v3-turbo-q5_0",
            "silero-vad",
            "pyannote-segmentation-3.0",
            "campplus-en-voxceleb",
            "onnxruntime-win-x64",
            "onnxruntime-win-x64-gpu-cuda13",
            "parakeet-tdt_ctc-110m-ctc",
            "parakeet-tdt_ctc-110m-tokens",
            "onnx-asr-nemo128",
            "qwen2.5-1.5b-instruct-q4_k_m",
        ] {
            assert!(all.contains(id), "{id} missing");
        }
        // PLAN D22: ci holds only default models (the English ladders' first CUDA/CPU rungs
        // and base.en, which ci and accept m2 load) and the speech models.
        let parakeet_defaults = [
            "parakeet-tdt-110m-encoder",
            "parakeet-tdt-110m-decoder",
            "parakeet-tdt-110m-joiner",
            "parakeet-tdt-110m-tokens",
            "parakeet-tdt-0.6b-v2-encoder",
            "parakeet-tdt-0.6b-v2-encoder-weights",
            "parakeet-tdt-0.6b-v2-decoder",
            "parakeet-tdt-0.6b-v2-joiner",
            "parakeet-tdt-0.6b-v2-tokens",
        ];
        let ci = ids(&m, "ci");
        let mut want: BTreeSet<String> = [
            "whisper-base.en",
            "silero-vad",
            "pyannote-segmentation-3.0",
            "campplus-en-voxceleb",
            "onnxruntime-win-x64",
        ]
        .map(String::from)
        .into();
        want.extend(parakeet_defaults.map(String::from));
        assert_eq!(ci, want);
        // desktop: the defaults of every English ladder (small.en is the Vulkan default).
        let mut want_desktop = want.clone();
        want_desktop.insert("whisper-small.en".to_owned());
        assert_eq!(ids(&m, "desktop"), want_desktop);
        // choice: offered to the user, never tested.
        let choice: BTreeSet<String> = [
            "whisper-tiny.en",
            "whisper-small.en-q5_1",
            "whisper-large-v3-turbo",
            "whisper-large-v3-turbo-q5_0",
            "parakeet-tdt-0.6b-v3-encoder",
            "parakeet-tdt-0.6b-v3-encoder-weights",
            "parakeet-tdt-0.6b-v3-decoder",
            "parakeet-tdt-0.6b-v3-joiner",
            "parakeet-tdt-0.6b-v3-tokens",
        ]
        .map(String::from)
        .into();
        assert_eq!(ids(&m, "choice"), choice);
        let cuda = ids(&m, "cuda");
        let want: BTreeSet<String> = ["onnxruntime-win-x64-gpu-cuda13"].map(String::from).into();
        assert_eq!(cuda, want, "the GPU runtime is never part of ci");
        // Summary-evaluation candidates (T6.20) are only ever in llm-eval (the shipped ones
        // also in summary): never ci or desktop, so nothing ships from the evaluation set.
        let eval: BTreeSet<String> = [
            "qwen2.5-1.5b-instruct-q4_k_m",
            "qwen3-4b-q4_k_m",
            "qwen3-8b-q4_k_m",
            "qwen3-14b-q4_k_m",
            "qwen3-4b-instruct-2507-q4_k_m",
            "qwen3.5-4b-q4_k_m",
            "qwen3.5-9b-q4_k_m",
            "phi-4-mini-instruct-q4_k_m",
            "granite-4.2-8b-q4_k_m",
            "gemma-4-12b-it-qat-q4_0",
            "qwen3.5-27b-q4_k_m",
            "qwen3-30b-a3b-instruct-2507-q4_k_m",
            "qwen3.5-2b-q4_k_m",
            "granite-4.2-3b-q4_k_m",
            "ornith-1.5-9b-q4_k_m",
        ]
        .map(String::from)
        .into();
        assert_eq!(ids(&m, "llm-eval"), eval);
        for a in m
            .artefacts
            .iter()
            .filter(|a| a.sets.iter().any(|s| s == "llm-eval"))
        {
            assert!(
                a.sets.iter().all(|s| s == "llm-eval" || s == "summary"),
                "{}: an llm-eval model is in {:?}",
                a.id,
                a.sets
            );
            assert!(
                a.path.starts_with("llm/") && a.path.ends_with(".gguf"),
                "{}",
                a.path
            );
        }
        // The shipped local summary models (T6.20, T6.21b): both measured in llm-eval,
        // Apache-2.0, never ci.
        let summary = ids(&m, "summary");
        let want: BTreeSet<String> = ["qwen3.5-4b-q4_k_m", "qwen3.5-9b-q4_k_m"]
            .map(String::from)
            .into();
        assert_eq!(summary, want);
        for a in m
            .artefacts
            .iter()
            .filter(|a| a.sets.iter().any(|s| s == "summary"))
        {
            assert!(a.sets.iter().any(|s| s == "llm-eval"), "{}", a.id);
            assert_eq!(a.licence, "Apache-2.0", "{}", a.id);
        }
        let qnn = ids(&m, "qnn");
        let want: BTreeSet<String> = ["onnxruntime-win-arm64", "qnn-ep-win-arm64"]
            .map(String::from)
            .into();
        assert_eq!(
            qnn, want,
            "the Surface NPU needs the arm64 runtime and the plugin"
        );
        let qnn_x64 = ids(&m, "qnn-x64");
        let want: BTreeSet<String> = ["qnn-ep-win-x64"].map(String::from).into();
        assert_eq!(qnn_x64, want);
        for set in [
            "ci", "desktop", "choice", "cuda", "eval", "arm64", "llm-eval", "summary",
        ] {
            assert!(
                ids(&m, set).iter().all(|id| !id.starts_with("qnn-ep-")),
                "Qualcomm-licensed libraries never join the {set} set"
            );
        }
    }

    #[test]
    fn models_ci_and_desktop_hold_only_default_models() {
        // PLAN D22: the English ladders and the summary set, read from the manifest itself.
        let m = load();
        for set in ["ci", "desktop"] {
            assert_eq!(m.non_default_models(set), Vec::<String>::new(), "{set}");
        }
        // The rule is not vacuous: the user-choice set holds non-default models.
        assert!(!m.non_default_models("choice").is_empty());
    }

    /// One `[[artefact]]` table for the guard manifests.
    fn guard_artefact(id: &str, path: &str, sets: &str) -> String {
        let sha = "0".repeat(64);
        [
            "[[artefact]]".to_owned(),
            format!("id = \"{id}\""),
            format!("sets = [{sets}]"),
            "location = \"cache\"".to_owned(),
            format!("url = \"https://example.org/{id}\""),
            format!("path = \"{path}\""),
            format!("sha256 = \"{sha}\""),
            "size = 1".to_owned(),
            "licence = \"MIT\"".to_owned(),
            "attribution = \"x\"".to_owned(),
            String::new(),
        ]
        .join("\n")
            + "\n"
    }

    /// A manifest with two whisper models, `base` (artefact `w-base` in `base_sets`) on the
    /// English CPU ladder and `big` (`w-big` in `big_sets`) on the multilingual one only,
    /// plus the `extra` artefacts.
    fn guard_manifest(base_sets: &str, big_sets: &str, extra: &str) -> Manifest {
        let model = |name: &str, artefact: &str, languages: &str| {
            [
                "[[model]]".to_owned(),
                format!("name = \"{name}\""),
                "engine = \"whisper\"".to_owned(),
                format!("artefacts = [\"{artefact}\"]"),
                format!("languages = [{languages}]"),
                format!("detects_language = {}", languages.contains(',')),
                String::new(),
            ]
            .join("\n")
                + "\n"
        };
        let ladder = |scope: &str, name: &str| {
            [
                "[[ladder]]".to_owned(),
                "backend = \"cpu\"".to_owned(),
                format!("scope = \"{scope}\""),
                format!("models = [\"{name}\"]"),
                "source = \"x\"".to_owned(),
                String::new(),
            ]
            .join("\n")
                + "\n"
        };
        let text = [
            "sets = [\"ci\", \"desktop\", \"choice\", \"summary\"]\nhosts = [\"example.org\"]\n"
                .to_owned(),
            guard_artefact("w-base", "whisper/ggml-base.bin", base_sets),
            guard_artefact("w-big", "whisper/ggml-big.bin", big_sets),
            extra.to_owned(),
            model("base", "w-base", "\"en\""),
            model("big", "w-big", "\"en\", \"de\""),
            ladder("english", "base"),
            ladder("multilingual", "big"),
        ]
        .join("\n");
        Manifest::parse(&text).unwrap_or_else(|e| panic!("{e:#}\n{text}"))
    }

    #[test]
    fn models_default_guard_refuses_non_default_transcription_and_llm_models() {
        let ok = guard_manifest("\"ci\", \"desktop\"", "\"choice\"", "");
        assert!(ok.non_default_models("ci").is_empty());
        assert!(ok.non_default_models("desktop").is_empty());
        assert_eq!(ok.non_default_models("choice").len(), 1);
        // A model only on a multilingual ladder.
        let multi = guard_manifest("\"ci\"", "\"ci\", \"choice\"", "");
        let v = multi.non_default_models("ci");
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(
            v[0].contains("w-big") && v[0].contains("model big"),
            "{v:?}"
        );
        assert!(multi.non_default_models("desktop").is_empty());
        // A whisper file that no [[model]] names.
        let orphan = guard_manifest(
            "\"ci\"",
            "\"choice\"",
            &guard_artefact("w-x", "whisper/ggml-x.bin", "\"desktop\""),
        );
        let v = orphan.non_default_models("desktop");
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("w-x"), "{v:?}");
        // A Parakeet model file that no [[model]] names.
        let orphan = guard_manifest(
            "\"ci\"",
            "\"choice\"",
            &guard_artefact("p-x", "parakeet/x/encoder.onnx", "\"ci\""),
        );
        let v = orphan.non_default_models("ci");
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("p-x"), "{v:?}");
        // A local LLM outside summary, and one inside it.
        let llm = guard_manifest(
            "\"ci\"",
            "\"choice\"",
            &format!(
                "{}\n{}",
                guard_artefact("small-llm", "llm/small.gguf", "\"ci\""),
                guard_artefact(
                    "shipped-llm",
                    "llm/shipped.gguf",
                    "\"desktop\", \"summary\""
                )
            ),
        );
        let v = llm.non_default_models("ci");
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("small-llm"), "{v:?}");
        assert!(llm.non_default_models("desktop").is_empty());
        // A file under parakeet/ that is no [[model]] (a reference front end) is not judged.
        let front_end = guard_manifest(
            "\"ci\"",
            "\"choice\"",
            &guard_artefact("fe", "parakeet/onnx-asr/nemo128.onnx", "\"ci\""),
        );
        assert!(front_end.non_default_models("ci").is_empty());
    }

    #[test]
    fn models_qnn_execution_provider_comes_from_the_onnxruntime_qnn_release() {
        let m = load();
        let qnn: Vec<_> = m
            .artefacts
            .iter()
            .filter(|a| a.id.starts_with("qnn-ep-"))
            .collect();
        assert_eq!(qnn.len(), 2, "arm64 and x64");
        for a in qnn {
            assert!(
                a.url.starts_with(
                    "https://github.com/onnxruntime/onnxruntime-qnn/releases/download/v2.7.0/"
                ),
                "{}: {}",
                a.id,
                a.url
            );
            let digest = format!("github-asset-digest:sha256:{}", a.sha256);
            assert_eq!(a.publisher_checksum.as_deref(), Some(digest.as_str()));
            let x = a.extract.as_ref().expect("extracted");
            for f in [
                "onnxruntime_providers_qnn.dll",
                "QnnHtp.dll",
                "Qualcomm_LICENSE.pdf",
            ] {
                assert!(
                    x.members.iter().any(|mm| mm.path == f),
                    "{}: {f} must be pinned",
                    a.id
                );
            }
        }
    }

    #[test]
    fn models_onnx_runtime_comes_from_the_official_microsoft_release() {
        let m = load();
        for a in m
            .artefacts
            .iter()
            .filter(|a| a.id.starts_with("onnxruntime-"))
        {
            assert!(
                a.url.starts_with(
                    "https://github.com/microsoft/onnxruntime/releases/download/v1.31.0/"
                ),
                "{}: {}",
                a.id,
                a.url
            );
            assert_eq!(a.licence, "MIT", "{}", a.id);
            let digest = format!("github-asset-digest:sha256:{}", a.sha256);
            assert_eq!(
                a.publisher_checksum.as_deref(),
                Some(digest.as_str()),
                "{}",
                a.id
            );
        }
    }

    #[test]
    fn models_are_cache_only_with_licence_and_permissive_terms() {
        let m = load();
        for a in &m.artefacts {
            assert_eq!(a.location, Location::Cache, "{}", a.id);
            // CC-BY-4.0 (attribution required) is allowed only for the community Parakeet
            // conversions of the evaluation set; shipping such a model is the owner's decision
            // (T6.10e, 2026-10-22). The Parakeet defaults are also in ci and desktop, and v3 in
            // choice, as development copies (PLAN D22; the manifest comment on each says an
            // installer must not ship them from here), never in another set.
            let eval_only = a.path.starts_with("parakeet/")
                && a.sets.iter().any(|s| s == "eval")
                && a.sets
                    .iter()
                    .all(|s| ["eval", "ci", "desktop", "choice"].contains(&s.as_str()));
            // Qualcomm's AI Stack License (development use, distribution only inside an
            // application) is allowed only in the QNN development sets, which are never
            // shipped from here; bundling is T8.4's question for the owner (T8.3, 2026-10-24).
            let qnn_only =
                !a.sets.is_empty() && a.sets.iter().all(|s| s == "qnn" || s == "qnn-x64");
            // NVIDIA's licences (docs/cuda-libraries.md, with its open question) are allowed only
            // in the set of the app's opt-in download; the last id is llama.cpp's CUDA set, which
            // holds NVIDIA's runtime statically.
            let cuda_only = a.sets == ["cuda-libraries"];
            // Earshot's own NPU conversions of the CC-BY-4.0 Parakeet models (T8.4g, PLAN 7:
            // published with the credit), in their own set only.
            let npu_only = a.sets == [crate::qnn_pack::SET];
            let allowed: &[&str] = if eval_only || npu_only {
                &["MIT", "Apache-2.0", "CC-BY-4.0"]
            } else if qnn_only {
                &["MIT", "Apache-2.0", "MIT AND LicenseRef-Qualcomm-AI-Stack"]
            } else if cuda_only {
                &[
                    "LicenseRef-NVIDIA-CUDA-EULA",
                    "LicenseRef-NVIDIA-cuDNN-SLA",
                    "MIT AND LicenseRef-NVIDIA-CUDA-EULA",
                ]
            } else {
                &["MIT", "Apache-2.0"]
            };
            assert!(
                allowed.contains(&a.licence.as_str()),
                "{}: unexpected licence {}",
                a.id,
                a.licence
            );
            if a.licence == "CC-BY-4.0" {
                assert!(
                    a.attribution.contains("CC-BY-4.0") && a.attribution.contains("https://"),
                    "{}: CC-BY-4.0 needs the creator, licence and a link in its attribution",
                    a.id
                );
            }
        }
    }

    #[test]
    fn models_cuda_folder_rule_follows_the_shared_cases() {
        #[derive(serde::Deserialize)]
        struct Cases {
            case: Vec<Case>,
        }
        #[derive(serde::Deserialize)]
        struct Case {
            path: String,
            inside: bool,
        }
        let path = crate::repo_root().join("fixtures/models/cuda_folder_cases.toml");
        let cases: Cases = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(!cases.case.is_empty());
        for c in cases.case {
            assert_eq!(manifest::in_cuda_folder(&c.path), c.inside, "{}", c.path);
        }
        // It holds with no archive in the manifest, as in the app's parser.
        let text = format!(
            "sets = [\"ci\"]\nhosts = [\"example.org\"]\n{}",
            guard_artefact("x", "CUDA/x.zip", "\"ci\"")
        );
        let err = format!("{:#}", Manifest::parse(&text).unwrap_err());
        assert!(err.contains("inside the CUDA folder"), "{err}");
    }

    /// The manifest's `[[cuda_archive]]` tables, read by this parser's own rules, and those
    /// rules, with one artefact whose member pin is fixed.
    #[test]
    fn models_cuda_archives_follow_the_apps_rules() {
        let m = load();
        let parts: BTreeSet<&str> = m.cuda_archives.iter().map(|a| a.part.as_str()).collect();
        assert_eq!(parts, BTreeSet::from(["summaries", "transcription"]));
        let sha = "0".repeat(64);
        let file = |name: &str, member: &str| {
            format!(
                "[[cuda_archive.file]]\nname = \"{name}\"\nmember = \"{member}\"\n\
                 sha256 = \"{sha}\"\nsize = 1\n"
            )
        };
        let archive = |part: &str, artefact: &str, format: &str, files: &str| {
            format!(
                "[[cuda_archive]]\npart = \"{part}\"\nartefact = \"{artefact}\"\n\
                 format = \"{format}\"\n{files}\n"
            )
        };
        let parse = |tables: &str, path: &str| {
            let text = format!(
                "sets = [\"ci\"]\nhosts = [\"example.org\"]\n{}{}{tables}",
                guard_artefact("x", path, "\"ci\""),
                guard_artefact("y", "archives/y.zip", "\"ci\""),
            );
            Manifest::parse(&text).map_err(|e| format!("{e:#}"))
        };
        let good = file("a.dll", "bin/a.dll");
        parse(&archive("summaries", "x", "zip", &good), "archives/x.zip").unwrap();
        // Later entries for another part, on a host already listed, need nothing else.
        let two = archive("summaries", "x", "zip", &good)
            + &archive("transcription", "y", "tar.bz2", &file("b.dll", "bin/b.dll"));
        parse(&two, "archives/x.zip").unwrap();
        let bad = [
            (archive("other", "x", "zip", &good), "unknown part"),
            (archive("summaries", "x", "7z", &good), "cannot extract"),
            (archive("summaries", "z", "zip", &good), "unknown artefact"),
            (archive("summaries", "x", "zip", ""), "names no file"),
            (
                archive("summaries", "x", "zip", &file("sub/a.dll", "bin/a.dll")),
                "not a plain name",
            ),
            (
                archive("summaries", "x", "zip", &file("a.dll", "../a.dll")),
                "escapes",
            ),
            (
                archive("summaries", "x", "zip", &good)
                    + &archive("transcription", "x", "zip", &file("b.dll", "b.dll")),
                "listed twice",
            ),
            (
                archive("summaries", "x", "zip", &good)
                    + &archive("transcription", "y", "zip", &file("A.DLL", "b.dll")),
                "repeats",
            ),
        ];
        for (tables, why) in bad {
            let err = parse(&tables, "archives/x.zip").unwrap_err();
            assert!(err.contains(why), "{why}: {err}");
        }
        let err = parse(&archive("summaries", "x", "zip", &good), "cuda/x.zip").unwrap_err();
        assert!(err.contains("inside the CUDA folder"), "{err}");
    }

    /// Earshot's NPU conversions (T8.4g): what `cargo xtask qnn-pack` writes, where the runtime
    /// looks (`parakeet-qnn/<model>`, earshot-runtime `ParakeetModel::qnn_dir`), from the release
    /// that holds every hosted file, every file pinned, and only in their own set.
    #[test]
    fn models_npu_conversions_are_the_packed_archives_of_the_npu_ladder() {
        use crate::qnn_pack;
        let m = load();
        let held: Vec<&manifest::Artefact> = m
            .artefacts
            .iter()
            .filter(|a| a.sets.iter().any(|s| s == qnn_pack::SET))
            .collect();
        let ids: BTreeSet<&str> = held.iter().map(|a| a.id.as_str()).collect();
        let want: BTreeSet<String> = qnn_pack::MODELS
            .iter()
            .map(|n| format!("{n}-npu"))
            .collect();
        assert_eq!(ids, want.iter().map(String::as_str).collect());
        let npu_ladder = m
            .ladders
            .iter()
            .find(|l| l.backend == "qnn" && l.scope == "english")
            .expect("the NPU ladder");
        for a in held {
            let model = a.id.strip_suffix("-npu").unwrap();
            assert!(npu_ladder.models.iter().any(|n| n == model), "{model}");
            assert!(
                m.models
                    .iter()
                    .any(|x| x.name == model && x.engine == "parakeet-tdt"),
                "{model}"
            );
            assert_eq!(a.sets, [qnn_pack::SET], "{}", a.id);
            assert_eq!(a.url, qnn_pack::release_url(model), "{}", a.id);
            assert_eq!(a.path, format!("archives/{}", qnn_pack::zip_name(model)));
            assert_eq!(a.licence, "CC-BY-4.0", "{}", a.id);
            assert!(a.attribution.contains("NVIDIA"), "{}", a.id);
            assert!(
                a.publisher_checksum.is_none(),
                "this project is the publisher"
            );
            let x = a.extract.as_ref().expect("an archive");
            assert_eq!(x.format, "zip");
            assert_eq!(x.into, format!("parakeet-qnn/{model}"));
            let members: Vec<&str> = x.members.iter().map(|mm| mm.path.as_str()).collect();
            let mut want = vec![qnn_pack::NOTICE];
            want.extend(qnn_pack::FILES);
            assert_eq!(members, want, "{}", a.id);
            // Every archive and file stays below 4 GiB: no zip64 (neither reader accepts it).
            assert!(a.size < u64::from(u32::MAX), "{}", a.id);
        }
    }

    #[test]
    fn models_archives_pin_their_onnx_member() {
        let m = load();
        for a in m.artefacts.iter().filter(|a| a.extract.is_some()) {
            let x = a.extract.as_ref().unwrap();
            // A model archive pins its ONNX model; an ONNX Runtime archive pins the library.
            let wanted = if a.id.starts_with("onnxruntime-") {
                "/lib/onnxruntime.dll"
            } else if a.id.starts_with("qnn-ep-") {
                "onnxruntime_providers_qnn.dll"
            } else {
                ".onnx"
            };
            assert!(
                x.members.iter().any(|mm| mm.path.ends_with(wanted)),
                "{}: extracted {wanted} file must be pinned",
                a.id
            );
        }
    }
}
