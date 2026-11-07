//! ONNX Runtime adapter: Silero VAD, diarisation segmentation, speaker embedding, Parakeet speech
//! recognition and execution-provider selection (including Qualcomm's QNN plugin for the Snapdragon
//! NPU, [`qnn`]).
//!
//! ONNX Runtime itself is Microsoft's official 1.31 release, loaded at run time with
//! [`init`] (see [`runtime`]); load it before creating any model.

pub mod embed;
pub mod fbank;
pub mod nemo_mel;
pub mod parakeet;
pub mod qnn;
pub mod runtime;
pub mod segment;
pub mod shared_weights;
pub mod silero;

pub use runtime::{
    CPU_EP, CUDA_EP, ORT_CPU_DYLIB, ORT_CPU_DYLIB_ARM64, ORT_CPU_DYLIB_ARM64_SHA256,
    ORT_CPU_DYLIB_SHA256, ORT_CPU_DYLIB_X64, ORT_CPU_DYLIB_X64_SHA256, ORT_CPU_SET, ORT_CUDA_DYLIB,
    ORT_CUDA_DYLIB_SHA256, ORT_VERSION, OnnxError, OnnxSession, Placement, Provider, QNN_EP,
    SessionBackend, SessionConfig, ThreadStart, environment, init, loaded_library, preload_library,
};
