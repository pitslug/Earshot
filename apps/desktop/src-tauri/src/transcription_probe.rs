//! Trying the CUDA transcription folder in a throwaway process (T8.4ad, docs/cuda-install.md).
//!
//! NVIDIA's libraries can end the process that loads them with no error reaching Rust (measured:
//! exit `0xC0000409` from a partial cuDNN), and the card, driver and files decide. So before the
//! app chooses CUDA, its own exe runs in the hidden mode `--cuda-transcription-probe --models
//! <dir>`: it chooses CUDA, loads Parakeet there and decodes a generated tone
//! (`earshot_runtime::cuda::trial_on_cuda`), never audio from a recording. Only a clean exit that
//! names CUDA from ONNX Runtime's placement counts; anything else stays off CUDA. Handled first in
//! `main`, like `llama_probe`: it never opens the store and prints fixed words and placement counts.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use earshot_runtime::backends::ModelDir;

use crate::llama_probe::{Verdict, verdict_of};

/// The flag that selects this mode.
pub const PROBE_ARG: &str = "--cuda-transcription-probe";
/// How long a trial may take: a model load from a slow disk plus a short decode. A judgement.
const PROBE_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// The exit code of a child whose files failed their check.
const FILES_REFUSED: i32 = 4;
/// The backend a usable trial names.
const CUDA: &str = "cuda";

/// The models folder of the child, `None` when `args` does not ask for this mode.
fn parse(args: &[OsString]) -> Option<Result<PathBuf, String>> {
    if !args.iter().any(|a| a == PROBE_ARG) {
        return None;
    }
    let models = args
        .iter()
        .position(|a| a == "--models")
        .and_then(|i| args.get(i + 1));
    Some(match models {
        Some(m) => Ok(PathBuf::from(m)),
        None => Err(format!("{PROBE_ARG} needs --models")),
    })
}

/// Runs the child when `args` asks for it, and returns its exit code.
#[must_use]
pub fn child_main(args: &[OsString]) -> Option<i32> {
    Some(match parse(args)? {
        Err(_) => 2,
        Ok(models) => match earshot_runtime::cuda::trial_on_cuda(&ModelDir::new(models)) {
            // The child's check of the files, which a start relies on: it says so by its code.
            Err(earshot_runtime::RuntimeError::Unavailable(why))
                if why == earshot_runtime::cuda::NOT_READY =>
            {
                FILES_REFUSED
            }
            Ok(placement) => {
                println!("placement: {placement}");
                println!("probe: ok {CUDA}");
                0
            }
            // A failed trial says nothing the parent could not say with its exit code.
            Err(_) => 3,
        },
    })
}

/// The verdict of a child that exited with `code` and printed `stdout`: only CUDA counts.
#[must_use]
pub fn verdict(code: Option<i32>, stdout: &str) -> Verdict {
    if code == Some(FILES_REFUSED) {
        return Verdict::Refused {
            why: "its files did not pass the check".to_owned(),
        };
    }
    match verdict_of(code, stdout) {
        Verdict::Usable { backend } if backend != CUDA => Verdict::Refused {
            why: format!("the trial ran on {backend}"),
        },
        other => other,
    }
}

/// Tries the CUDA transcription folder of `models` in a child of this exe. Call it before
/// `earshot_runtime::cuda::use_for_transcription`, and only on [`Verdict::Usable`] make that call.
#[must_use]
pub fn try_transcription_folder(models: &ModelDir) -> Verdict {
    if let Err(e) = earshot_runtime::cuda::verify_transcription_folder(models) {
        return Verdict::Refused { why: e.to_string() };
    }
    trial_within(models, PROBE_DEADLINE)
}

/// The trial in a child of this exe, with a time limit of the caller's and no check of the files
/// by this process (the child makes its own): a start that has just checked them calls this.
#[must_use]
pub fn trial_within(models: &ModelDir, deadline: Duration) -> Verdict {
    let Ok(program) = std::env::current_exe() else {
        return Verdict::Refused {
            why: "the app could not find itself".to_owned(),
        };
    };
    let spec = earshot_proc::ChildSpec {
        program,
        args: vec![
            OsString::from(PROBE_ARG),
            OsString::from("--models"),
            models.root().as_os_str().to_owned(),
        ],
        current_dir: std::env::temp_dir(),
        env_remove: Vec::new(),
        env_set: Vec::new(),
        stdin: Vec::new(),
        deadline,
        max_output: 4 * 1024 * 1024,
    };
    match earshot_proc::run(&spec, &|| false) {
        Ok(out) => verdict(out.status.code(), &String::from_utf8_lossy(&out.stdout)),
        Err(earshot_proc::ChildError::TimedOut(_)) => Verdict::Refused {
            why: "the trial did not finish in time".to_owned(),
        },
        Err(_) => Verdict::Refused {
            why: "the trial could not run".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn transcription_probe_is_asked_for_only_by_its_flag_and_needs_the_models() {
        assert_eq!(parse(&args(&["--replay", "x", "--models", "m"])), None);
        assert_eq!(
            parse(&args(&[PROBE_ARG, "--models", "C:/m"])),
            Some(Ok(PathBuf::from("C:/m")))
        );
        assert!(parse(&args(&[PROBE_ARG])).unwrap().is_err());
    }

    #[test]
    fn transcription_probe_only_a_clean_exit_naming_cuda_is_usable() {
        assert_eq!(
            verdict(Some(0), "placement: x\nprobe: ok cuda\n"),
            Verdict::Usable {
                backend: CUDA.to_owned()
            }
        );
        for (code, stdout) in [
            (Some(0), "probe: ok cpu\n"),
            (Some(0), "placement: x\n"),
            (Some(-1_073_740_791), "probe: ok cuda\n"),
            (Some(3), ""),
            (None, "probe: ok cuda\n"),
        ] {
            assert!(
                matches!(verdict(code, stdout), Verdict::Refused { .. }),
                "{code:?} {stdout}"
            );
        }
    }

    #[test]
    fn transcription_probe_files_that_fail_the_childs_check_say_so() {
        let Verdict::Refused { why } = verdict(Some(FILES_REFUSED), "") else {
            panic!("refused");
        };
        assert!(why.contains("did not pass the check"), "{why}");
    }

    #[test]
    fn transcription_probe_a_folder_with_a_file_missing_starts_no_child() {
        let Verdict::Refused { why } = try_transcription_folder(&ModelDir::new("does/not/exist"))
        else {
            panic!("a missing folder is refused");
        };
        assert!(why.contains("not all installed"), "{why}");
    }
}
