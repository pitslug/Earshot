//! Whisper encoder/decoder on the ONNX Runtime QNN execution provider (Snapdragon NPU).

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "earshot-asr-qnn");
    }
}
