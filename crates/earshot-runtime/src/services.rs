//! What every runtime job shares: the store (with this device's single HLC), the data
//! directory, the wall clock and a source of randomness (for row ids).

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hasher};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use earshot_core::identity::{Clock, DeviceId, EntityId, Hlc};
use earshot_store::{DataDir, Store};
use rand_chacha::ChaCha20Rng;
use rand_core::{Rng, SeedableRng};

use crate::RuntimeError;

/// The wall clock (Unix milliseconds). Core takes time only through [`Clock`]; this is
/// the production implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// A clock shared between the caller and runtime threads.
pub type SharedClock = Arc<dyn Clock + Send + Sync>;

/// A random generator owned by one runtime thread.
pub type BoxRng = Box<dyn Rng + Send>;

/// A ChaCha20 generator seeded from std's process-random hash keys, which std draws
/// from the operating system's random source. Used for UUIDv7 row ids and device ids
/// (uniqueness, not secrecy); it avoids a `getrandom` dependency.
#[must_use]
pub fn os_seeded_rng() -> BoxRng {
    let mut seed = [0u8; 32];
    for (i, chunk) in seed.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_usize(i);
        chunk.copy_from_slice(&h.finish().to_le_bytes());
    }
    Box::new(ChaCha20Rng::from_seed(seed))
}

/// The store together with this device's hybrid logical clock.
///
/// There is exactly one [`Hlc`] per device and process, and it lives here, under the
/// store lock: every runtime thread stamps rows with it while holding the lock, so no
/// two writers can issue the same timestamp (AGENTS.md: a runtime HLC comes from
/// `Store::hlc(device)`, taken once).
#[derive(Debug)]
pub struct Db {
    store: Store,
    hlc: Hlc,
    device: DeviceId,
}

impl Db {
    /// This device's id.
    #[must_use]
    pub fn device(&self) -> DeviceId {
        self.device
    }

    /// The store and the HLC, borrowed together (to stamp rows while writing them).
    pub fn parts(&mut self) -> (&mut Store, &mut Hlc) {
        (&mut self.store, &mut self.hlc)
    }
}

impl Deref for Db {
    type Target = Store;
    fn deref(&self) -> &Store {
        &self.store
    }
}

impl DerefMut for Db {
    fn deref_mut(&mut self) -> &mut Store {
        &mut self.store
    }
}

/// What a job holding a [`MeetingLease`] does with its meeting (for
/// [`Services::meeting_activity`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MeetingJob {
    /// A live session records it.
    Recording,
    /// Its final pass, or a host's post-meeting run (final pass then summary,
    /// [`crate::host::after_meeting`]), works on it.
    Processing,
    /// Recorded and waiting for its turn in the queue ([`crate::meeting_queue`]).
    Waiting,
    /// Anything else (an export): it does not change what [`Services::meeting_activity`]
    /// reports.
    Other,
}

/// What is happening to a meeting ([`Services::meeting_activity`]); the first that applies
/// wins, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeetingActivity {
    /// A live session in this process is recording it.
    Recording,
    /// Its final pass or a host's post-meeting run (final pass, then summary) is working
    /// on it in this process.
    Processing,
    /// Recorded; its final pass and summary wait for an older meeting's ([`crate::meeting_queue`]).
    Waiting,
    /// Nothing works on it and its recording never finished: the meeting has no duration,
    /// which a live session sets only at its end, so the process (or the machine) stopped
    /// during the meeting. The live transcript up to then is stored; the audio may be in
    /// unfinished files.
    Interrupted,
    /// Recorded to the end, but no final pass ran: only the live draft exists (the cleanup
    /// was skipped, cancelled or failed, or the process stopped before or during it).
    Unprocessed,
    /// The final pass ran: final transcript segments are stored, or (a meeting without
    /// speech) the final pass's backend was recorded.
    Ready,
}

/// Which meetings runtime jobs are working on, and which are being deleted.
#[derive(Debug, Default)]
struct Activity {
    /// Leases held per meeting and job.
    leased: BTreeMap<EntityId, BTreeMap<MeetingJob, usize>>,
    /// Meetings a deletion is running on.
    deleting: BTreeSet<EntityId>,
}

/// A job's claim on one meeting ([`Services::lease_meeting`]): while it is held, the meeting
/// cannot be deleted in this process. Released on drop.
#[derive(Debug)]
pub struct MeetingLease {
    activity: Arc<Mutex<Activity>>,
    meeting: EntityId,
    job: MeetingJob,
}

impl MeetingLease {
    /// The meeting.
    #[must_use]
    pub fn meeting(&self) -> EntityId {
        self.meeting
    }
}

impl Drop for MeetingLease {
    fn drop(&mut self) {
        let mut a = self.activity.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(jobs) = a.leased.get_mut(&self.meeting) {
            if let Some(n) = jobs.get_mut(&self.job) {
                *n -= 1;
                if *n == 0 {
                    jobs.remove(&self.job);
                }
            }
            if jobs.is_empty() {
                a.leased.remove(&self.meeting);
            }
        }
    }
}

/// A running deletion's claim ([`Services::begin_deletion`]); released on drop.
#[derive(Debug)]
pub(crate) struct DeletionClaim {
    activity: Arc<Mutex<Activity>>,
    meeting: EntityId,
}

impl Drop for DeletionClaim {
    fn drop(&mut self) {
        self.activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .deleting
            .remove(&self.meeting);
    }
}

/// Shared runtime resources.
///
/// The store is one SQLite connection (`Send`, not `Sync`): runtime threads lock it
/// for single reads or writes and never hold the lock across a blocking model call.
/// Callers must not hold [`Services::store`] while waiting for a runtime job
/// (`LiveSession::stop` / `wait`), which needs the lock to finish.
#[derive(Clone)]
pub struct Services {
    db: Arc<Mutex<Db>>,
    activity: Arc<Mutex<Activity>>,
    /// Where audio, models and the database live.
    pub data: DataDir,
    /// Wall clock.
    pub clock: SharedClock,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("data", &self.data)
            .finish_non_exhaustive()
    }
}

impl Services {
    /// Services over an open `store`: reads (or creates, on first use) this device's id
    /// and resumes its HLC from the stored rows.
    ///
    /// # Errors
    /// Store errors.
    pub fn new(mut store: Store, data: DataDir, clock: SharedClock) -> Result<Self, RuntimeError> {
        let device = store.device_id(&*clock, &mut *os_seeded_rng())?;
        let hlc = store.hlc(device)?;
        Ok(Self {
            db: Arc::new(Mutex::new(Db { store, hlc, device })),
            activity: Arc::default(),
            data,
            clock,
        })
    }

    /// Locks the store (and the HLC).
    ///
    /// A thread that panicked while holding the lock does not make the store unusable:
    /// every multi-row write is one SQLite transaction (rolled back if it did not
    /// commit), so the lock is recovered rather than reported.
    pub fn store(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Claims `meeting` for a job (a live session, a final pass, an export) until the lease
    /// is dropped: a deletion of the meeting in this process is refused meanwhile. Several
    /// jobs may hold leases on one meeting. Never blocks on the store.
    ///
    /// # Errors
    /// [`RuntimeError::MeetingBusy`] (retryable) while the meeting is being deleted.
    pub fn lease_meeting(&self, meeting: EntityId) -> Result<MeetingLease, RuntimeError> {
        self.lease_meeting_for(meeting, MeetingJob::Other)
    }

    /// [`Services::lease_meeting`] for a `job` that [`Services::meeting_activity`] reports.
    ///
    /// # Errors
    /// As [`Services::lease_meeting`].
    pub fn lease_meeting_for(
        &self,
        meeting: EntityId,
        job: MeetingJob,
    ) -> Result<MeetingLease, RuntimeError> {
        let mut a = self.activity();
        if a.deleting.contains(&meeting) {
            return Err(RuntimeError::MeetingBusy(meeting));
        }
        *a.leased.entry(meeting).or_default().entry(job).or_insert(0) += 1;
        Ok(MeetingLease {
            activity: Arc::clone(&self.activity),
            meeting,
            job,
        })
    }

    /// [`Services::lease_meeting_for`], refused also while a live session records `meeting`
    /// or a final pass works on it or waits to ([`MeetingJob::Recording`],
    /// [`MeetingJob::Processing`] or [`MeetingJob::Waiting`] leases): for a job that must not run beside them (a re-transcription).
    ///
    /// # Errors
    /// [`RuntimeError::MeetingBusy`] (retryable).
    pub fn lease_meeting_alone(
        &self,
        meeting: EntityId,
        job: MeetingJob,
    ) -> Result<MeetingLease, RuntimeError> {
        let mut a = self.activity();
        let busy = a.leased.get(&meeting).is_some_and(|jobs| {
            jobs.contains_key(&MeetingJob::Recording)
                || jobs.contains_key(&MeetingJob::Processing)
                || jobs.contains_key(&MeetingJob::Waiting)
        });
        if busy || a.deleting.contains(&meeting) {
            return Err(RuntimeError::MeetingBusy(meeting));
        }
        *a.leased.entry(meeting).or_default().entry(job).or_insert(0) += 1;
        Ok(MeetingLease {
            activity: Arc::clone(&self.activity),
            meeting,
            job,
        })
    }

    /// What is happening to `meeting` ([`MeetingActivity`]); `None` if it does not exist or
    /// is deleted. Safe after a crash: a job's claim lives only in this process, so a
    /// meeting whose recording or processing died with an earlier process reads as
    /// [`MeetingActivity::Interrupted`] or [`MeetingActivity::Unprocessed`], never as still
    /// running. Jobs of another process on the same data directory are not seen. Takes the
    /// activity lock, then the store lock, each briefly.
    ///
    /// # Errors
    /// Store errors.
    pub fn meeting_activity(
        &self,
        meeting: EntityId,
    ) -> Result<Option<MeetingActivity>, RuntimeError> {
        use earshot_core::model::{MeetingRepo, TranscriptPass, live_segments};
        let jobs: BTreeSet<MeetingJob> = self
            .activity()
            .leased
            .get(&meeting)
            .map(|jobs| jobs.keys().copied().collect())
            .unwrap_or_default();
        let db = self.store();
        let Some(m) = db.meeting(meeting)?.filter(|m| !m.meta.deleted) else {
            return Ok(None);
        };
        let activity = if jobs.contains(&MeetingJob::Recording) {
            MeetingActivity::Recording
        } else if jobs.contains(&MeetingJob::Processing) {
            MeetingActivity::Processing
        } else if jobs.contains(&MeetingJob::Waiting) {
            MeetingActivity::Waiting
        } else if m.duration_ms.is_none() {
            MeetingActivity::Interrupted
        } else if !live_segments(&**db, meeting, TranscriptPass::Final)?.is_empty()
            || crate::backends::pass_backend(&db, meeting, TranscriptPass::Final, None)?.is_some()
        {
            MeetingActivity::Ready
        } else {
            MeetingActivity::Unprocessed
        };
        Ok(Some(activity))
    }

    /// Claims `meeting` for a deletion: refused while any lease on it is held or another
    /// deletion of it runs; new leases are refused until the claim is dropped.
    pub(crate) fn begin_deletion(&self, meeting: EntityId) -> Result<DeletionClaim, RuntimeError> {
        let mut a = self.activity();
        if a.leased.contains_key(&meeting) || !a.deleting.insert(meeting) {
            return Err(RuntimeError::MeetingBusy(meeting));
        }
        Ok(DeletionClaim {
            activity: Arc::clone(&self.activity),
            meeting,
        })
    }

    /// Meetings a job holds a lease on now.
    pub(crate) fn leased_meetings(&self) -> BTreeSet<EntityId> {
        self.activity().leased.keys().copied().collect()
    }

    /// The activity lock: held only for map updates, never across store work or a wait.
    fn activity(&self) -> MutexGuard<'_, Activity> {
        self.activity.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_system_clock_is_after_2025_and_seeded_rngs_differ() {
        assert!(SystemClock.now_unix_ms() > 1_735_689_600_000);
        let (mut a, mut b) = (os_seeded_rng(), os_seeded_rng());
        let (x, y) = (a.next_u64(), b.next_u64());
        assert_ne!(x, y, "two generators must not share a seed");
        assert_ne!(x, a.next_u64());
    }

    #[test]
    fn runtime_store_lock_survives_a_panicking_holder() {
        let data = DataDir::new(std::env::temp_dir().join("earshot-runtime-unused"));
        let svc = Services::new(
            Store::open_in_memory().unwrap(),
            data,
            Arc::new(earshot_core::identity::ManualClock::new(1)),
        )
        .unwrap();
        let s2 = svc.clone();
        let _ = std::thread::spawn(move || {
            let _guard = s2.store();
            panic!("holder panics");
        })
        .join();
        let device = svc.store().device();
        assert_eq!(svc.store().device(), device);
    }
}
