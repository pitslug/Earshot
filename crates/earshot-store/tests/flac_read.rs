//! T14.25: FLAC reads are bounded: a span decodes only up to its end and holds only its own
//! samples, and a header's sample count never sizes an allocation the file could not fill
//! (`cargo test -p earshot-store flac`).

mod common;

use std::path::{Path, PathBuf};

use common::TempDir;
use earshot_store::{FlacTrackWriter, read_flac, read_flac_span};

/// Samples of an invented signal, several 4096-sample frames and a short last one.
fn signal(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| 0.5 * (i as f32 * 0.023).sin() - 0.25 * (i as f32 * 0.11).cos())
        .collect()
}

fn written(dir: &TempDir, name: &str, samples: &[f32]) -> PathBuf {
    let path = dir.path().join(name);
    let mut w = FlacTrackWriter::create(&path, 16_000).unwrap();
    w.write(samples).unwrap();
    w.finish().unwrap();
    path
}

/// What `read_flac` gives for `[from, to)`, clamped to the file as the speaker sample clamps.
fn whole_then_cut(path: &Path, from: u64, to: u64) -> Vec<f32> {
    let (_, all) = read_flac(path).unwrap();
    let a = usize::try_from(from).unwrap_or(usize::MAX).min(all.len());
    let b = usize::try_from(to).unwrap_or(usize::MAX).min(all.len());
    if a >= b {
        Vec::new()
    } else {
        all[a..b].to_vec()
    }
}

#[test]
fn flac_read_span_gives_the_samples_of_the_whole_read_on_every_span() {
    let dir = TempDir::new("flac-span");
    let total = 4096 * 4 + 777;
    let path = written(&dir, "t.flac", &signal(total));
    let total = total as u64;
    let spans = [
        (0, 0),
        (0, 1),
        (0, 4096),
        (4095, 4097),
        (1_000, 9_000),
        (8_192, 12_288),
        (total - 10, total),
        (total - 10, total + 5_000),
        (total, total + 1),
        (total + 100, total + 200),
        (5_000, 4_000),
        (0, total),
        (0, u64::MAX),
    ];
    for (from, to) in spans {
        let (rate, got) = read_flac_span(&path, from, to).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(got, whole_then_cut(&path, from, to), "[{from}, {to})");
        let want = to.saturating_sub(from).min(total.saturating_sub(from));
        assert!(
            got.capacity() as u64 <= want,
            "[{from}, {to}): holds {} for {want} samples",
            got.capacity()
        );
    }
}

#[test]
fn flac_read_span_stops_at_its_end_and_never_reads_the_frames_after_it() {
    let dir = TempDir::new("flac-span-stops");
    let samples = signal(4096 * 5 + 300);
    let path = written(&dir, "t.flac", &samples);
    let (_, before) = read_flac_span(&path, 100, 4_000).unwrap();
    // Damage everything after the first two frames: a read that went on would fail there.
    let bytes = std::fs::read(&path).unwrap();
    // The end of frame 2 is where a span of the first 8192 samples stops reading: the shortest
    // prefix ending before a frame sync code (0xFFF8) that still decodes those samples.
    let mut first_two = 0;
    for cut in (43..bytes.len() - 1).filter(|&p| bytes[p] == 0xFF && bytes[p + 1] == 0xF8) {
        std::fs::write(&path, &bytes[..cut]).unwrap();
        if read_flac_span(&path, 0, 8_192).is_ok_and(|(_, s)| s.len() == 8_192) {
            first_two = cut;
            break;
        }
    }
    assert!(first_two > 0 && first_two < bytes.len());
    let mut damaged = bytes[..first_two].to_vec();
    damaged.extend(std::iter::repeat_n(0xA5u8, bytes.len() - first_two));
    std::fs::write(&path, &damaged).unwrap();
    assert!(read_flac(&path).is_err(), "the whole read meets the damage");
    let (_, after) = read_flac_span(&path, 100, 4_000).unwrap();
    assert_eq!(after, before, "a span before the damage never reads it");
    assert_eq!(
        read_flac_span(&path, 4_096, 8_192).unwrap().1,
        whole_then_cut(&written(&dir, "u.flac", &samples), 4_096, 8_192)
    );
}

/// The STREAMINFO total-samples field (36 bits, file bytes 21 to 25) set to its largest value.
fn claim_most_samples(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"fLaC");
    // Bytes 18..26 hold rate (20 bits), channels (3), bits per sample (5), samples (36).
    let mut field = u64::from_be_bytes(bytes[18..26].try_into().unwrap());
    field |= (1u64 << 36) - 1;
    bytes[18..26].copy_from_slice(&field.to_be_bytes());
    std::fs::write(path, bytes).unwrap();
    let claimed = claxon::FlacReader::open(path).unwrap().streaminfo().samples;
    assert_eq!(
        claimed,
        Some((1u64 << 36) - 1),
        "the header now claims 2^36 - 1"
    );
}

#[test]
fn flac_read_a_header_claiming_more_than_the_file_holds_allocates_no_more_than_it_could() {
    let dir = TempDir::new("flac-claim");
    let samples = signal(4096 + 50);
    let path = written(&dir, "t.flac", &samples);
    let (_, honest) = read_flac(&path).unwrap();
    claim_most_samples(&path);
    let len = std::fs::metadata(&path).unwrap().len();
    // No FLAC frame is under 10 bytes or over 65536 samples: the most the file could hold.
    let could_hold = len * 65_536 / 10;
    let (_, got) = read_flac(&path).unwrap();
    assert_eq!(got, honest, "the real samples, whatever the header says");
    assert!(
        got.capacity() as u64 <= could_hold,
        "held {} for a file of {len} bytes",
        got.capacity()
    );
    let (_, span) = read_flac_span(&path, 10, u64::MAX).unwrap();
    assert_eq!(span, honest[10..].to_vec());
    assert!(span.capacity() as u64 <= could_hold);
}
