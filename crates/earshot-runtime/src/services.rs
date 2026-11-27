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

/// The store lock held for a guarded root's reads ([`Services::guarded_read`]). It gives no
/// decoding reader: only [`ServicesRead::read`].
pub struct ServicesRead<'s> {
    db: MutexGuard<'s, Db>,
}

impl ServicesRead<'_> {
    /// A guarded read of the store.
    #[must_use]
    pub fn read(&self) -> earshot_store::GuardedRead<'_> {
        self.db.store.guarded_read()
    }

    /// This device's id.
    #[must_use]
    pub fn device(&self) -> DeviceId {
        self.db.device
    }
}

/// The store lock and the HLC held for a guarded root's whole closure
/// ([`Services::guarded_write`]).
pub struct ServicesWrite<'s> {
    db: MutexGuard<'s, Db>,
    clock: SharedClock,
}

impl ServicesWrite<'_> {
    /// A guarded read of the store.
    #[must_use]
    pub fn read(&self) -> earshot_store::GuardedRead<'_> {
        self.db.store.guarded_read()
    }

    /// This device's id.
    #[must_use]
    pub fn device(&self) -> DeviceId {
        self.db.device
    }

    /// One guarded transaction with a stamp from the services clock, the device's HLC and a
    /// fresh OS-seeded RNG; lines it wrote go out before the cleared rows they replaced, as
    /// every edit's do (`crate::edit::write`).
    ///
    /// # Errors
    /// `f`'s error; store errors.
    pub fn transaction<T, E: From<earshot_store::StoreError>>(
        &mut self,
        f: impl FnOnce(
            &mut earshot_store::GuardedTx<'_, '_>,
            &mut crate::edit::DynStamp<'_, '_>,
        ) -> Result<T, E>,
    ) -> Result<T, E> {
        let clock = Arc::clone(&self.clock);
        let mut rng = os_seeded_rng();
        let (store, hlc) = self.db.parts();
        store.guarded_transaction(|g| {
            let mut stamp: crate::edit::DynStamp<'_, '_> = earshot_core::model::Stamp {
                clock: &*clock,
                hlc,
                rng: &mut *rng,
            };
            let out = f(g, &mut stamp)?;
            g.send_lines_first(stamp.hlc, stamp.clock)?;
            Ok(out)
        })
    }

    /// [`earshot_store::Store::remove_person`]: its own transaction, then a checkpoint, under
    /// this lock.
    ///
    /// # Errors
    /// As the store's.
    pub fn remove_person(
        &mut self,
        person: EntityId,
    ) -> Result<earshot_store::PersonRemoval, earshot_store::StoreError> {
        let clock = Arc::clone(&self.clock);
        let (store, hlc) = self.db.parts();
        store.remove_person(person, hlc, &*clock)
    }

    /// [`earshot_store::Store::forget_voiceprints_of`]: its own transaction, then a checkpoint,
    /// under this lock.
    ///
    /// # Errors
    /// As the store's: `Busy` after the commit means every deletion is committed.
    pub fn forget_voiceprints_of(
        &mut self,
        persons: &[EntityId],
    ) -> Result<earshot_core::voiceprint::Forgotten, earshot_store::StoreError> {
        let clock = Arc::clone(&self.clock);
        let (store, hlc) = self.db.parts();
        store.forget_voiceprints_of(persons, hlc, &*clock)
    }

    /// A pulled page applied ([`earshot_store::Store::apply_remote`], or with every repair, or
    /// as the release of the two holds), then a checkpoint when it erased anything, under this
    /// lock.
    ///
    /// # Errors
    /// As the store's.
    pub fn apply_remote(
        &mut self,
        page: &[earshot_store::Incoming],
        rules: &earshot_store::ApplyRules<'_>,
        how: ApplyHow<'_>,
    ) -> Result<earshot_store::Applied, earshot_store::StoreError> {
        let clock = Arc::clone(&self.clock);
        let (store, hlc) = self.db.parts();
        let applied = match how {
            ApplyHow::Plain => store.apply_remote(page, rules, hlc, &*clock)?,
            ApplyHow::EveryRepair => store.apply_remote_every_repair(page, rules, hlc, &*clock)?,
            ApplyHow::Releasing(blocked) => {
                store.apply_remote_releasing(page, rules, blocked, hlc, &*clock)?
            }
        };
        // Every erased item counts (docs/sync-spec.md 7A.4): names cleared, speaker names
        // rewritten, identity entries dropped and biometric rows, so a removal of a person with
        // no saved voice is checkpointed too.
        if applied.erased > 0 {
            // A Busy here means the erasure is committed; the next checkpoint erases it.
            let _ = store.flush_erasures();
        }
        Ok(applied)
    }

    /// [`earshot_store::Store::outbox_sent`] (its own guarded transaction) under this lock.
    ///
    /// # Errors
    /// As the store's.
    pub fn outbox_sent(
        &mut self,
        sent: &[(&str, EntityId, earshot_core::identity::HlcTimestamp)],
    ) -> Result<usize, earshot_store::StoreError> {
        self.db.store.outbox_sent(sent)
    }

    /// [`earshot_store::Store::flush_erasures`] under this lock.
    ///
    /// # Errors
    /// As the store's.
    pub fn flush_erasures(&self) -> Result<(), earshot_store::StoreError> {
        self.db.store.flush_erasures()
    }
}

/// How [`ServicesWrite::apply_remote`] applies a page.
pub enum ApplyHow<'a> {
    /// Each repair only for the kinds the page wrote.
    Plain,
    /// Every repair (the first page a client applies).
    EveryRepair,
    /// The release of the two holds, with its `blocked`.
    Releasing(&'a dyn Fn(EntityId, EntityId) -> bool),
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

/// The jobs that held each meeting at one moment.
type HeldJobs = BTreeMap<EntityId, BTreeSet<MeetingJob>>;

/// What the jobs holding a meeting make of it: recording before processing before waiting;
/// `None` when no job that counts holds it (the rows then say, [`rows_activity`]).
fn job_activity(jobs: Option<&BTreeSet<MeetingJob>>) -> Option<MeetingActivity> {
    let jobs = jobs?;
    if jobs.contains(&MeetingJob::Recording) {
        Some(MeetingActivity::Recording)
    } else if jobs.contains(&MeetingJob::Processing) {
        Some(MeetingActivity::Processing)
    } else if jobs.contains(&MeetingJob::Waiting) {
        Some(MeetingActivity::Waiting)
    } else {
        None
    }
}

/// What the rows of `store` say of the live meeting `m` when no job holds it.
fn rows_activity(
    store: &Store,
    m: &earshot_core::model::Meeting,
) -> Result<MeetingActivity, RuntimeError> {
    use earshot_core::model::{MeetingRepo, TranscriptPass};
    Ok(if m.duration_ms.is_none() {
        MeetingActivity::Interrupted
    } else if store.has_live_segment(m.id, TranscriptPass::Final)?
        // A pass backend this build cannot read still says the final pass ran (T12.56).
        || crate::pass_backend::pass_backend_record(store, m.id, TranscriptPass::Final, None)?
            .exists()
    {
        MeetingActivity::Ready
    } else {
        MeetingActivity::Unprocessed
    })
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
    /// The debounced flush user text edits ask for ([`Services::request_flush`]).
    flush: Arc<FlushDebounce>,
    /// Where audio, models and the database live.
    pub data: DataDir,
    /// Wall clock.
    pub clock: SharedClock,
}

/// How long after the last request the debounced flush runs (T14.47, docs/sync-spec.md 15.8; a
/// judgement).
pub const FLUSH_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(5);

/// The debounced flush ([`Services::request_flush`]): the latest request and whether its thread
/// runs.
#[derive(Debug)]
struct FlushDebounce {
    state: Mutex<(Option<std::time::Instant>, bool)>,
    delay: std::sync::atomic::AtomicU64,
    /// Flushes run, for tests.
    runs: std::sync::atomic::AtomicUsize,
}

impl Default for FlushDebounce {
    fn default() -> Self {
        Self {
            state: Mutex::new((None, false)),
            delay: std::sync::atomic::AtomicU64::new(
                u64::try_from(FLUSH_DEBOUNCE.as_millis()).unwrap_or(u64::MAX),
            ),
            runs: std::sync::atomic::AtomicUsize::new(0),
        }
    }
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
        // "Lines go out before what they replace" judges queued rows by the push's own guard
        // (docs/sync-spec.md 15.2).
        store.set_push_guard(crate::sync::push_guard());
        Ok(Self {
            db: Arc::new(Mutex::new(Db { store, hlc, device })),
            activity: Arc::default(),
            flush: Arc::default(),
            data,
            clock,
        })
    }

    /// Whether another thread holds the store lock now (tests of what may run between a
    /// read and a write).
    #[cfg(test)]
    pub(crate) fn store_is_held(&self) -> bool {
        matches!(self.db.try_lock(), Err(std::sync::TryLockError::WouldBlock))
    }

    /// Asks for a truncating checkpoint ([`Store::flush_erasures`]) after a user's text edit
    /// (T14.47, docs/sync-spec.md 15.8): one flush runs [`FLUSH_DEBOUNCE`] after the last
    /// request, on a thread of its own, never inside the edit and never once per click. Best
    /// effort: a `Busy` leaves it to the next checkpoint.
    pub fn request_flush(&self) {
        let debounce = Arc::clone(&self.flush);
        let delay = std::time::Duration::from_millis(
            debounce.delay.load(std::sync::atomic::Ordering::Relaxed),
        );
        {
            let mut s = debounce
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            s.0 = Some(std::time::Instant::now() + delay);
            if s.1 {
                return;
            }
            s.1 = true;
        }
        // Weak: a flush never keeps a closed store open.
        let db = Arc::downgrade(&self.db);
        let spawned = std::thread::Builder::new()
            .name("earshot-flush".into())
            .spawn(move || {
                loop {
                    let wait = {
                        let mut s = debounce
                            .state
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let due = s.0.unwrap_or_else(std::time::Instant::now);
                        let now = std::time::Instant::now();
                        if now >= due {
                            s.0 = None;
                            s.1 = false;
                            None
                        } else {
                            Some(due - now)
                        }
                    };
                    match wait {
                        Some(w) => std::thread::sleep(w),
                        None => break,
                    }
                }
                if let Some(db) = db.upgrade() {
                    let _ = db
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .flush_erasures();
                }
                debounce
                    .runs
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
        if spawned.is_err() {
            // No thread: nothing waits for it; the closing checkpoint flushes.
            self.flush
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .1 = false;
        }
    }

    /// Test only: the debounced flush waits `delay` ([`FLUSH_DEBOUNCE`] otherwise).
    #[cfg(test)]
    pub(crate) fn set_flush_delay(&self, delay: std::time::Duration) {
        self.flush.delay.store(
            u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Test only: how many debounced flushes ran.
    #[cfg(test)]
    pub(crate) fn flushes_run(&self) -> usize {
        self.flush.runs.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Locks the store (and the HLC).
    ///
    /// A thread that panicked while holding the lock does not make the store unusable:
    /// every multi-row write is one SQLite transaction (rolled back if it did not
    /// commit), so the lock is recovered rather than reported.
    pub fn store(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The store lock for a guarded root's reads (T14.57, docs/sync-spec.md 16.3): offers only
    /// an `earshot_store::GuardedRead`.
    pub fn guarded_read(&self) -> ServicesRead<'_> {
        ServicesRead { db: self.store() }
    }

    /// The store lock and the HLC for a guarded root's whole closure (T14.57, docs/sync-spec.md
    /// 16.3): guarded reads, guarded transactions (several may run in turn under the one lock)
    /// and the store's guarded erasures, which commit and then checkpoint under the lock.
    pub fn guarded_write<T>(&self, f: impl FnOnce(&mut ServicesWrite<'_>) -> T) -> T {
        let mut w = ServicesWrite {
            db: self.store(),
            clock: Arc::clone(&self.clock),
        };
        f(&mut w)
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
        use earshot_core::model::MeetingRepo;
        let jobs = self.held_jobs();
        let db = self.store();
        let Some(m) = db.meeting(meeting)?.filter(|m| !m.meta.deleted) else {
            return Ok(None);
        };
        Ok(Some(match job_activity(jobs.get(&meeting)) {
            Some(busy) => busy,
            None => rows_activity(&db, &m)?,
        }))
    }

    /// [`Services::meeting_activity`] for a deletion (T14.26): a meeting must always be
    /// deletable, so a meeting row that does not read is still answered. Its jobs (held in
    /// memory) decide as ever; with none, it reads as [`MeetingActivity::Ready`], which a
    /// deletion takes as free to go and which only deletions ask. A damaged row whose `deleted`
    /// column says deleted is `None`.
    ///
    /// # Errors
    /// Store errors.
    pub fn deletion_activity(
        &self,
        meeting: EntityId,
    ) -> Result<Option<MeetingActivity>, RuntimeError> {
        let jobs = self.held_jobs();
        let db = self.store();
        let m = match db.meeting_or_damaged(meeting)? {
            None => return Ok(None),
            Some(Ok(m)) if m.meta.deleted => return Ok(None),
            Some(Ok(m)) => Some(m),
            Some(Err(())) if db.meeting_reads_deleted(meeting)? => return Ok(None),
            Some(Err(())) => None,
        };
        Ok(Some(match (job_activity(jobs.get(&meeting)), m) {
            (Some(busy), _) => busy,
            (None, Some(m)) => rows_activity(&db, &m)?,
            (None, None) => MeetingActivity::Ready,
        }))
    }

    /// Every live meeting with what is happening to it ([`Services::meeting_activity`]'s rule),
    /// read under one store lock (T14.13): what a meeting list reads, so a list of many meetings
    /// waits for the store once and sees one state of it. `each` is called for each meeting, in
    /// the store's order, with the store while it is locked: it reads what else the list shows of
    /// that meeting and must not take the store lock itself.
    ///
    /// The jobs are read before the store is waited for (the activity lock and the store lock
    /// are never held together) and once more after it, so a meeting whose job began while this
    /// waited for the store reads as that job, never as interrupted or unprocessed. A job that
    /// ended meanwhile is still reported: the event that follows its end brings the next read.
    ///
    /// # Errors
    /// Store errors; `each`'s error.
    pub fn live_meetings<T>(
        &self,
        each: impl FnMut(&Store, &earshot_core::model::Meeting) -> Result<T, RuntimeError>,
    ) -> Result<Vec<(earshot_core::model::Meeting, MeetingActivity, T)>, RuntimeError> {
        let before = self.held_jobs();
        self.live_meetings_since(&before, each)
    }

    /// [`Services::live_meetings`] with the jobs as they were read before the store is waited
    /// for.
    fn live_meetings_since<T>(
        &self,
        before: &HeldJobs,
        mut each: impl FnMut(&Store, &earshot_core::model::Meeting) -> Result<T, RuntimeError>,
    ) -> Result<Vec<(earshot_core::model::Meeting, MeetingActivity, T)>, RuntimeError> {
        use earshot_core::model::MeetingRepo;
        let mut read = Vec::new();
        {
            let db = self.store();
            for m in db.meetings()?.into_iter().filter(|m| !m.meta.deleted) {
                let busy = job_activity(before.get(&m.id));
                let activity = match busy {
                    Some(busy) => busy,
                    None => rows_activity(&db, &m)?,
                };
                let more = each(&db, &m)?;
                read.push((m, activity, more, busy.is_some()));
            }
        }
        let after = self.held_jobs();
        Ok(read
            .into_iter()
            .map(|(m, activity, more, was_busy)| {
                let now = (!was_busy)
                    .then(|| job_activity(after.get(&m.id)))
                    .flatten();
                (m, now.unwrap_or(activity), more)
            })
            .collect())
    }

    /// The jobs holding each meeting now.
    fn held_jobs(&self) -> HeldJobs {
        self.activity()
            .leased
            .iter()
            .map(|(meeting, jobs)| (*meeting, jobs.keys().copied().collect()))
            .collect()
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

    /// Meetings a live session records now ([`MeetingJob::Recording`] leases): sync leaves
    /// their rows queued until the recording is saved.
    pub(crate) fn recording_meetings(&self) -> Vec<EntityId> {
        self.activity()
            .leased
            .iter()
            .filter(|(_, jobs)| jobs.contains_key(&MeetingJob::Recording))
            .map(|(m, _)| *m)
            .collect()
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

    fn services_in_memory() -> Services {
        Services::new(
            Store::open_in_memory().unwrap(),
            DataDir::new(std::env::temp_dir().join("earshot-runtime-unused")),
            Arc::new(earshot_core::identity::ManualClock::new(1_800_000_000_000)),
        )
        .unwrap()
    }

    /// A meeting row; with a `duration` its recording finished. Distinct seeds per call.
    fn meeting_row(svc: &Services, seed: u64, duration: Option<u64>) -> EntityId {
        use earshot_core::model::{Meeting, MeetingRepo, Stamp};
        let clock = Arc::clone(&svc.clock);
        let mut rng = ChaCha20Rng::seed_from_u64(seed);
        let mut db = svc.store();
        let (store, hlc) = db.parts();
        let mut stamp = Stamp {
            clock: &*clock,
            hlc,
            rng: &mut rng,
        };
        let mut m = Meeting::new(
            "",
            1_800_000_000_000 + seed,
            600,
            earshot_core::prompt::Template::General,
            &mut stamp,
        );
        m.duration_ms = duration;
        let id = m.id;
        store.put_meeting(m).unwrap();
        id
    }

    fn listed(svc: &Services) -> BTreeMap<EntityId, MeetingActivity> {
        svc.live_meetings(|_, _| Ok(()))
            .unwrap()
            .into_iter()
            .map(|(m, activity, ())| (m.id, activity))
            .collect()
    }

    /// T14.13: the list read gives every live meeting the activity `meeting_activity` gives it.
    #[test]
    fn runtime_live_meetings_give_each_meeting_its_activity() {
        let svc = services_in_memory();
        let interrupted = meeting_row(&svc, 1, None);
        let unprocessed = meeting_row(&svc, 2, Some(60_000));
        let recording = meeting_row(&svc, 3, None);
        let processing = meeting_row(&svc, 4, Some(60_000));
        let waiting = meeting_row(&svc, 5, Some(60_000));
        let exporting = meeting_row(&svc, 6, Some(60_000));
        let _leases = [
            svc.lease_meeting_for(recording, MeetingJob::Recording)
                .unwrap(),
            svc.lease_meeting_for(processing, MeetingJob::Processing)
                .unwrap(),
            svc.lease_meeting_for(waiting, MeetingJob::Waiting).unwrap(),
            svc.lease_meeting_for(exporting, MeetingJob::Other).unwrap(),
        ];
        let all = listed(&svc);
        assert_eq!(all.len(), 6);
        for (id, expected) in [
            (interrupted, MeetingActivity::Interrupted),
            (unprocessed, MeetingActivity::Unprocessed),
            (recording, MeetingActivity::Recording),
            (processing, MeetingActivity::Processing),
            (waiting, MeetingActivity::Waiting),
            (exporting, MeetingActivity::Unprocessed),
        ] {
            assert_eq!(all[&id], expected);
            assert_eq!(svc.meeting_activity(id).unwrap(), Some(expected));
        }
    }

    /// T14.13: `each` runs with the store locked, once per live meeting, and the store is free
    /// again when the read returns.
    #[test]
    fn runtime_live_meetings_are_read_under_one_store_lock() {
        let svc = services_in_memory();
        let ids = [
            meeting_row(&svc, 1, None),
            meeting_row(&svc, 2, Some(1)),
            meeting_row(&svc, 3, Some(1)),
        ];
        let mut seen = Vec::new();
        let read = svc
            .live_meetings(|_, m| {
                // Held while each meeting is read, so `each` reads through the store it is
                // given (a lock of its own would wait for ever).
                assert!(matches!(
                    svc.db.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                seen.push(m.id);
                Ok(m.duration_ms)
            })
            .unwrap();
        assert_eq!(seen.len(), 3);
        assert!(ids.iter().all(|id| seen.contains(id)));
        assert_eq!(read.len(), 3);
        assert!(read.iter().all(|(m, _, more)| *more == m.duration_ms));
        assert!(svc.db.try_lock().is_ok(), "the store is free again");
        // A failing read of one meeting fails the list, with the store free again.
        let e = svc.live_meetings(|_, _| -> Result<(), RuntimeError> {
            Err(RuntimeError::Internal("a read that fails"))
        });
        assert!(matches!(e, Err(RuntimeError::Internal(_))));
        assert!(svc.db.try_lock().is_ok());
    }

    /// T14.13: a meeting whose job began while the list waited for the store reads as that job,
    /// never as interrupted; a job that ended meanwhile is still reported (the next read follows
    /// its end's event).
    #[test]
    fn runtime_live_meetings_see_a_job_that_began_while_the_store_was_waited_for() {
        let svc = services_in_memory();
        let started = meeting_row(&svc, 1, None);
        let finished = meeting_row(&svc, 2, Some(60_000));
        let untouched = meeting_row(&svc, 3, None);
        let ending = svc
            .lease_meeting_for(finished, MeetingJob::Processing)
            .unwrap();
        // The jobs as the list read them before it waited for the store.
        let before = svc.held_jobs();
        // Meanwhile one meeting starts recording and the other's work ends.
        let _recording = svc
            .lease_meeting_for(started, MeetingJob::Recording)
            .unwrap();
        drop(ending);
        let all: BTreeMap<EntityId, MeetingActivity> = svc
            .live_meetings_since(&before, |_, _| Ok(()))
            .unwrap()
            .into_iter()
            .map(|(m, activity, ())| (m.id, activity))
            .collect();
        assert_eq!(all[&started], MeetingActivity::Recording);
        assert_eq!(all[&finished], MeetingActivity::Processing);
        // The control: a meeting no job ever held reads from its rows.
        assert_eq!(all[&untouched], MeetingActivity::Interrupted);
        // A read from now on sees both as they are.
        let now = listed(&svc);
        assert_eq!(now[&started], MeetingActivity::Recording);
        assert_eq!(now[&finished], MeetingActivity::Unprocessed);
    }
}
