//! Streaming FLAC writer (flacenc, single-threaded) and reader (claxon).
//!
//! The writer appends encoded frames to `<path>.part` behind a placeholder header of the
//! final size (`fLaC` + STREAMINFO block = 42 bytes, RFC 9639 sections 8.1-8.2). `finish`
//! encodes the last partial block, rewrites the header with the real STREAMINFO (sample
//! count, block and frame sizes, MD5), syncs the file and renames it to `<path>`. A crash
//! before that leaves only the `.part` file, which is never overwritten.
//!
//! A write error never deletes audio (T9.17): each encoded frame is written to the file as
//! one piece, and the writer remembers the length and stream info of the frames that were
//! written whole. After the first failed write it takes no more samples, and
//! [`FlacTrackWriter::finish`] finishes the file as far as it got: the torn tail is cut
//! off, the header says how many samples the whole frames hold (MD5 zero, which FLAC allows
//! for "unknown"), and the file is renamed. If even that fails, the `.part` is left in place
//! for recovery (T9.18). Only [`FlacTrackWriter::abort`] deletes, and only callers whose
//! recording never started use it.
//!
//! Every frame goes to the file in one unbuffered write as soon as its 4096 samples (256 ms at
//! 16 kHz) are in, so a killed process loses at most the samples of the frame being filled:
//! the operating system holds what was written. Nothing syncs during the recording (a power
//! loss may lose what the system had not yet written to disk).
//!
//! Recovery (T9.18): [`recover_part`] turns an interrupted `.part` into a finished file. It
//! decodes the frames after the placeholder header one by one, each checked whole (sync code,
//! header CRC-8, subframes, frame CRC-16, frame number, block size), cuts the file after the
//! last whole frame and writes the real STREAMINFO (sample count from the frames, MD5 zero).
//! A `.part` with no whole frame is left alone.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use earshot_core::audio::{f32_to_i16, i16_to_f32};
use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, Stream, StreamInfo};
use flacenc::config;
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};

use crate::db::StoreError;

/// `fLaC` + metadata block header + 34-byte STREAMINFO.
const HEADER_LEN: usize = 4 + 4 + 34;
const BITS: usize = 16;

fn audio_err(what: &str, e: impl std::fmt::Debug) -> StoreError {
    StoreError::Audio(format!("{what}: {e:?}"))
}

/// A finished FLAC file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedFlac {
    /// Final path.
    pub path: PathBuf,
    /// Sample rate.
    pub sample_rate: u32,
    /// Frames (samples, mono).
    pub frames: u64,
    /// File size.
    pub size_bytes: u64,
    /// A write failed during the recording, so the file holds only the audio before it
    /// (T9.17; the module docs).
    pub cut_short: bool,
}

/// A failure injected into a writer's file, for tests of what a failed write keeps (T9.17).
/// A seam on the file, never a real full disk; production never sets one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteFault {
    /// After this many more bytes appended (the placeholder header not counted), every
    /// append fails as a full disk does (`ErrorKind::StorageFull`), part of the write that
    /// crosses the limit landing first. Overwriting the header, syncing and renaming still
    /// work, as they do on a full disk.
    DiskFullAfter(u64),
    /// After this many more bytes appended, every write, sync and truncation fails (a drive
    /// that went away), so the file cannot be finished and its `.part` stays.
    BrokenAfter(u64),
}

/// Writes one mono track as 16-bit FLAC, frame by frame.
pub struct FlacTrackWriter {
    path: PathBuf,
    part: PathBuf,
    file: File,
    config: Verified<config::Encoder>,
    block: usize,
    buf: (FrameBuf, Context),
    info: StreamInfo,
    pending: Vec<i32>,
    sample_rate: u32,
    /// Length of the file's whole frames (header included): everything after it is a torn
    /// tail from a failed write.
    whole_len: u64,
    /// The first write error, after which no sample is taken.
    failed: Option<String>,
    /// A failure injected by a test ([`FlacTrackWriter::inject_fault`]) and the bytes left
    /// before it.
    fault: Option<WriteFault>,
}

impl std::fmt::Debug for FlacTrackWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlacTrackWriter")
            .field("path", &self.path)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

fn part_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

impl FlacTrackWriter {
    /// Starts `<path>.part` (creating parent directories).
    ///
    /// # Errors
    /// [`StoreError::Io`] if `path` or `<path>.part` already exists (finished or crashed
    /// audio is never overwritten) or cannot be created; [`StoreError::Audio`] for a sample
    /// rate FLAC cannot store.
    pub fn create(path: &Path, sample_rate: u32) -> Result<Self, StoreError> {
        if path.exists() {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} already exists", path.display()),
            )));
        }
        let mut cfg = config::Encoder::default();
        cfg.multithread = false;
        let config = cfg
            .into_verified()
            .map_err(|(_, e)| audio_err("encoder config", e))?;
        let block = config.block_size;
        let info = StreamInfo::new(sample_rate as usize, 1, BITS)
            .map_err(|e| audio_err("stream info", e))?;
        let buf = (
            FrameBuf::with_size(1, block).map_err(|e| audio_err("frame buffer", e))?,
            Context::new(BITS, 1),
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let part = part_path(path);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)?;
        file.write_all(&[0u8; HEADER_LEN])?;
        Ok(FlacTrackWriter {
            path: path.to_owned(),
            part,
            file,
            config,
            block,
            buf,
            info,
            pending: Vec::with_capacity(block),
            sample_rate,
            whole_len: HEADER_LEN as u64,
            failed: None,
            fault: None,
        })
    }

    /// Makes this writer's file fail as `fault` says (a test seam, T9.17; the bytes count
    /// from now).
    #[doc(hidden)]
    pub fn inject_fault(&mut self, fault: WriteFault) {
        self.fault = Some(fault);
    }

    /// Whether a write failed, so the writer takes no more samples.
    #[must_use]
    pub fn has_failed(&self) -> bool {
        self.failed.is_some()
    }

    /// Samples the writer took: what a finish keeps when no write failed (the whole frames plus
    /// the samples of the frame being filled), else [`FlacTrackWriter::whole_frames`].
    #[must_use]
    pub fn frames_taken(&self) -> u64 {
        if self.failed.is_some() {
            self.whole_frames()
        } else {
            self.whole_frames() + self.pending.len() as u64
        }
    }

    /// Samples in the file's whole frames: what a finish keeps after a failed write.
    #[must_use]
    pub fn whole_frames(&self) -> u64 {
        self.info.total_samples() as u64
    }

    /// Appends `bytes` to the file through the injected fault, if any.
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let (limit, broken) = match self.fault {
            None => return self.file.write_all(bytes),
            Some(WriteFault::DiskFullAfter(n)) => (n, false),
            Some(WriteFault::BrokenAfter(n)) => (n, true),
        };
        let fits = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        self.file.write_all(&bytes[..fits])?;
        let left = limit - fits as u64;
        self.fault = Some(if broken {
            WriteFault::BrokenAfter(left)
        } else {
            WriteFault::DiskFullAfter(left)
        });
        if fits == bytes.len() {
            return Ok(());
        }
        Err(if broken {
            std::io::Error::other("the drive stopped answering (injected)")
        } else {
            std::io::Error::from(std::io::ErrorKind::StorageFull)
        })
    }

    /// Fails a non-append file operation when the injected fault is a broken drive whose
    /// budget is spent.
    fn broken(&self) -> std::io::Result<()> {
        match self.fault {
            Some(WriteFault::BrokenAfter(0)) => Err(std::io::Error::other(
                "the drive stopped answering (injected)",
            )),
            _ => Ok(()),
        }
    }

    /// The final path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends samples in `[-1, 1]` (clamped, rounded to 16 bits).
    ///
    /// # Errors
    /// Encoding or I/O errors. After the first error the writer takes no more samples (every
    /// later call fails at once) and [`FlacTrackWriter::finish`] keeps the whole frames
    /// written before it.
    pub fn write(&mut self, samples: &[f32]) -> Result<(), StoreError> {
        if let Some(e) = &self.failed {
            return Err(StoreError::Audio(format!(
                "the recording stopped after an earlier error: {e}"
            )));
        }
        for &x in samples {
            self.pending.push(i32::from(f32_to_i16(x)));
            if self.pending.len() == self.block {
                self.encode_pending()?;
            }
        }
        Ok(())
    }

    /// Encodes the pending samples as one frame and appends it; any error marks the writer
    /// failed.
    fn encode_pending(&mut self) -> Result<(), StoreError> {
        let result = self.try_encode_pending();
        if let Err(e) = &result {
            self.failed.get_or_insert_with(|| e.to_string());
        }
        result
    }

    fn try_encode_pending(&mut self) -> Result<(), StoreError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.buf
            .fill_interleaved(&self.pending)
            .map_err(|e| audio_err("fill", e))?;
        let n = self
            .buf
            .1
            .current_frame_number()
            .ok_or_else(|| audio_err("frame number", "none"))?;
        let frame = flacenc::encode_fixed_size_frame(&self.config, &self.buf.0, n, &self.info)
            .map_err(|e| audio_err("encode", e))?;
        let mut sink = ByteSink::new();
        frame.write(&mut sink).map_err(|e| audio_err("frame", e))?;
        self.append(sink.as_slice())?;
        // Only a frame written whole counts: the stream info and the length describe the
        // frames a finish after a later failure keeps.
        self.info.update_frame_info(&frame);
        self.whole_len += sink.as_slice().len() as u64;
        self.pending.clear();
        Ok(())
    }

    /// Encodes the remaining samples, writes the real header, syncs and renames
    /// `<path>.part` to `<path>`. After a failed write (or when encoding the rest fails) it
    /// keeps the whole frames written before the failure instead
    /// ([`FinishedFlac::cut_short`], module docs).
    ///
    /// # Errors
    /// Encoding or I/O errors while finishing the file (the `.part` file is left in place).
    pub fn finish(mut self) -> Result<FinishedFlac, StoreError> {
        if self.failed.is_none() {
            // A failure here is the first one: what was written before it is kept.
            let _ = self.encode_pending();
        }
        let cut_short = self.failed.is_some();
        let frames = self.info.total_samples() as u64;
        if cut_short {
            // The samples of the lost frames went into the MD5 too: zero means unknown.
            self.info.set_md5_digest(&[0; 16]);
        } else {
            self.info.set_md5_digest(&self.buf.1.md5_digest());
        }
        if frames == 0 {
            self.info
                .set_block_sizes(self.block, self.block)
                .map_err(|e| audio_err("block sizes", e))?;
            self.info
                .set_frame_sizes(0, 0)
                .map_err(|e| audio_err("frame sizes", e))?;
        }
        let mut sink = ByteSink::new();
        Stream::with_stream_info(self.info.clone())
            .write(&mut sink)
            .map_err(|e| audio_err("header", e))?;
        let header = sink.as_slice();
        if header.len() != HEADER_LEN {
            return Err(audio_err("header length", header.len()));
        }
        self.broken()?;
        if cut_short {
            // Drop the torn tail of the write that failed.
            self.file.set_len(self.whole_len)?;
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(header)?;
        self.file.sync_all()?;
        let size_bytes = self.file.metadata()?.len();
        let FlacTrackWriter {
            path,
            part,
            file,
            sample_rate,
            ..
        } = self;
        drop(file);
        std::fs::rename(&part, &path)?;
        Ok(FinishedFlac {
            path,
            sample_rate,
            frames,
            size_bytes,
            cut_short,
        })
    }

    /// Gives up: deletes `<path>.part`. Only for a recording that never started (no audio
    /// was ever given to the writer); a recording that failed part way is finished instead
    /// (module docs).
    ///
    /// # Errors
    /// I/O errors.
    pub fn abort(self) -> Result<(), StoreError> {
        let part = self.part.clone();
        drop(self);
        Ok(std::fs::remove_file(part)?)
    }
}

/// Reads a 16-bit mono FLAC file: `(sample_rate, samples in [-1, 1])`.
///
/// # Errors
/// I/O or decoding errors, or a file that is not 16-bit mono.
pub fn read_flac(path: &Path) -> Result<(u32, Vec<f32>), StoreError> {
    let mut r = claxon::FlacReader::open(path).map_err(|e| audio_err("open flac", e))?;
    let info = r.streaminfo();
    if info.channels != 1 || info.bits_per_sample != BITS as u32 {
        return Err(StoreError::Audio(format!(
            "{}: expected 16-bit mono, got {} channels of {} bits",
            path.display(),
            info.channels,
            info.bits_per_sample
        )));
    }
    let mut out = Vec::with_capacity(info.samples.unwrap_or(0) as usize);
    for s in r.samples() {
        let s = s.map_err(|e| audio_err("decode flac", e))?;
        let s = i16::try_from(s).map_err(|_| audio_err("sample out of range", s))?;
        out.push(i16_to_f32(s));
    }
    Ok((info.sample_rate, out))
}

/// What [`recover_part`] did with an interrupted recording's `<path>.part` (T9.18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartRecovery {
    /// The `.part`'s whole frames are now the finished file at the final path: the torn tail
    /// cut off, the real STREAMINFO written with MD5 zero (FLAC's "unknown"), synced, renamed.
    Recovered {
        /// The finished file ([`FinishedFlac::cut_short`] is true: it ends where the recording
        /// stopped).
        file: FinishedFlac,
        /// Bytes after the whole frames (a torn last frame) that were cut off.
        torn_bytes: u64,
    },
    /// The `.part` holds no whole frame: it was left exactly as it was.
    NoWholeFrame,
    /// Another process holds the `.part` open (a recording in progress there): not touched.
    InUse,
}

/// A claxon byte reader that counts the bytes it hands out, so the end of each decoded frame is
/// known to the byte.
struct Counting<R> {
    inner: R,
    pos: u64,
}

impl<R: claxon::input::ReadBytes> claxon::input::ReadBytes for Counting<R> {
    fn read_u8(&mut self) -> std::io::Result<u8> {
        let b = self.inner.read_u8()?;
        self.pos += 1;
        Ok(b)
    }

    fn read_u8_or_eof(&mut self) -> std::io::Result<Option<u8>> {
        let b = self.inner.read_u8_or_eof()?;
        if b.is_some() {
            self.pos += 1;
        }
        Ok(b)
    }

    fn read_into(&mut self, buffer: &mut [u8]) -> std::io::Result<()> {
        self.inner.read_into(buffer)?;
        self.pos += buffer.len() as u64;
        Ok(())
    }

    fn skip(&mut self, amount: u32) -> std::io::Result<()> {
        self.inner.skip(amount)?;
        self.pos += u64::from(amount);
        Ok(())
    }
}

/// The whole frames found after the header of a `.part`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WholeFrames {
    /// Samples in them.
    samples: u64,
    /// Smallest block (samples), counted as flacenc counts it: the last block too (RFC 9639
    /// section 8.2 leaves the last block out; counting it keeps a recovered header byte-equal
    /// to the one the writer would have written).
    min_block: u32,
    /// Largest block (samples).
    max_block: u32,
    /// Smallest frame (bytes).
    min_frame: u64,
    /// Largest frame (bytes).
    max_frame: u64,
    /// Where they end in the file (header included).
    end: u64,
}

/// Scans the frames after the 42-byte header. A frame counts only when it decodes whole (sync
/// code, header CRC-8, every subframe, frame CRC-16: claxon checks each), is mono, carries the
/// next frame number and keeps the stream's block size (only the last frame may be shorter).
/// The first frame that fails any of these ends the scan: it and everything after it are a
/// torn tail. `None` when no frame is whole.
///
/// A truncated frame can never pass: the frame's own structure says how many bytes it needs,
/// so a cut anywhere inside it runs out of bytes before its CRC-16 is read.
fn scan_frames(file: &File) -> Option<WholeFrames> {
    let mut reader = Counting {
        inner: claxon::input::BufferedReader::new(file),
        pos: 0,
    };
    let mut header = [0u8; HEADER_LEN];
    claxon::input::ReadBytes::read_into(&mut reader, &mut header).ok()?;
    let mut found: Option<WholeFrames> = None;
    let mut buffer = Vec::new();
    let mut index: u64 = 0;
    loop {
        let start = reader.pos;
        // A frame reader holds no state between frames; a fresh one per frame lets the count
        // be read back after each.
        let block = match claxon::frame::FrameReader::new(&mut reader)
            .read_next_or_eof(std::mem::take(&mut buffer))
        {
            Ok(Some(block)) => block,
            Ok(None) | Err(_) => break,
        };
        let size = block.duration();
        let bytes = reader.pos - start;
        let numbered = block.time() == index * u64::from(size);
        // After a shorter (last) block nothing may follow, and no block is longer than the
        // first.
        let fits = found.is_none_or(|f| f.min_block == f.max_block && size <= f.max_block);
        if block.channels() != 1 || size == 0 || !numbered || !fits {
            break;
        }
        found = Some(match found {
            None => WholeFrames {
                samples: u64::from(size),
                min_block: size,
                max_block: size,
                min_frame: bytes,
                max_frame: bytes,
                end: reader.pos,
            },
            Some(f) => WholeFrames {
                samples: f.samples + u64::from(size),
                min_block: f.min_block.min(size),
                max_block: f.max_block.max(size),
                min_frame: f.min_frame.min(bytes),
                max_frame: f.max_frame.max(bytes),
                end: reader.pos,
            },
        });
        index += 1;
        buffer = block.into_buffer();
    }
    found
}

/// The FLAC sample-rate code a frame header uses for `hz` (RFC 9639 section 9.1.2), when the
/// 4-bit table has one.
fn rate_code(hz: u32) -> Option<u8> {
    Some(match hz {
        88_200 => 1,
        176_400 => 2,
        192_000 => 3,
        8_000 => 4,
        16_000 => 5,
        22_050 => 6,
        24_000 => 7,
        32_000 => 8,
        44_100 => 9,
        48_000 => 10,
        96_000 => 11,
        _ => return None,
    })
}

/// `fLaC` + the only metadata block, STREAMINFO (RFC 9639 section 8.2), MD5 zero. The same
/// bytes flacenc writes for the same frames apart from the MD5 (pinned by a test).
fn stream_header(sample_rate: u32, f: &WholeFrames) -> Result<[u8; HEADER_LEN], StoreError> {
    let too_big = |what: &str| StoreError::Audio(format!("{what} does not fit a FLAC header"));
    let min_block = u16::try_from(f.min_block).map_err(|_| too_big("block size"))?;
    let max_block = u16::try_from(f.max_block).map_err(|_| too_big("block size"))?;
    if f.max_frame >= 1 << 24 {
        return Err(too_big("frame size"));
    }
    if f.samples >= 1 << 36 {
        return Err(too_big("sample count"));
    }
    if sample_rate == 0 || sample_rate >= 1 << 20 {
        return Err(too_big("sample rate"));
    }
    let mut h = [0u8; HEADER_LEN];
    h[..4].copy_from_slice(b"fLaC");
    // Last metadata block, type 0 (STREAMINFO), 34 bytes.
    h[4..8].copy_from_slice(&[0x80, 0, 0, 34]);
    h[8..10].copy_from_slice(&min_block.to_be_bytes());
    h[10..12].copy_from_slice(&max_block.to_be_bytes());
    h[12..15].copy_from_slice(&f.min_frame.to_be_bytes()[5..]);
    h[15..18].copy_from_slice(&f.max_frame.to_be_bytes()[5..]);
    // Sample rate (20 bits), channels - 1 (3 bits, mono: 0), bits per sample - 1 (5 bits),
    // samples (36 bits).
    let packed = (u64::from(sample_rate) << 44) | ((BITS as u64 - 1) << 36) | f.samples;
    h[18..26].copy_from_slice(&packed.to_be_bytes());
    Ok(h)
}

/// Opens `path` for reading and writing with no sharing, so a file another process has open
/// (a recording in progress there) is refused rather than changed under it.
fn open_exclusive(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    options.open(path)
}

/// Whether `e` says another process has the file open (Windows `ERROR_SHARING_VIOLATION`).
fn in_use(e: &std::io::Error) -> bool {
    cfg!(windows) && e.raw_os_error() == Some(32)
}

/// Recovers the interrupted recording `<path>.part` (T9.18): its whole frames (each decoded and
/// checked, see the module docs) become the finished FLAC at `path` with a real STREAMINFO
/// (MD5 zero), the torn tail is cut off, the file is synced, then renamed without replacing
/// anything. `sample_rate` is the rate the track was recorded at (the `.part`'s own header is
/// still the zeroed placeholder); a frame header whose 4-bit rate code names another rate is
/// refused (the encoder always writes such a code for the rates the pipeline records at; a
/// code that defers to STREAMINFO or the header's extra bytes is not checked). A `.part` with
/// no whole frame is left exactly as it was ([`PartRecovery::NoWholeFrame`]); one another
/// process holds open is not touched ([`PartRecovery::InUse`]).
///
/// Safe to run again after a crash part way: the scan starts after the header whatever it
/// holds, and the cut drops only bytes that are not a whole frame.
///
/// # Errors
/// [`StoreError::Io`] when `path` already exists (a finished file is never overwritten) or
/// `<path>.part` cannot be read or written; [`StoreError::Audio`] for frames recorded at
/// another sample rate or a stream a FLAC header cannot describe.
pub fn recover_part(path: &Path, sample_rate: u32) -> Result<PartRecovery, StoreError> {
    if path.exists() {
        return Err(StoreError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} already exists", path.display()),
        )));
    }
    let part = part_path(path);
    let mut file = match open_exclusive(&part) {
        Ok(f) => f,
        Err(e) if in_use(&e) => return Ok(PartRecovery::InUse),
        Err(e) => return Err(e.into()),
    };
    let Some(found) = scan_frames(&file) else {
        return Ok(PartRecovery::NoWholeFrame);
    };
    // The first frame header's sample-rate code, when the 4-bit table has one, must agree.
    let mut first = [0u8; 4];
    file.seek(SeekFrom::Start(HEADER_LEN as u64))?;
    std::io::Read::read_exact(&mut file, &mut first)?;
    let code = first[2] & 0x0F;
    if (1..=11).contains(&code) && rate_code(sample_rate) != Some(code) {
        return Err(StoreError::Audio(format!(
            "{} was recorded at another sample rate than {sample_rate} Hz",
            part.display()
        )));
    }
    let header = stream_header(sample_rate, &found)?;
    let len = file.metadata()?.len();
    if len > found.end {
        file.set_len(found.end)?;
    }
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;
    file.sync_all()?;
    drop(file);
    crate::fsutil::rename_noreplace(&part, path)?;
    Ok(PartRecovery::Recovered {
        file: FinishedFlac {
            path: path.to_owned(),
            sample_rate,
            frames: found.samples,
            size_bytes: found.end,
            cut_short: true,
        },
        torn_bytes: len.saturating_sub(found.end),
    })
}

/// Describes a finished FLAC file from its STREAMINFO without decoding it (T9.18: a finished
/// file whose `AudioFile` row was never written). [`FinishedFlac::cut_short`] is false: the
/// header cannot say.
///
/// # Errors
/// I/O or decoding errors, a file that is not 16-bit mono, or one whose header does not give
/// its sample count.
pub fn describe_flac(path: &Path) -> Result<FinishedFlac, StoreError> {
    let r = claxon::FlacReader::open(path).map_err(|e| audio_err("open flac", e))?;
    let info = r.streaminfo();
    if info.channels != 1 || info.bits_per_sample != BITS as u32 {
        return Err(StoreError::Audio(format!(
            "{}: expected 16-bit mono, got {} channels of {} bits",
            path.display(),
            info.channels,
            info.bits_per_sample
        )));
    }
    let frames = info
        .samples
        .ok_or_else(|| audio_err("sample count", "not in the header"))?;
    drop(r);
    Ok(FinishedFlac {
        path: path.to_owned(),
        sample_rate: info.sample_rate,
        frames,
        size_bytes: std::fs::metadata(path)?.len(),
        cut_short: false,
    })
}
