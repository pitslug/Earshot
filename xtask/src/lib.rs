//! Project automation library behind `cargo xtask`.
//!
//! The binary in `main.rs` is a thin clap front end; the logic lives here so integration
//! tests (for example `tests/thresholds.rs`) can use the same code paths.

pub mod accept;
pub mod aec_rumble;
pub mod archive;
pub mod bench;
pub mod bench_meeting;
pub mod build;
pub mod bundle;
pub mod ci;
pub mod common_words;
pub mod doctor;
pub mod download;
pub mod echo_nl;
pub mod fixtures;
pub mod flac;
pub mod llama_dlls;
pub mod llama_guard;
pub mod manifest;
pub mod model_eval;
pub mod models;
pub mod native_flags;
pub mod net_guard;
pub mod proc;
pub mod public_guard;
pub mod purity;
pub mod qnn_convert;
pub mod qnn_etw;
pub mod qnn_pack;
pub mod schema;
pub mod sum_eval;
pub mod t613;
pub mod thresholds;
pub mod timefmt;
pub mod ui;
pub mod update;
pub mod vulkan_parity;

use std::path::{Path, PathBuf};

/// Repository root (the parent of the `xtask` crate directory).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the repo root")
        .to_path_buf()
}
