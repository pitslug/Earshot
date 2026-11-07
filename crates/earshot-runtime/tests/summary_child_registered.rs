//! Registering the exe as the GPU summaries' child (T9.3) is process-wide, so its own test binary.

use earshot_runtime::backends::ModelDir;
use earshot_runtime::summary_child::{SUMMARY_CHILD_FLAG, registered, use_this_exe};
use earshot_runtime::summary_model::{
    ChoiceReason, SUMMARY_MODELS, SummaryModelChoice, local_summary_config,
};

#[test]
fn summary_child_registered_exe_runs_gpu_summaries_and_only_those() {
    let dir = std::env::temp_dir().join(format!("earshot-summary-child-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let model = &SUMMARY_MODELS[0];
    let file = dir.join(model.file);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    // A sparse stand-in of the manifest's size: nothing here loads it.
    std::fs::File::create(&file)
        .unwrap()
        .set_len(model.size)
        .unwrap();
    let models = ModelDir::new(dir.clone());
    let choice = SummaryModelChoice {
        model,
        reason: ChoiceReason::Requested,
    };

    // Not registered: in process, as `earshot-cli.exe` runs.
    assert_eq!(registered(), None);
    let config = local_summary_config(&models, &choice, true).unwrap();
    assert_eq!(config.gpu_child, None);

    use_this_exe();
    let gpu = local_summary_config(&models, &choice, true).unwrap();
    let child = gpu.gpu_child.expect("a GPU summary runs in the child");
    assert_eq!(child.program, std::env::current_exe().unwrap());
    assert_eq!(child.args, [std::ffi::OsString::from(SUMMARY_CHILD_FLAG)]);
    let cpu = local_summary_config(&models, &choice, false).unwrap();
    assert_eq!(cpu.gpu_child, None, "a CPU summary stays in the parent");

    std::fs::remove_dir_all(&dir).unwrap();
}
