//! The privacy record of the agent-CLI summary providers (PLAN.md 3.9 exception, the owner
//! 2026-11-04): Claude Code ([`crate::claude_code`], T7.4ga) and OpenAI Codex
//! ([`crate::codex`], T7.4gc). Earshot hands the prompt text to the user's own CLI, which
//! connects to its vendor itself; Earshot cannot audit that request, so every run records a
//! [`Handover`] (when, how many characters, how it ended; never the text). The types are
//! generic over the provider's error (`E`), with Claude Code's as the default.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::claude_code::ClaudeCodeError;

/// One run's record (PLAN.md 3.9): when Earshot handed text to the CLI, how much, and how
/// the run ended. Never the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handover<E = ClaudeCodeError> {
    /// Clock reading when the CLI was started (Unix milliseconds).
    pub timestamp_ms: u64,
    /// Characters handed over on standard input.
    pub characters: usize,
    /// How the run ended.
    pub outcome: HandoverOutcome<E>,
}

/// How a run that was handed text ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoverOutcome<E = ClaudeCodeError> {
    /// The CLI answered (characters of the answer).
    Answered {
        /// Characters of the reply text.
        characters: usize,
    },
    /// The run failed.
    Failed(E),
    /// The run was cancelled and the CLI stopped.
    Cancelled,
}

/// Shared, append-only log of [`Handover`]s. Cloning gives another handle to the same log.
#[derive(Debug)]
pub struct HandoverLog<E = ClaudeCodeError>(Arc<Mutex<Vec<Handover<E>>>>);

impl<E> Clone for HandoverLog<E> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<E> Default for HandoverLog<E> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
}

impl<E: Clone> HandoverLog<E> {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record(&self, h: Handover<E>) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(h);
    }

    /// Every entry so far, oldest first.
    #[must_use]
    pub fn entries(&self) -> Vec<Handover<E>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Hears about every handover as it happens (the runtime stores them per meeting).
pub trait HandoverSink<E = ClaudeCodeError>: Send + Sync {
    /// The CLI runs and `characters` are about to be written to it. An error stops the run
    /// before any text is written (the provider's `NotRecorded`).
    ///
    /// # Errors
    /// Why the handover could not be recorded (shown to nobody but the caller's log).
    fn started(&self, timestamp_ms: u64, characters: usize) -> Result<(), String>;
    /// A run that [`HandoverSink::started`] accepted ended.
    fn ended(&self, handover: &Handover<E>);
}

/// An empty folder of its own under a root, deleted (with anything in it) on drop.
pub(crate) struct TempFolder(PathBuf);

impl TempFolder {
    /// A new folder `<root>/<prefix>-<pid>-<nanos>-<n>`.
    pub(crate) fn new(root: &Path, prefix: &str) -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!("{prefix}-{}-{nanos:08x}-{n}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

/// Tries to delete a [`TempFolder`] this many times, [`REMOVE_WAIT`] apart. A job's termination
/// returns before Windows has closed every handle of the killed processes, so a run that was just
/// killed can still hold its working folder for a moment. About 2 s in all, a judgement: a run
/// left holding it past that leaves an empty folder in the temp directory, and nothing fails.
const REMOVE_TRIES: u32 = 40;
/// See [`REMOVE_TRIES`].
const REMOVE_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

impl Drop for TempFolder {
    fn drop(&mut self) {
        for attempt in 0..REMOVE_TRIES {
            if attempt > 0 {
                std::thread::sleep(REMOVE_WAIT);
            }
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => {}
            }
        }
    }
}

#[cfg(all(test, windows))]
mod temp_folder_tests {
    use std::os::windows::fs::OpenOptionsExt;

    use super::*;

    #[test]
    fn temp_folder_goes_once_a_killed_run_lets_go_of_it() {
        let root = std::env::temp_dir().join(format!("earshot-temp-folder-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let folder = TempFolder::new(&root, "held").unwrap();
        let path = folder.path().to_owned();
        let held = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .share_mode(0)
            .open(path.join("held"))
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(REMOVE_WAIT * 4);
            drop(held);
        });
        drop(folder);
        release.join().unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
