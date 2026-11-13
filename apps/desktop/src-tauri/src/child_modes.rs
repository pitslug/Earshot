//! The app's hidden child modes, in the order `main` tries them: each runs and exits before
//! anything of the app (Tauri, the store, the window) is built. Every one takes the process's
//! arguments after the program (T12.12e: 0.3.3 gave the NPU compile the other form, so the helper
//! started the app again instead of compiling).

use std::ffi::OsString;

/// The exit code of the child mode `args` (the process's arguments after the program) ask for, or
/// `None` for the app itself.
pub fn run(args: &[OsString]) -> Option<i32> {
    // The model check (T7.4e).
    if let Some(code) = crate::check_child::child_main(args) {
        return Some(code);
    }
    // The trial of a set of llama.cpp's DLLs, which may end its process.
    if let Some(code) = crate::llama_probe::child_main(args) {
        return Some(code);
    }
    // The trial of the CUDA transcription folder, for the same reason.
    if let Some(code) = crate::transcription_probe::child_main(args) {
        return Some(code);
    }
    // A summary on the graphics card, so a driver crash costs only that summary (T9.3).
    if let Some(code) = earshot_runtime::summary_child::child_main(args) {
        return Some(code);
    }
    // An NPU compile (T12.12b): the app itself never closes the session that compiled.
    earshot_runtime::npu_compiled::child_main(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_modes_the_npu_compile_line_reaches_its_mode() {
        // Exactly the line the helper is started with, after the program. With its paths missing
        // the mode refuses (exit 1) rather than starting the app (`None`), which loads nothing.
        let dir = std::env::temp_dir().join(format!("earshot-child-modes-{}", std::process::id()));
        let line = earshot_runtime::npu_compiled::helper_args(
            &dir.join("no-models"),
            "parakeet-tdt-0.6b-v2",
            &dir.join("encoder_ctx.onnx"),
        );
        assert_eq!(run(&line), Some(1));
        assert!(!dir.exists(), "nothing written");
        // The app itself.
        assert_eq!(run(&[]), None);
    }
}
