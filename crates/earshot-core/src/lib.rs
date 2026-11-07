//! Sans-IO core (no OS dependencies): domain, DSP, AEC, pipeline state machine, diarisation clustering, voiceprint matching, summary schema, Obsidian renderer, metrics and call simulator.

#![forbid(unsafe_code)]

pub mod aec;
pub mod asr;
pub mod audio;
pub mod audio_check;
pub mod capture;
pub mod diar;
pub mod dsp;
pub mod export;
pub mod identity;
pub mod metrics;
pub mod model;
pub mod pipeline;
pub mod prompt;
pub mod sim;
pub mod summary;
pub mod voiceprint;

#[cfg(test)]
mod testutil;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert_eq!(env!("CARGO_PKG_NAME"), "earshot-core");
    }
}
