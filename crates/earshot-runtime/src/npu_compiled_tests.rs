use std::cell::Cell;
use std::path::{Path, PathBuf};

use super::*;
use crate::testutil::TempDir;

const BINARY: &str = "encoder_ctx_qnn.bin";

fn npu(driver: &str) -> earshot_hardware::Npu {
    earshot_hardware::Npu {
        name: "Test NPU".to_owned(),
        driver_version: Some(driver.to_owned()),
    }
}

fn key_with(driver: &str) -> CompileKey {
    CompileKey::from_parts(
        &[("encoder.onnx", "aa11"), ("encoder.weights", "bb22")],
        &earshot_onnx::qnn::QNN_ARM64,
        "cc33",
        &[npu(driver)],
    )
}

fn key() -> CompileKey {
    key_with("1.0.0.0")
}

/// What a fake load returned, and which fake loads ran.
#[derive(Default)]
struct Calls {
    fit: Cell<u32>,
    compiled: Cell<u32>,
    compile: Cell<u32>,
    ordinary: Cell<u32>,
}

/// One [`load`] with fakes: the plugin answers `fit`; a compiled load succeeds when `compiled_ok`;
/// a compile writes the context model and its binary when `compile_ok`; the ordinary load always
/// succeeds.
fn run(
    conversion: &Path,
    key: &CompileKey,
    calls: &Calls,
    fit: bool,
    compiled_ok: bool,
    compile_ok: bool,
) -> (&'static str, How) {
    load(
        conversion,
        key,
        |model| {
            calls.fit.set(calls.fit.get() + 1);
            assert!(model.ends_with(CONTEXT_MODEL));
            Ok(fit)
        },
        |model| {
            calls.compiled.set(calls.compiled.get() + 1);
            assert!(model.is_file(), "a compiled load gets a verified file");
            if compiled_ok {
                Ok("compiled")
            } else {
                Err("the plugin refused the context model".to_owned())
            }
        },
        Compile::InProcess(Box::new(|model: &Path| {
            calls.compile.set(calls.compile.get() + 1);
            if !compile_ok {
                return Err("the plugin could not write".to_owned());
            }
            write_fake_compile(model);
            Ok("ordinary, writing")
        })),
        || {
            calls.ordinary.set(calls.ordinary.get() + 1);
            Ok::<_, String>("ordinary")
        },
    )
    .unwrap()
}

/// What the plugin writes, faked: the context model and its binary.
fn write_fake_compile(model: &Path) {
    let folder = model.parent().unwrap();
    std::fs::write(model, b"context model").unwrap();
    std::fs::write(folder.join(BINARY), vec![7u8; 4096]).unwrap();
}

/// [`run`] with a helper process that writes when `helper_ok`.
fn run_helper(
    conversion: &Path,
    key: &CompileKey,
    calls: &Calls,
    helper_ok: bool,
    compiled_ok: bool,
) -> (&'static str, How) {
    load(
        conversion,
        key,
        |_| Ok(true),
        |model| {
            calls.compiled.set(calls.compiled.get() + 1);
            assert!(model.is_file());
            if compiled_ok {
                Ok("compiled")
            } else {
                Err("the plugin refused the context model".to_owned())
            }
        },
        Compile::Helper(Box::new(|model: &Path| {
            calls.compile.set(calls.compile.get() + 1);
            if !helper_ok {
                return Err("the helper ended with exit code: 1".to_owned());
            }
            write_fake_compile(model);
            Ok(())
        })),
        || {
            calls.ordinary.set(calls.ordinary.get() + 1);
            Ok::<_, String>("ordinary")
        },
    )
    .unwrap()
}

/// With a helper this process only ever loads compiled models: a first compile is the helper's,
/// then the compiled load, and the next load uses the folder.
#[test]
fn npu_compiled_a_helper_compiles_and_this_process_loads_only_compiled_models() {
    let dir = TempDir::new("npu-helper");
    let calls = Calls::default();
    let (s, how) = run_helper(&dir.0, &key(), &calls, true, true);
    assert_eq!(s, "compiled");
    assert_eq!(how, How::CompiledByHelper { refused: None });
    assert!(!how.is_refusal());
    assert_eq!(
        (
            calls.compile.get(),
            calls.compiled.get(),
            calls.ordinary.get()
        ),
        (1, 1, 0)
    );
    assert_eq!(
        names(&folder(&dir.0, &key())),
        [RECORD, CONTEXT_MODEL, BINARY]
    );
    let calls = Calls::default();
    assert_eq!(
        run_helper(&dir.0, &key(), &calls, true, true),
        ("compiled", How::Compiled)
    );
    assert_eq!(calls.compile.get(), 0);
}

#[test]
fn npu_compiled_a_failed_helper_or_its_refused_model_is_the_ordinary_load() {
    for (helper_ok, compiled_ok) in [(false, true), (true, false)] {
        let dir = TempDir::new("npu-helper-fails");
        let calls = Calls::default();
        let (s, how) = run_helper(&dir.0, &key(), &calls, helper_ok, compiled_ok);
        assert_eq!(s, "ordinary");
        assert!(how.is_refusal(), "{how:?}");
        assert!(matches!(how, How::Ordinary { .. }), "{how:?}");
        assert_eq!(calls.ordinary.get(), 1);
        assert!(
            !names(&dir.0.join(COMPILED_DIR))
                .iter()
                .any(|n| n.starts_with(".tmp")),
            "no temp folder left"
        );
    }
}

/// A damaged folder with a helper: put aside, the helper compiles again, and the load is compiled.
#[test]
fn npu_compiled_a_damaged_folder_with_a_helper_is_compiled_again_there() {
    let dir = TempDir::new("npu-helper-damaged");
    run_helper(&dir.0, &key(), &Calls::default(), true, true);
    std::fs::write(folder(&dir.0, &key()).join(BINARY), vec![8u8; 4096]).unwrap();
    let calls = Calls::default();
    let (s, how) = run_helper(&dir.0, &key(), &calls, true, true);
    assert_eq!(s, "compiled");
    match &how {
        How::CompiledByHelper { refused: Some(why) } => assert!(why.contains(BINARY), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(how.is_refusal());
}

#[test]
fn npu_compiled_child_mode_answers_only_its_flag() {
    use std::ffi::OsString;
    let exe = OsString::from("earshot.exe");
    assert_eq!(child_main(std::slice::from_ref(&exe)), None);
    assert_eq!(child_main(&[exe.clone(), "--summary-child".into()]), None);
    // Its own flag with arguments it does not take: refused, nothing loaded.
    assert_eq!(
        child_main(&[exe.clone(), COMPILE_FLAG.into(), "--out".into()]),
        Some(1)
    );
    assert_eq!(
        child_main(&[
            exe,
            COMPILE_FLAG.into(),
            "--model".into(),
            "a".into(),
            "--models".into(),
            "b".into(),
            "--out".into(),
            "c".into()
        ]),
        Some(1)
    );
}

fn folder(conversion: &Path, key: &CompileKey) -> PathBuf {
    conversion.join(COMPILED_DIR).join(key.folder_name())
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn npu_compiled_first_load_compiles_and_the_next_uses_it() {
    let dir = TempDir::new("npu-first");
    let calls = Calls::default();
    let (s, how) = run(&dir.0, &key(), &calls, true, true, true);
    assert_eq!(s, "ordinary, writing");
    assert_eq!(
        how,
        How::Compiling {
            refused: None,
            kept: Ok(())
        }
    );
    assert!(
        !how.is_refusal(),
        "the first compile of a key is no refusal"
    );
    assert_eq!(
        names(&folder(&dir.0, &key())),
        [RECORD, CONTEXT_MODEL, BINARY]
    );
    assert_eq!(
        names(&dir.0.join(COMPILED_DIR)),
        [key().folder_name()],
        "no temp folder is left"
    );

    let calls = Calls::default();
    let (s, how) = run(&dir.0, &key(), &calls, true, true, true);
    assert_eq!((s, how), ("compiled", How::Compiled));
    assert_eq!(
        (calls.compile.get(), calls.ordinary.get(), calls.fit.get()),
        (0, 0, 1)
    );
}

/// The accept's case: a damaged compiled model falls back to the ordinary load (which compiles it
/// again), says why, and the next load uses the new one.
#[test]
fn npu_compiled_a_damaged_binary_is_refused_and_compiled_again() {
    let dir = TempDir::new("npu-damaged");
    run(&dir.0, &key(), &Calls::default(), true, true, true);
    let binary = folder(&dir.0, &key()).join(BINARY);
    // Same size, one byte changed: only the hash can tell.
    let mut bytes = std::fs::read(&binary).unwrap();
    bytes[100] ^= 1;
    std::fs::write(&binary, bytes).unwrap();

    let calls = Calls::default();
    let (s, how) = run(&dir.0, &key(), &calls, true, true, true);
    assert_eq!(s, "ordinary, writing");
    assert_eq!(
        calls.compiled.get(),
        0,
        "a damaged file never reaches the NPU"
    );
    match &how {
        How::Compiling {
            refused: Some(why),
            kept: Ok(()),
        } => assert!(why.contains(BINARY), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(how.is_refusal());
    let calls = Calls::default();
    assert_eq!(
        run(&dir.0, &key(), &calls, true, true, true).1,
        How::Compiled
    );
}

#[test]
fn npu_compiled_a_shorter_file_a_damaged_record_or_an_extra_file_is_refused() {
    type Spoil = (&'static str, fn(&Path));
    let damage: [Spoil; 4] = [
        ("shorter", |f| {
            std::fs::write(f.join(BINARY), vec![7u8; 10]).unwrap();
        }),
        ("record", |f| {
            std::fs::write(f.join(RECORD), b"{ not json").unwrap();
        }),
        ("extra", |f| {
            std::fs::write(f.join("other.bin"), b"x").unwrap();
        }),
        ("missing", |f| {
            std::fs::remove_file(f.join(CONTEXT_MODEL)).unwrap();
        }),
    ];
    for (name, spoil) in damage {
        let dir = TempDir::new("npu-spoilt");
        run(&dir.0, &key(), &Calls::default(), true, true, true);
        spoil(&folder(&dir.0, &key()));
        let calls = Calls::default();
        let (s, how) = run(&dir.0, &key(), &calls, true, true, true);
        assert_eq!(s, "ordinary, writing", "{name}");
        assert_eq!(calls.compiled.get(), 0, "{name}");
        assert!(how.is_refusal(), "{name}: {how:?}");
        assert_eq!(
            names(&folder(&dir.0, &key())),
            [RECORD, CONTEXT_MODEL, BINARY],
            "{name}: compiled again whole"
        );
    }
}

/// A stale model (anything in the key changed: here the NPU driver) is never read: a new folder
/// is compiled without a note and the old one swept.
#[test]
fn npu_compiled_a_stale_key_compiles_a_new_folder_and_sweeps_the_old() {
    let dir = TempDir::new("npu-stale");
    let old = key_with("1.0.0.0");
    run(&dir.0, &old, &Calls::default(), true, true, true);
    let new = key_with("2.0.0.0");
    assert_ne!(old.folder_name(), new.folder_name());

    let calls = Calls::default();
    let (s, how) = run(&dir.0, &new, &calls, true, true, true);
    assert_eq!(s, "ordinary, writing");
    assert_eq!((calls.fit.get(), calls.compiled.get()), (0, 0));
    assert!(!how.is_refusal(), "{how:?}");
    assert_eq!(names(&dir.0.join(COMPILED_DIR)), [new.folder_name()]);
}

/// A folder under the right name whose record names another key (a hand copy, or a hash
/// collision) is refused, not loaded.
#[test]
fn npu_compiled_a_record_for_another_key_is_refused() {
    let dir = TempDir::new("npu-other-key");
    let other = key_with("9.9.9.9");
    run(&dir.0, &other, &Calls::default(), true, true, true);
    std::fs::rename(folder(&dir.0, &other), folder(&dir.0, &key())).unwrap();
    let calls = Calls::default();
    let (_, how) = run(&dir.0, &key(), &calls, true, true, true);
    assert_eq!(calls.compiled.get(), 0);
    match how {
        How::Compiling {
            refused: Some(why), ..
        } => assert!(why.contains("something else"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn npu_compiled_the_plugins_no_or_a_refused_load_compiles_again() {
    for (fit, compiled_ok) in [(false, true), (true, false)] {
        let dir = TempDir::new("npu-refused");
        run(&dir.0, &key(), &Calls::default(), true, true, true);
        let calls = Calls::default();
        let (s, how) = run(&dir.0, &key(), &calls, fit, compiled_ok, true);
        assert_eq!(s, "ordinary, writing");
        assert_eq!(calls.compile.get(), 1);
        assert!(how.is_refusal(), "{how:?}");
    }
}

/// A compile that fails is the ordinary load, with no folder or temp left behind.
#[test]
fn npu_compiled_a_failed_compile_is_the_ordinary_load() {
    let dir = TempDir::new("npu-compile-fails");
    let calls = Calls::default();
    let (s, how) = run(&dir.0, &key(), &calls, true, true, false);
    assert_eq!(s, "ordinary");
    assert_eq!(calls.ordinary.get(), 1);
    match &how {
        How::Ordinary { why } => assert!(why.contains("could not write"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(how.is_refusal());
    assert!(names(&dir.0.join(COMPILED_DIR)).is_empty());
}

/// A model the ordinary load itself refuses is an error, as before.
#[test]
fn npu_compiled_only_the_ordinary_loads_failure_fails() {
    let dir = TempDir::new("npu-ordinary-fails");
    let r: Result<(&str, How), String> = load(
        &dir.0,
        &key(),
        |_| Ok(true),
        |_| Err("no".to_owned()),
        Compile::InProcess(Box::new(|_: &Path| Err("no".to_owned()))),
        || Err("the model is not on the NPU".to_owned()),
    );
    assert_eq!(r.unwrap_err(), "the model is not on the NPU");
}

#[test]
fn npu_compiled_a_young_temp_folder_is_left_and_an_old_one_swept() {
    let dir = TempDir::new("npu-sweep");
    let root = dir.0.join(COMPILED_DIR);
    let young = root.join(".tmp-1-0");
    std::fs::create_dir_all(&young).unwrap();
    let aside = root.join(".old-1-0");
    std::fs::create_dir_all(&aside).unwrap();
    run(&dir.0, &key(), &Calls::default(), true, true, true);
    assert!(young.is_dir(), "another process may be compiling into it");
    assert!(!aside.exists());
}

/// Another process published the same key first: ours is dropped and the load still succeeds.
#[test]
fn npu_compiled_losing_the_publish_race_is_no_failure() {
    let dir = TempDir::new("npu-race");
    let dest = folder(&dir.0, &key());
    let r = load(
        &dir.0,
        &key(),
        |_| Ok(true),
        |_| Ok::<_, String>("compiled"),
        Compile::InProcess(Box::new(|model: &Path| {
            std::fs::write(model, b"ours").unwrap();
            // Meanwhile the other process finished.
            std::fs::create_dir_all(&dest).unwrap();
            std::fs::write(dest.join(CONTEXT_MODEL), b"theirs").unwrap();
            std::fs::write(dest.join(RECORD), b"{}").unwrap();
            Ok("ordinary, writing")
        })),
        || Ok("ordinary"),
    )
    .unwrap();
    assert_eq!(
        r.1,
        How::Compiling {
            refused: None,
            kept: Ok(())
        }
    );
    assert_eq!(std::fs::read(dest.join(CONTEXT_MODEL)).unwrap(), b"theirs");
    assert_eq!(names(&dir.0.join(COMPILED_DIR)), [key().folder_name()]);
}

#[test]
fn npu_compiled_key_covers_the_encoder_qnn_runtime_and_npu() {
    let base = key();
    let npus = [npu("1.0.0.0")];
    let other_qnn = earshot_onnx::qnn::QnnPackage {
        htp_sha256: "0000",
        ..earshot_onnx::qnn::QNN_ARM64
    };
    let changed = [
        CompileKey::from_parts(
            &[("encoder.onnx", "ff11"), ("encoder.weights", "bb22")],
            &earshot_onnx::qnn::QNN_ARM64,
            "cc33",
            &npus,
        ),
        CompileKey::from_parts(
            &[("encoder.onnx", "aa11"), ("encoder.weights", "ff22")],
            &earshot_onnx::qnn::QNN_ARM64,
            "cc33",
            &npus,
        ),
        CompileKey::from_parts(
            &[("encoder.onnx", "aa11"), ("encoder.weights", "bb22")],
            &other_qnn,
            "cc33",
            &npus,
        ),
        CompileKey::from_parts(
            &[("encoder.onnx", "aa11"), ("encoder.weights", "bb22")],
            &earshot_onnx::qnn::QNN_ARM64,
            "ff33",
            &npus,
        ),
        key_with("1.0.0.1"),
        CompileKey::from_parts(
            &[("encoder.onnx", "aa11"), ("encoder.weights", "bb22")],
            &earshot_onnx::qnn::QNN_ARM64,
            "cc33",
            &[],
        ),
    ];
    for k in &changed {
        assert_ne!(k.folder_name(), base.folder_name(), "{}", k.text());
    }
    assert_eq!(base.folder_name().len(), 16);
    // Case in a pin is not a change.
    assert_eq!(
        CompileKey::from_parts(
            &[("encoder.onnx", "AA11"), ("encoder.weights", "bb22")],
            &earshot_onnx::qnn::QNN_ARM64,
            "CC33",
            &npus,
        ),
        base
    );
}

/// The only test that touches the process's note state.
#[test]
fn npu_compiled_a_refusal_is_noted_once_per_process() {
    assert_eq!(take_note(), None);
    record(&How::Compiled);
    record(&How::Compiling {
        refused: None,
        kept: Ok(()),
    });
    assert_eq!(take_note(), None, "no refusal, no note");
    record(&How::Ordinary {
        why: "x".to_owned(),
    });
    record(&How::Ordinary {
        why: "y".to_owned(),
    });
    assert_eq!(take_note(), Some(NOTE));
    assert_eq!(take_note(), None);
    record(&How::Ordinary {
        why: "z".to_owned(),
    });
    assert_eq!(take_note(), None, "once per process");
}

#[test]
fn npu_compiled_note_is_plain_copy() {
    assert!(!NOTE.contains('\u{2014}'));
    for word in ["QNN", "onnx", "compiled.json", "cargo"] {
        assert!(!NOTE.contains(word), "{word}");
    }
}

/// Hard links of every file under `from` (not its `compiled` folder) into `to`.
#[cfg(all(feature = "qnn", target_arch = "aarch64"))]
fn link_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            if entry.file_name() != COMPILED_DIR {
                link_tree(&path, &to.join(entry.file_name()));
            }
        } else {
            std::fs::hard_link(&path, to.join(entry.file_name())).unwrap();
        }
    }
}

/// The app's start-up compile: helper only, then a load finds it compiled; a second start does
/// nothing; a damaged folder is compiled again; a failed helper leaves nothing behind.
#[test]
fn npu_compiled_prepare_compiles_ahead_so_the_first_load_is_compiled() {
    let dir = TempDir::new("npu-prepare");
    let calls = Cell::new(0);
    let helper = |model: &Path| {
        calls.set(calls.get() + 1);
        write_fake_compile(model);
        Ok(())
    };
    assert_eq!(
        prepare(&dir.0, &key(), helper),
        Ok(Prepared::Compiled { replaced: None })
    );
    assert_eq!(
        prepare(&dir.0, &key(), helper),
        Ok(Prepared::AlreadyCompiled)
    );
    assert_eq!(calls.get(), 1, "a second start compiles nothing");
    let load_calls = Calls::default();
    assert_eq!(
        run(&dir.0, &key(), &load_calls, true, true, true),
        ("compiled", How::Compiled)
    );
    assert_eq!(load_calls.compile.get(), 0);

    std::fs::write(folder(&dir.0, &key()).join(BINARY), vec![9u8; 4096]).unwrap();
    match prepare(&dir.0, &key(), helper) {
        Ok(Prepared::Compiled {
            replaced: Some(why),
        }) => assert!(why.contains(BINARY), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(calls.get(), 2);
}

#[test]
fn npu_compiled_prepare_with_a_failed_helper_leaves_nothing() {
    let dir = TempDir::new("npu-prepare-fails");
    let r = prepare(&dir.0, &key(), |_| {
        Err("the helper ended with exit code: 1".to_owned())
    });
    assert!(r.unwrap_err().contains("exit code"));
    assert!(names(&dir.0.join(COMPILED_DIR)).is_empty());
    // A load then compiles as it would have.
    let calls = Calls::default();
    run(&dir.0, &key(), &calls, true, true, true);
    assert_eq!(calls.compile.get(), 1);
}
/// The real thing on the Surface's NPU: the first load compiles and keeps the context model, the
/// next loads it (the folder untouched), both give exactly the words, times and probabilities of
/// the load before T12.12a, and a damaged binary falls back to the ordinary load, compiles again
/// and is noted. The models folder is hard links of the real files in a temp folder, so nothing in
/// the real one is written.
#[cfg(all(feature = "qnn", target_arch = "aarch64"))]
#[test]
#[ignore = "needs the Surface's NPU and model sets qnn and qnn-models (cargo xtask fetch-models --set qnn --set qnn-models), in the ARM64 build environment `cargo xtask build` sets: cargo test --release -p earshot-runtime --features qnn --lib npu_compiled_real -- --ignored"]
fn npu_compiled_real_npu_compiles_once_then_loads_the_same_words() {
    use crate::backends::{Accelerator, AsrModel, ModelDir, asr_model, init_onnx, load_parakeet};
    use earshot_core::asr::{CancelToken, TranscribeOptions, Transcriber};
    use earshot_core::audio::{MonoBuffer, PIPELINE_RATE};
    use std::time::Instant;

    const MODEL: &str = "parakeet-tdt-110m";
    let real = ModelDir::from_env(&earshot_store::DataDir::from_local_app_data().unwrap());
    let tmp = TempDir::new("npu-real");
    let ort_lib = Path::new(earshot_onnx::ORT_CPU_DYLIB).parent().unwrap();
    link_tree(&real.root().join(ort_lib), &tmp.0.join(ort_lib));
    let qnn = earshot_onnx::qnn::QNN_ARM64.dir;
    link_tree(&real.root().join(qnn), &tmp.0.join(qnn));
    let conversion = Path::new("parakeet-qnn").join(MODEL);
    link_tree(&real.root().join(&conversion), &tmp.0.join(&conversion));
    let models = ModelDir::new(&tmp.0);
    init_onnx(&models, Accelerator::Qnn).unwrap();
    let Ok(AsrModel::Parakeet(model)) = asr_model(MODEL) else {
        panic!("{MODEL} is a Parakeet model");
    };

    let clip = earshot_store::read_flac(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/smoke/121-121726-0002.flac"),
    )
    .unwrap()
    .1;
    let audio = MonoBuffer::new(PIPELINE_RATE, clip);
    let words = |t: &mut dyn Transcriber| -> Vec<String> {
        let out = t
            .transcribe(&audio, &TranscribeOptions::live(), &CancelToken::new())
            .unwrap();
        assert_eq!(out.backend.kind, earshot_core::asr::BackendKind::Qnn);
        out.segments
            .iter()
            .flat_map(|s| s.timed_words())
            .map(|w| {
                format!(
                    "{}@{}-{}:{}",
                    w.text,
                    w.start.as_millis(),
                    w.end.as_millis(),
                    w.probability
                )
            })
            .collect()
    };
    let dir = tmp.0.join(&conversion);
    let compiled_root = dir.join(COMPILED_DIR);
    let load = |what: &str| {
        let started = Instant::now();
        let t = load_parakeet(&models, model, Accelerator::Qnn, Some(2)).unwrap();
        println!(
            "{what}: loaded in {:.1} s, {:?}",
            started.elapsed().as_secs_f64(),
            last_load()
        );
        t
    };

    // The load before T12.12a, as the reference.
    let mut ordinary = {
        let steps = earshot_onnx::SessionConfig::cpu(2);
        let encoder = crate::backends::onnx_session_config(Accelerator::Qnn, 2).unwrap();
        earshot_onnx::parakeet::ParakeetTdt::load_split(&dir, &encoder, &steps, MODEL, true)
            .unwrap()
    };
    let reference = words(&mut ordinary);
    assert!(!reference.is_empty());
    drop(ordinary);
    assert!(!compiled_root.exists(), "the ordinary load writes nothing");

    let mut first = load("first (compiles)");
    let names_after_first = names(&compiled_root);
    assert_eq!(names_after_first.len(), 1, "{names_after_first:?}");
    let folder = compiled_root.join(&names_after_first[0]);
    assert!(folder.join(RECORD).is_file());
    let record_time = std::fs::metadata(folder.join(RECORD))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(words(&mut first), reference, "the session that compiled");
    assert_eq!(take_note(), None, "a first compile is no refusal");

    let mut second = load("second (compiled)");
    assert_eq!(words(&mut second), reference, "from the compiled model");
    assert_eq!(
        std::fs::metadata(folder.join(RECORD))
            .unwrap()
            .modified()
            .unwrap(),
        record_time,
        "the second load used the folder as it was"
    );
    assert_eq!(names(&compiled_root), names_after_first);
    assert_eq!(take_note(), None);
    drop((first, second));

    // The sequences that crashed before T12.12b: close, then load compiled, again and again. Every
    // close now keeps the session, and the next load takes it back.
    for round in 0..5 {
        let mut again = load(&format!("round {round} (from the pool)"));
        assert_eq!(words(&mut again), reference);
        drop(again);
        assert!(earshot_onnx::idle_qnn_sessions() >= 1);
    }

    // A damaged binary. The one this process loaded is mapped and cannot be written while it runs,
    // so the damage is done to a fresh copy under a second models folder.
    let tmp2 = TempDir::new("npu-real-damaged");
    link_tree(&real.root().join(&conversion), &tmp2.0.join(&conversion));
    let folder2 = tmp2
        .0
        .join(&conversion)
        .join(COMPILED_DIR)
        .join(&names_after_first[0]);
    std::fs::create_dir_all(&folder2).unwrap();
    for name in names(&folder) {
        std::fs::copy(folder.join(&name), folder2.join(&name)).unwrap();
    }
    let binary = names(&folder2)
        .into_iter()
        .find(|n| n.ends_with(".bin"))
        .expect("a context binary");
    let mut bytes = std::fs::read(folder2.join(&binary)).unwrap();
    let damaged_byte = bytes.len() / 2;
    bytes[damaged_byte] ^= 1;
    std::fs::write(folder2.join(&binary), &bytes).unwrap();
    let models2 = ModelDir::new(&tmp2.0);
    let load2 = |what: &str| {
        let started = Instant::now();
        let t = load_parakeet(&models2, model, Accelerator::Qnn, Some(2)).unwrap();
        println!(
            "{what}: loaded in {:.1} s, {:?}",
            started.elapsed().as_secs_f64(),
            last_load()
        );
        t
    };
    let mut third = load2("damaged copy (compiles again)");
    assert_eq!(words(&mut third), reference, "after the fallback");
    assert_eq!(take_note(), Some(NOTE), "the refusal is noted");
    assert!(
        matches!(
            last_load(),
            Some(How::Compiling {
                refused: Some(_),
                kept: Ok(())
            })
        ),
        "{:?}",
        last_load()
    );
    drop(third);
    let mut fourth = load2("damaged copy, compiled again");
    assert_eq!(words(&mut fourth), reference);
    assert_eq!(last_load(), Some(How::Compiled));
    // Pooled sessions keep their files mapped to the end of the process, so the temp folders stay
    // behind (hard links and a compiled copy, under %TEMP%).
    std::mem::forget((tmp, tmp2));
}

/// T12.12b's idle battery check: holds `EARSHOT_IDLE_SESSIONS` loaded NPU sessions (0: the plugin
/// registered and none loaded) of `EARSHOT_STRESS_MODEL` doing nothing for `EARSHOT_IDLE_SECONDS`
/// (default 600), sampling Windows' battery discharge rate (`root\wmi` `BatteryStatus`,
/// milliwatts) every 15 s, and prints the mean. Run unplugged, with the screen on and nothing else
/// open, once with 0 and once with 2.
#[cfg(all(feature = "qnn", target_arch = "aarch64"))]
#[test]
#[ignore = "needs the Surface unplugged, model sets qnn and qnn-models and a compiled folder: set EARSHOT_IDLE_SESSIONS=0, then 2, and run cargo test --release -p earshot-runtime --features qnn --lib npu_compiled_idle_battery -- --ignored --nocapture"]
fn npu_compiled_idle_battery() {
    use crate::backends::{Accelerator, AsrModel, ModelDir, asr_model, init_onnx, load_parakeet};
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_owned());
    let sessions: usize = env("EARSHOT_IDLE_SESSIONS", "0").parse().unwrap();
    let seconds: u64 = env("EARSHOT_IDLE_SECONDS", "600").parse().unwrap();
    let name = env("EARSHOT_STRESS_MODEL", "parakeet-tdt-0.6b-v2");
    let models = ModelDir::from_env(&earshot_store::DataDir::from_local_app_data().unwrap());
    init_onnx(&models, Accelerator::Qnn).unwrap();
    let Ok(AsrModel::Parakeet(model)) = asr_model(&name) else {
        panic!("{name} is a Parakeet model");
    };
    let held: Vec<_> = (0..sessions)
        .map(|_| load_parakeet(&models, model, Accelerator::Qnn, Some(2)).unwrap())
        .collect();
    let rate = || -> Option<f64> {
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$b = Get-CimInstance -Namespace root\\wmi -ClassName BatteryStatus | \
                 Select-Object -First 1; if ($b.PowerOnline) { 'online' } else { $b.DischargeRate }",
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        assert_ne!(text, "online", "the Surface must run on its battery");
        text.parse().ok()
    };
    // Let the loads settle before measuring.
    std::thread::sleep(std::time::Duration::from_secs(60));
    let mut samples = Vec::new();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    while std::time::Instant::now() < end {
        if let Some(r) = rate() {
            samples.push(r);
        }
        std::thread::sleep(std::time::Duration::from_secs(15));
    }
    let mean = samples.iter().sum::<f64>() / samples.len().max(1) as f64;
    println!(
        "{sessions} idle NPU sessions of {name}: mean discharge {mean:.0} mW over {} samples \
         ({seconds} s)",
        samples.len()
    );
    drop(held);
}
