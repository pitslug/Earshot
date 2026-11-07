//! Writes the calibration inputs for quantising a Parakeet encoder for the NPU (T8.3):
//! NeMo log-mel features ([`earshot_onnx::nemo_mel::NemoMel`], the production front end) of
//! LibriSpeech dev-clean speech, at the fixed encoder input shape
//! ([`earshot_onnx::parakeet::STATIC_FRAMES`] frames, one 30 s piece).
//!
//! `cargo run --release -p earshot-onnx --example parakeet_calibration -- <n_mels> <plan>
//! <out dir>`; normally run by `cargo xtask qnn-convert`, which chooses the utterances
//! (`xtask::qnn_convert::calibration_plan`: dev-clean only, never the pinned subset, whose
//! dev-clean half is the dev WER set, nor the test-clean or smoke sets the accuracy is
//! measured on) and writes the plan: one piece per line, its FLAC files separated by tabs.
//! The files of a piece are joined with 0.5 s of silence, the way the piece planner merges
//! speech, and cut at 30 s. Each piece is `cal_NNNN.npy` (float32 `[1, n_mels, frames]`,
//! frames past the valid ones zero) and `cal_NNNN_len.npy` (int64 `[1]`, valid frames).

use std::io::Write;
use std::path::{Path, PathBuf};

use earshot_onnx::nemo_mel::NemoMel;
use earshot_onnx::parakeet::{STATIC_FRAMES, static_features};

const RATE: usize = 16_000;
const GAP: usize = RATE / 2;
const MAX: usize = 30 * RATE;

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("parakeet_calibration: {msg}");
    std::process::exit(1);
}

fn decode_flac(path: &Path) -> Vec<f32> {
    let mut reader = claxon::FlacReader::open(path)
        .unwrap_or_else(|e| fail(format!("cannot open {}: {e}", path.display())));
    let info = reader.streaminfo();
    if info.sample_rate != 16_000 || info.channels != 1 {
        fail(format!("{}: not 16 kHz mono", path.display()));
    }
    let scale = 1.0 / f64::from(1u32 << (info.bits_per_sample - 1));
    reader
        .samples()
        .map(|s| (f64::from(s.unwrap_or_else(|e| fail(e))) * scale) as f32)
        .collect()
}

/// A minimal NumPy `.npy` (format 1.0) writer for little-endian float32 or int64.
fn write_npy(path: &Path, descr: &str, shape: &[usize], data: &[u8]) {
    let dims: Vec<String> = shape.iter().map(ToString::to_string).collect();
    let shape = if dims.len() == 1 {
        format!("({},)", dims[0])
    } else {
        format!("({})", dims.join(", "))
    };
    let mut header = format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': {shape}, }}");
    // Magic (6) + version (2) + length (2) + header + newline, padded to 64 bytes.
    let total = 10 + header.len() + 1;
    header.push_str(&" ".repeat((64 - total % 64) % 64));
    header.push('\n');
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    let len = u16::try_from(header.len()).unwrap_or_else(|_| fail("npy header too long"));
    f.write_all(b"\x93NUMPY\x01\x00")
        .and_then(|()| f.write_all(&len.to_le_bytes()))
        .and_then(|()| f.write_all(header.as_bytes()))
        .and_then(|()| f.write_all(data))
        .unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [n_mels, plan, out] = args.as_slice() else {
        fail("usage: parakeet_calibration <n_mels> <plan file> <out dir>");
    };
    let n_mels: usize = n_mels.parse().unwrap_or_else(|e| fail(e));
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out).unwrap_or_else(|e| fail(e));
    let plan = std::fs::read_to_string(plan).unwrap_or_else(|e| fail(format!("{plan}: {e}")));
    let mel = NemoMel::new(n_mels);
    let mut written = 0usize;
    for line in plan.lines().filter(|l| !l.trim().is_empty()) {
        let mut piece: Vec<f32> = Vec::new();
        for f in line.split('\t') {
            let audio = decode_flac(Path::new(f));
            if !piece.is_empty() {
                piece.extend(std::iter::repeat_n(0.0, GAP));
            }
            let room = MAX.saturating_sub(piece.len());
            piece.extend_from_slice(&audio[..audio.len().min(room)]);
        }
        let (features, valid) = static_features(&mel, &piece)
            .unwrap_or_else(|e| fail(format!("piece of {} samples: {e}", piece.len())));
        let bytes: Vec<u8> = features.iter().flat_map(|v| v.to_le_bytes()).collect();
        write_npy(
            &out.join(format!("cal_{written:04}.npy")),
            "<f4",
            &[1, n_mels, STATIC_FRAMES],
            &bytes,
        );
        let len = i64::try_from(valid).unwrap_or(0).to_le_bytes();
        write_npy(
            &out.join(format!("cal_{written:04}_len.npy")),
            "<i8",
            &[1],
            &len,
        );
        written += 1;
    }
    if written == 0 {
        fail("the plan lists no pieces");
    }
    println!(
        "parakeet_calibration: {written} pieces of {n_mels} x {STATIC_FRAMES} in {}",
        out.display()
    );
}
