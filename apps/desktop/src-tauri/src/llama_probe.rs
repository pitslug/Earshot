//! Trying a folder of llama.cpp DLLs in a throwaway process (T8.4ada, docs/cuda-install.md).
//!
//! A CUDA set can pass every load and still end the process: on a card it holds no code for, ggml
//! aborts on the first GPU call (measured 2026-11-06, exit `0xC0000409`), and no error ever reaches
//! Rust. NVIDIA's generations this project cannot test, and future ones, could do the same. So
//! before the app runs from such a folder, its own exe runs in the hidden mode `--llama-probe
//! <folder> --model <gguf>`: it loads the set and the model on the GPU and decodes a few tokens.
//! If that process dies, only it is lost and the app stays on the set beside its exe. The same
//! reasoning as the model check's child (`check_child`): this mode is handled first in `main`,
//! never opens the store, and prints fixed words only.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The flag that selects this mode.
pub const PROBE_ARG: &str = "--llama-probe";
/// How long a trial may take: a model load from a slow disk plus a few tokens. A judgement.
const PROBE_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// What the child prints when the GPU ran, before the backend's name.
const OK_LINE: &str = "probe: ok ";

/// What the trial found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The set ran on `backend`, by its own load's evidence.
    Usable {
        /// `cuda` or `vulkan`.
        backend: String,
    },
    /// It did not; `why` is fixed text with at most an exit code, for the note the user sees.
    Refused {
        /// Why.
        why: String,
    },
}

/// The child's arguments, `None` when `args` does not ask for this mode.
fn parse(args: &[OsString]) -> Option<Result<(PathBuf, PathBuf), String>> {
    let at = args.iter().position(|a| a == PROBE_ARG)?;
    let folder = args.get(at + 1);
    let model = args
        .iter()
        .position(|a| a == "--model")
        .and_then(|i| args.get(i + 1));
    Some(match (folder, model) {
        (Some(f), Some(m)) => Ok((PathBuf::from(f), PathBuf::from(m))),
        _ => Err(format!("{PROBE_ARG} needs a folder and --model")),
    })
}

/// Runs the child when `args` asks for it, and returns its exit code.
#[must_use]
pub fn child_main(args: &[OsString]) -> Option<i32> {
    let parsed = parse(args)?;
    Some(match parsed {
        Err(_) => 2,
        Ok((folder, model)) => {
            if earshot_runtime::host::load_llama_folder(&folder).is_err() {
                return Some(3);
            }
            // A failed trial says nothing the parent could not say with its exit code.
            match earshot_runtime::host::probe_llama_gpu(&model) {
                Ok(backend) => {
                    println!("{OK_LINE}{}", backend.name());
                    0
                }
                Err(_) => 4,
            }
        }
    })
}

/// The verdict of a child that exited with `code` and printed `stdout`. Only a clean exit that
/// names a backend counts: a crash, or a clean exit with no such line, is a refusal.
#[must_use]
pub fn verdict_of(code: Option<i32>, stdout: &str) -> Verdict {
    let refused = |what: String| Verdict::Refused { why: what };
    match code {
        Some(0) => stdout
            .lines()
            .find_map(|l| l.trim().strip_prefix(OK_LINE))
            .map_or_else(
                || refused("the trial named no backend".to_owned()),
                |b| Verdict::Usable {
                    backend: b.trim().to_owned(),
                },
            ),
        Some(c) => refused(format!(
            "the trial ended with exit code {:#x}",
            c.cast_unsigned()
        )),
        None => refused("the trial was stopped".to_owned()),
    }
}

/// Tries `folder` with the GGUF `model` in a child of this exe.
#[must_use]
pub fn try_folder(folder: &Path, model: &Path) -> Verdict {
    try_folder_within(folder, model, PROBE_DEADLINE)
}

/// [`try_folder`] with a time limit of the caller's: a trial past it is a refusal.
#[must_use]
pub fn try_folder_within(folder: &Path, model: &Path, deadline: Duration) -> Verdict {
    let Ok(program) = std::env::current_exe() else {
        return Verdict::Refused {
            why: "the app could not find itself".to_owned(),
        };
    };
    let spec = earshot_proc::ChildSpec {
        program,
        args: vec![
            OsString::from(PROBE_ARG),
            folder.as_os_str().to_owned(),
            OsString::from("--model"),
            model.as_os_str().to_owned(),
        ],
        current_dir: std::env::temp_dir(),
        env_remove: Vec::new(),
        env_set: Vec::new(),
        stdin: Vec::new(),
        deadline,
        // llama.cpp logs its load; only the last line matters, and output past this is dropped.
        max_output: 4 * 1024 * 1024,
    };
    match earshot_proc::run(&spec, &|| false) {
        Ok(out) => verdict_of(out.status.code(), &String::from_utf8_lossy(&out.stdout)),
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
    fn llama_probe_is_asked_for_only_by_its_flag_and_needs_both_paths() {
        assert_eq!(parse(&args(&["--replay", "x"])), None);
        assert_eq!(
            parse(&args(&[PROBE_ARG, "C:/set", "--model", "m.gguf"])),
            Some(Ok((PathBuf::from("C:/set"), PathBuf::from("m.gguf"))))
        );
        assert!(parse(&args(&[PROBE_ARG, "C:/set"])).unwrap().is_err());
        assert!(parse(&args(&[PROBE_ARG])).unwrap().is_err());
    }

    #[test]
    fn llama_probe_only_a_clean_exit_naming_a_backend_is_usable() {
        let usable = |b: &str| Verdict::Usable {
            backend: b.to_owned(),
        };
        // llama.cpp's log may surround the line.
        assert_eq!(
            verdict_of(Some(0), "load_tensors: x\nprobe: ok cuda\nmore\n"),
            usable("cuda")
        );
        assert_eq!(
            verdict_of(Some(0), "probe: ok vulkan\r\n"),
            usable("vulkan")
        );
        for (code, stdout, word) in [
            (Some(0), "nothing useful\n", "no backend"),
            // What a card with no matching code does (0xC0000409 as Windows reports it).
            (Some(-1_073_740_791), "", "0xc0000409"),
            // A crash after the line still is a crash.
            (Some(-1_073_740_791), "probe: ok cuda\n", "0xc0000409"),
            (Some(3), "", "0x3"),
            (None, "probe: ok cuda\n", "stopped"),
        ] {
            let Verdict::Refused { why } = verdict_of(code, stdout) else {
                panic!("{code:?} {stdout}");
            };
            assert!(why.contains(word), "{why}");
        }
    }
}
