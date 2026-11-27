//! Damaged items in the app (T14.65, docs/sync-spec.md 17.1 to 17.7): one banner, Details and
//! Repair from sync.
//!
//! Every scan runs through [`Scans`], so two never overlap. A report that meets damaged rows
//! (a sync tick, the People page's actions and list, the meeting list, a search, a meeting page
//! with unreadable lines, a refusal of kind `damaged`) calls [`report`], which starts a scan
//! only when its count is above the last scan's figure for that scope: at once when none ran in
//! the last [`SCAN_DEBOUNCE`], else once when that time is up; a request during a scan runs one
//! more after it. The scan itself reports nothing, so it never starts another. Counts only:
//! nothing here names a damaged row.

use std::collections::BTreeMap;
#[cfg(not(test))]
use std::sync::OnceLock;
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use earshot_core::identity::EntityId;
use earshot_runtime::Services;
use earshot_runtime::damaged::{self, DamagedItems};
use earshot_runtime::sync::{RepairStart, TickDamage};
use earshot_store::DamageCounts;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri_specta::Event;

use crate::ipc::{IpcError, blocking};
use crate::sync_settings::SyncHost;

/// The shortest time between two scans a report starts (30 s, a judgement): reports that come
/// in a burst run one scan.
pub const SCAN_DEBOUNCE: Duration = Duration::from_secs(30);

/// The one refusal line (docs/sync-spec.md 17.5; `refusal` in
/// `fixtures/ui/damaged_copy_cases.json`), IPC kind [`DAMAGED_KIND`].
pub const REFUSAL_COPY: &str =
    "This can't be done because some items on this computer are damaged.";

/// The IPC kind of a refusal a damaged row causes.
pub const DAMAGED_KIND: &str = "damaged";

/// Repair pressed while one runs (`busy` in `fixtures/ui/damaged_copy_cases.json`).
pub const BUSY_COPY: &str = "Repair is already running.";

/// Repair pressed with sync off or never set up (fixed copy, `no_repair_sync_off`).
pub const SYNC_OFF_COPY: &str = "Turn sync on to repair them from your other computers. This computer can't repair them on its own yet.";

/// The refusal a damaged row causes, kind `damaged`, and a request for a scan (counts as 1).
#[must_use]
pub fn refusal() -> IpcError {
    report(Scope::Total, 1);
    IpcError::new(DAMAGED_KIND, REFUSAL_COPY)
}

// ---- the scans ----

/// Which figure of the last scan a report is compared with (docs/sync-spec.md 17.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every damaged row (a sync tick, a refusal).
    Total,
    /// People plus saved voice data (Remove, Forget voice, Forget my voice).
    PeopleAndVoice,
    /// People (the People page's list).
    People,
    /// Meetings and folders (the meeting list).
    MeetingsAndFolders,
    /// Damaged meeting, line and summary rows (a search, which counts all three).
    Search,
    /// Damaged parts of one meeting (a meeting page that could not be read).
    Meeting(EntityId),
}

/// The last scan's figures, kept in memory (never stored): the record's six counts, and per
/// table and per meeting what a search or a meeting page compares with.
#[derive(Debug, Clone, Default)]
pub struct Figures {
    counts: DamageCounts,
    /// Damaged rows of the tables a search reads: meetings, lines and summaries.
    search: u64,
    by_meeting: BTreeMap<EntityId, u64>,
    /// Meeting figures are known only after a scan of this run (17.2: the start-up scan
    /// refills them).
    scanned: bool,
}

impl Figures {
    fn of(&self, scope: Scope) -> u64 {
        let c = &self.counts;
        match scope {
            Scope::Total => c.total(),
            Scope::PeopleAndVoice => c.people.saturating_add(c.voice),
            Scope::People => c.people,
            Scope::MeetingsAndFolders => c.meetings.saturating_add(c.folders),
            Scope::Search => self.search,
            Scope::Meeting(m) => self.by_meeting.get(&m).copied().unwrap_or(0),
        }
    }
}

/// When the next scan starts (the gate's answer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Now.
    Now,
    /// At this time on the gate's clock.
    At(Duration),
    /// Not for this request: one runs, or one is already due.
    None,
}

/// The scan gate (pure, clock-free): no two scans overlap; a scan starts at once when none
/// started in the last [`SCAN_DEBOUNCE`], else once when that time is up; a request during a
/// scan runs one more after it.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    running: bool,
    again: bool,
    /// The scan asked for during the running one must run as soon as it ends, not after the
    /// debounce: a forced scan (Details, the end of a Repair, the start-up scan) waits only for
    /// the one running (docs/sync-spec.md 17.7).
    again_forced: bool,
    last_start: Option<Duration>,
    due: Option<Duration>,
}

impl Gate {
    /// A report asks for a scan at `now`.
    pub fn request(&mut self, now: Duration) -> Start {
        if self.running {
            self.again = true;
            return Start::None;
        }
        if self.due.is_some() {
            return Start::None;
        }
        match self.last_start {
            Some(last) if now.saturating_sub(last) < SCAN_DEBOUNCE => {
                let at = last + SCAN_DEBOUNCE;
                self.due = Some(at);
                Start::At(at)
            }
            _ => self.begin(now),
        }
    }

    /// A scan that must run now (Details, the end of a Repair, the start-up scan): it waits
    /// only for one running, never for the debounce.
    pub fn force(&mut self, now: Duration) -> Start {
        if self.running {
            self.again = true;
            self.again_forced = true;
            return Start::None;
        }
        self.begin(now)
    }

    /// The time a request waited for has come.
    pub fn due(&mut self, now: Duration) -> Start {
        self.due = None;
        if self.running {
            self.again = true;
            return Start::None;
        }
        self.begin(now)
    }

    /// A scan ended at `now`.
    pub fn finished(&mut self, now: Duration) -> Start {
        self.running = false;
        let forced = std::mem::take(&mut self.again_forced);
        if !std::mem::take(&mut self.again) {
            return Start::None;
        }
        if forced {
            self.due = None;
            self.begin(now)
        } else {
            self.request(now)
        }
    }

    /// The scan just begun could not be started (no thread): nothing runs and nothing is due.
    fn not_started(&mut self) {
        self.running = false;
        self.again = false;
        self.again_forced = false;
        self.due = None;
    }

    fn begin(&mut self, now: Duration) -> Start {
        self.running = true;
        self.last_start = Some(now);
        Start::Now
    }

    /// Whether a scan runs (tests).
    #[cfg(test)]
    #[must_use]
    pub fn running(&self) -> bool {
        self.running
    }
}

/// Runs a job off the caller's thread (a thread in the app; tests may hold the jobs); `false`
/// when it could not be started.
pub type Spawn = Arc<dyn Fn(Box<dyn FnOnce() + Send>) -> bool + Send + Sync>;

/// The clock the gate reads (monotonic).
pub type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;

/// Told after every scan and Repair step what the banner and Repair show.
pub type DamageSink = Arc<dyn Fn(DamageEvent) + Send + Sync>;

struct State {
    gate: Gate,
    figures: Figures,
    /// Scans finished so far (Details waits for one that started after it asked).
    finished: u64,
    /// Scans started so far.
    started: u64,
    /// What the last scan found since a Repair ended (`Done`): set by the closing scan and
    /// replaced by every later scan, so it never outlives the figures; cleared by a new Repair.
    repair_left: Option<DamageCounts>,
    /// A Repair ended once this many scans had started: only a scan started after it closes it.
    closing_after: Option<u64>,
    /// The last scan failed (a store error): Details says it could not read.
    failed: bool,
}

/// Every scan of the app's store (module docs).
pub struct Scans {
    services: Services,
    state: Mutex<State>,
    done: Condvar,
    clock: Clock,
    spawn: Spawn,
    sink: DamageSink,
    me: Weak<Scans>,
}

impl Scans {
    /// Scans over `services`, the gate on `clock`, jobs run by `spawn`, events to `sink`. The
    /// figures start from the stored record's counts.
    pub fn new(services: Services, clock: Clock, spawn: Spawn, sink: DamageSink) -> Arc<Self> {
        let counts = damaged::stored(&services)
            .map(|s| s.items.found)
            .unwrap_or_default();
        Arc::new_cyclic(|me| Self {
            services,
            state: Mutex::new(State {
                gate: Gate::default(),
                figures: Figures {
                    counts,
                    ..Figures::default()
                },
                finished: 0,
                started: 0,
                repair_left: None,
                closing_after: None,
                failed: false,
            }),
            done: Condvar::new(),
            clock,
            spawn,
            sink,
            me: me.clone(),
        })
    }

    /// Scans for the app: a thread per job, the process's monotonic clock.
    pub fn for_app(services: Services, sink: DamageSink) -> Arc<Self> {
        let origin = Instant::now();
        Self::new(
            services,
            Arc::new(move || origin.elapsed()),
            Arc::new(|job| {
                std::thread::Builder::new()
                    .name("earshot-damage-scan".into())
                    .spawn(job)
                    .is_ok()
            }),
            sink,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A report's count for `scope`: a scan is requested only when it is above the last
    /// scan's figure for that scope (docs/sync-spec.md 17.3).
    pub fn request(&self, scope: Scope, count: u64) {
        let start = {
            let mut s = self.lock();
            let figure = match scope {
                // Before this run's first scan a meeting's figure is unknown: the start-up
                // scan refills it, and a report meanwhile asks.
                Scope::Meeting(_) | Scope::Search if !s.figures.scanned => 0,
                _ => s.figures.of(scope),
            };
            if count <= figure {
                return;
            }
            let now = (self.clock)();
            s.gate.request(now)
        };
        self.act(start);
    }

    /// A scan now, unless one runs (then one more after it): the start-up scan.
    pub fn scan_soon(&self) {
        let start = {
            let mut s = self.lock();
            let now = (self.clock)();
            s.gate.force(now)
        };
        self.act(start);
    }

    /// What a sync tick said: its damaged rows are a report against the total; the end of a
    /// Repair starts the closing scan; a Repair still running tells the window where it is.
    ///
    /// It runs on the sync thread every tick, so it reads only the store (never Credential
    /// Manager or the sync host's locks).
    pub fn heard_tick(&self, tick: &TickDamage) {
        if tick.repair_ended {
            let start = {
                let mut s = self.lock();
                // A scan already running started before the end: the closing one starts after.
                s.closing_after = Some(s.started);
                let now = (self.clock)();
                s.gate.force(now)
            };
            self.act(start);
            return;
        }
        self.request(
            Scope::Total,
            u64::try_from(tick.damaged).unwrap_or(u64::MAX),
        );
        if earshot_runtime::sync::repair_step(&self.services)
            .ok()
            .flatten()
            .is_some()
        {
            let shown = damaged::stored(&self.services).is_ok_and(|s| s.items.shown());
            (self.sink)(DamageEvent {
                shown,
                repair: self.repair_view_without_host(),
            });
        }
    }

    /// A scan that started after this call, run now unless one runs (then right after it),
    /// waited for: Details opening (docs/sync-spec.md 17.3). Returns its figures.
    ///
    /// # Errors
    /// `internal` when the scan could not read the store.
    pub fn scan_and_wait(&self) -> Result<Figures, IpcError> {
        let (start, target) = {
            let mut s = self.lock();
            let now = (self.clock)();
            let start = s.gate.force(now);
            // The scan that serves this call is the next one to start: now, or right after the
            // one running (`again`).
            let target = s.started + 1;
            (start, target)
        };
        self.act(start);
        let mut s = self.lock();
        while s.finished < target {
            s = self.done.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
        if s.failed {
            return Err(IpcError::internal());
        }
        Ok(s.figures.clone())
    }

    /// A new Repair started: forget what the last one left.
    pub fn repair_started(&self) {
        self.lock().repair_left = None;
    }

    fn act(&self, start: Start) {
        match start {
            Start::None => {}
            Start::Now => {
                let number = {
                    let mut s = self.lock();
                    s.started += 1;
                    s.started
                };
                let spawned = self
                    .me
                    .upgrade()
                    .is_some_and(|me| (self.spawn)(Box::new(move || me.run(number))));
                if !spawned {
                    // No thread: the scan counts as finished and failed, so nothing waits for
                    // it and the next request can start one.
                    let mut s = self.lock();
                    s.gate.not_started();
                    s.failed = true;
                    s.finished += 1;
                    self.done.notify_all();
                }
            }
            Start::At(at) => {
                if let Some(me) = self.me.upgrade() {
                    let spawned = (self.spawn)(Box::new(move || {
                        let wait = at.saturating_sub((me.clock)());
                        if !wait.is_zero() {
                            std::thread::sleep(wait);
                        }
                        let start = {
                            let mut s = me.lock();
                            let now = (me.clock)();
                            s.gate.due(now)
                        };
                        me.act(start);
                    }));
                    if !spawned {
                        self.lock().gate.not_started();
                    }
                }
            }
        }
    }

    /// One scan: reads the store, stores the record, keeps the figures, tells the window.
    fn run(&self, number: u64) {
        let scanned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let scan = damaged::scan(&self.services).ok()?;
            let items = damaged::record_scan(&self.services, scan.counts()).ok()?;
            Some((scan, items))
        }))
        .ok()
        .flatten();
        let (start, event) = {
            let mut s = self.lock();
            let mut event = None;
            match &scanned {
                Some((scan, items)) => {
                    s.failed = false;
                    s.figures = Figures {
                        counts: scan.counts(),
                        search: scan
                            .in_table("meeting")
                            .saturating_add(scan.in_table("segment"))
                            .saturating_add(scan.in_table("summary")),
                        by_meeting: scan
                            .meetings()
                            .map(|(m, n)| (EntityId::from_bytes(m), n))
                            .collect(),
                        scanned: true,
                    };
                    let closes = s.closing_after.is_some_and(|after| number > after);
                    if closes {
                        s.closing_after = None;
                    }
                    if closes || s.repair_left.is_some() {
                        s.repair_left = Some(scan.counts());
                    }
                    event = Some(items.shown());
                }
                None => s.failed = true,
            }
            s.finished += 1;
            let now = (self.clock)();
            let start = s.gate.finished(now);
            self.done.notify_all();
            (start, event)
        };
        if let Some(shown) = event {
            (self.sink)(DamageEvent {
                shown,
                repair: self.repair_view_without_host(),
            });
        }
        self.act(start);
    }

    /// Where a Repair is, for the window.
    #[must_use]
    pub fn repair_view(&self, sync: &SyncHost) -> RepairView {
        let stopped = crate::sync_settings::view(&self.services, sync).is_ok_and(|v| v.stopped);
        self.repair_view_with(stopped)
    }

    fn repair_view_without_host(&self) -> RepairView {
        let stopped = earshot_runtime::sync::sync_status(&self.services.store()).is_ok_and(|s| {
            s.problem
                .is_some_and(earshot_runtime::sync::SyncProblem::stops)
        });
        self.repair_view_with(stopped)
    }

    fn repair_view_with(&self, stopped: bool) -> RepairView {
        match earshot_runtime::sync::repair_progress(&self.services) {
            Ok(Some(_)) if stopped => RepairView::Stopped,
            Ok(Some(p)) => RepairView::Reading {
                percent: earshot_runtime::sync::repair_percent(p.pull_seq, p.seen_seq),
            },
            _ => match self.lock().repair_left {
                Some(left) => RepairView::Done {
                    left: DamageCountsView::of(left),
                },
                None => RepairView::Idle,
            },
        }
    }

    /// The gate's state (tests).
    #[cfg(test)]
    pub fn gate_for_test(&self) -> Gate {
        self.lock().gate.clone()
    }

    /// Scans started so far (tests).
    #[cfg(test)]
    pub fn started_for_test(&self) -> u64 {
        self.lock().started
    }

    /// The figures now (tests).
    #[cfg(test)]
    pub fn figures_for_test(&self) -> Figures {
        self.lock().figures.clone()
    }
}

#[cfg(test)]
impl Figures {
    pub fn of_for_test(&self, scope: Scope) -> u64 {
        self.of(scope)
    }
}

// ---- the app's one gate, for reports from anywhere ----

#[cfg(not(test))]
static APP_SCANS: OnceLock<Mutex<Weak<Scans>>> = OnceLock::new();

#[cfg(not(test))]
fn app_scans() -> Option<Arc<Scans>> {
    APP_SCANS
        .get_or_init(|| Mutex::new(Weak::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .upgrade()
}

// Tests: each test thread has its own gate, so tests running side by side never reach one
// another's.
#[cfg(test)]
thread_local! {
    static TEST_SCANS: std::cell::RefCell<Weak<Scans>> = const { std::cell::RefCell::new(Weak::new()) };
}

#[cfg(test)]
fn app_scans() -> Option<Arc<Scans>> {
    TEST_SCANS.with(|s| s.borrow().upgrade())
}

#[cfg(test)]
thread_local! {
    static HEARD: std::cell::RefCell<Option<Vec<(Scope, usize)>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` and says whether it made a report with damaged rows on this thread (tests).
#[cfg(test)]
pub fn reports_for_test<T>(f: impl FnOnce() -> T) -> (T, bool) {
    HEARD.with(|h| *h.borrow_mut() = Some(Vec::new()));
    let out = f();
    let heard = HEARD.with(|h| h.borrow_mut().take()).unwrap_or_default();
    (out, !heard.is_empty())
}

/// Makes `scans` the gate every [`report`] reaches (the app, once at start; a test, for its
/// own thread).
pub fn set_app_scans(scans: &Arc<Scans>) {
    #[cfg(not(test))]
    {
        *APP_SCANS
            .get_or_init(|| Mutex::new(Weak::new()))
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(scans);
    }
    #[cfg(test)]
    TEST_SCANS.with(|s| *s.borrow_mut() = Arc::downgrade(scans));
}

/// A report that met `count` damaged rows in `scope` (module docs); nothing when no gate is
/// set (tests of other modules, the replay).
pub fn report(scope: Scope, count: usize) {
    if count == 0 {
        return;
    }
    #[cfg(test)]
    HEARD.with(|h| {
        if let Some(heard) = h.borrow_mut().as_mut() {
            heard.push((scope, count));
        }
    });
    if let Some(scans) = app_scans() {
        scans.request(scope, u64::try_from(count).unwrap_or(u64::MAX));
    }
}

// ---- what the window sees ----

/// Damaged rows by kind, for the window: counts only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DamageCountsView {
    /// Meetings that can't be opened.
    pub meetings: u32,
    /// Parts of meetings (lines, speakers, tracks, notes, summaries, summary requests).
    pub parts: u32,
    /// People.
    pub people: u32,
    /// Saved voice data (voiceprints, voice samples).
    pub voice: u32,
    /// Folders.
    pub folders: u32,
    /// Dictionary words.
    pub words: u32,
}

impl DamageCountsView {
    fn of(c: DamageCounts) -> Self {
        let n = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        Self {
            meetings: n(c.meetings),
            parts: n(c.parts),
            people: n(c.people),
            voice: n(c.voice),
            folders: n(c.folders),
            words: n(c.words),
        }
    }
}

/// The damaged kinds whose own sync switch is off, so Repair cannot fix them
/// (`kind_off_voice`, `kind_off_words`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct KindsOff {
    /// Saved voice data is damaged and voiceprint sync is off.
    pub voice: bool,
    /// Dictionary words are damaged and Dictionary sync is off.
    pub words: bool,
}

/// Where a Repair from sync is (docs/sync-spec.md 17.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RepairView {
    /// None runs and none ended in this run of the app.
    Idle,
    /// Reading everything from the sync server again; the percentage once known (at most 99).
    Reading {
        /// `pull_seq * 100 / seen_seq`, at most 99; `None` while `seen_seq` is 0.
        percent: Option<u8>,
    },
    /// Waiting because sync has stopped on a problem.
    Stopped,
    /// Ended: what the closing scan left.
    Done {
        /// What is still damaged.
        left: DamageCountsView,
    },
}

/// What Details shows (docs/sync-spec.md 17.7). Counts and flags only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DamagedView {
    /// What the scan Details ran found, by kind.
    pub counts: DamageCountsView,
    /// Damaged kinds whose own sync switch is off.
    pub kinds_off: KindsOff,
    /// Sync is set up and on, so Repair is offered.
    pub sync_repairs: bool,
    /// Where a Repair is.
    pub repair: RepairView,
}

/// What the banner and Repair show, after every scan and Repair step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type, Event)]
pub struct DamageEvent {
    /// The banner shows.
    pub shown: bool,
    /// Where a Repair is.
    pub repair: RepairView,
}

/// Details' view: a scan through [`Scans`] (waiting for one running), then the switches and
/// where a Repair is.
///
/// # Errors
/// `internal` when the store cannot be read.
pub fn details(scans: &Scans, sync: &SyncHost) -> Result<DamagedView, IpcError> {
    let figures = scans.scan_and_wait()?;
    let v = crate::sync_settings::view(&scans.services, sync)?;
    let c = figures.counts;
    Ok(DamagedView {
        counts: DamageCountsView::of(c),
        kinds_off: KindsOff {
            voice: c.voice > 0 && !v.voiceprints,
            words: c.words > 0 && !v.dictionary,
        },
        sync_repairs: v.set_up && v.on,
        repair: scans.repair_view(sync),
    })
}

/// Starts a Repair from sync and asks the sync thread for a tick.
///
/// # Errors
/// `invalid` with sync off or never set up; `busy` while a Repair runs; `internal`.
pub fn repair(scans: &Scans, sync: &SyncHost) -> Result<(), IpcError> {
    let services = &scans.services;
    let v = crate::sync_settings::view(services, sync)?;
    if !(v.set_up && v.on) {
        return Err(IpcError::invalid(SYNC_OFF_COPY));
    }
    match earshot_runtime::sync::start_repair(services).map_err(|_| IpcError::internal())? {
        RepairStart::AlreadyRunning => Err(IpcError::busy(BUSY_COPY)),
        RepairStart::SyncOff => Err(IpcError::invalid(SYNC_OFF_COPY)),
        RepairStart::Started => {
            scans.repair_started();
            sync.sync_now(services);
            Ok(())
        }
    }
}

/// Whether the banner shows (the record; no scan). An unreadable record counts as nothing
/// found and starts a scan.
///
/// # Errors
/// `internal` when the store cannot be read.
pub fn banner(scans: &Scans) -> Result<bool, IpcError> {
    let stored = damaged::stored(&scans.services).map_err(|_| IpcError::internal())?;
    if stored.unreadable {
        scans.scan_soon();
    }
    Ok(stored.items.shown())
}

/// Dismisses the banner until a scan finds more.
///
/// # Errors
/// `internal` when the store cannot be written.
pub fn dismiss(scans: &Scans) -> Result<DamagedItems, IpcError> {
    damaged::dismiss(&scans.services).map_err(|_| IpcError::internal())
}

fn scans_of(state: &tauri::State<'_, Arc<Scans>>) -> Arc<Scans> {
    Arc::clone(state)
}

/// Details: a scan, counts by kind, the switches and where a Repair is.
#[tauri::command]
#[specta::specta]
pub async fn damaged_items(
    scans: tauri::State<'_, Arc<Scans>>,
    sync: tauri::State<'_, Arc<SyncHost>>,
) -> Result<DamagedView, IpcError> {
    let scans = scans_of(&scans);
    let sync = Arc::clone(&sync);
    blocking(move || details(&scans, &sync)).await
}

/// Repair from sync: reads everything from the sync server again; removes nothing.
#[tauri::command]
#[specta::specta]
pub async fn repair_from_sync(
    scans: tauri::State<'_, Arc<Scans>>,
    sync: tauri::State<'_, Arc<SyncHost>>,
) -> Result<(), IpcError> {
    let scans = scans_of(&scans);
    let sync = Arc::clone(&sync);
    blocking(move || repair(&scans, &sync)).await
}

/// Whether the banner shows (the stored record; no scan).
#[tauri::command]
#[specta::specta]
pub async fn damage_banner(scans: tauri::State<'_, Arc<Scans>>) -> Result<bool, IpcError> {
    let scans = scans_of(&scans);
    blocking(move || banner(&scans)).await
}

/// Dismisses the banner until a scan finds more damaged items in any kind.
#[tauri::command]
#[specta::specta]
pub async fn dismiss_damage_banner(scans: tauri::State<'_, Arc<Scans>>) -> Result<(), IpcError> {
    let scans = scans_of(&scans);
    blocking(move || dismiss(&scans).map(|_| ())).await
}

#[cfg(test)]
#[path = "damage_tests.rs"]
mod tests;
