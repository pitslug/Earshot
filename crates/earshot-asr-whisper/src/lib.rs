//! Speech recognition on whisper.cpp via whisper-rs, with CPU, CUDA and Vulkan backends.
//!
//! whisper-rs 0.16.0 (whisper.cpp 1.8.3) runs on the CPU by default, `cuda` adds CUDA and
//! `vulkan` builds the Vulkan backend (`cargo xtask build --features vulkan`, T2.6; chosen
//! automatically after CUDA since T6.10d). [`WhisperConfig::for_backend`] picks the device of a
//! backend and [`gpu_devices`] lists the GPUs whisper.cpp can use. [`WhisperTranscriber`] implements
//! [`earshot_core::asr::Transcriber`]: load a ggml model with [`WhisperConfig`], then
//! transcribe 16 kHz mono buffers; [`earshot_core::asr::Transcriber::backend_info`] says
//! which device whisper.cpp actually computes on. Streaming goes through
//! `earshot_core::asr::StreamingChunker` over this transcriber.
//!
//! [`evidence`] holds the one rule for trusting a reported backend: `backend_info()` and
//! whisper.cpp's own log must agree (whisper.cpp falls back to the CPU silently when a GPU
//! cannot initialise).
//!
//! All `unsafe` code (ggml device registry, abort callback) is in minimal blocks with
//! `SAFETY` comments in `backend.rs` and `transcriber.rs`.

mod backend;
mod decode;
pub mod dtw;
pub mod evidence;
mod transcriber;

pub use backend::{GpuDevice, gpu_devices};
pub use dtw::WordTiming;
pub use transcriber::{FallbackStats, MIN_INPUT_SAMPLES, WhisperConfig, WhisperTranscriber};
