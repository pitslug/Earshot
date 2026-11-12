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
