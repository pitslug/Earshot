//! llama.cpp's log (and its ggml's), captured in a bounded ring, and the backend evidence
//! read from it.
//!
//! llama.cpp's own ggml (in `ggml.dll`) logs through `llama_log_set`; whisper.cpp's static
//! ggml has a separate logger, so whisper lines never appear here.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Lines kept; older lines are dropped.
pub const MAX_LINES: usize = 4096;

struct Ring {
    /// Sequence number of `lines[0]`.
    first: u64,
    lines: VecDeque<String>,
}

static RING: Mutex<Ring> = Mutex::new(Ring {
    first: 0,
    lines: VecDeque::new(),
});

fn ring() -> std::sync::MutexGuard<'static, Ring> {
    RING.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn push(line: String) {
    let mut r = ring();
    r.lines.push_back(line);
    if r.lines.len() > MAX_LINES {
        r.lines.pop_front();
        r.first += 1;
    }
}

/// The sequence number the next line will get.
#[must_use]
pub fn mark() -> u64 {
    let r = ring();
    r.first + r.lines.len() as u64
}

/// Lines logged since `mark`, or `None` when some of them were already dropped.
#[must_use]
pub fn since(mark: u64) -> Option<Vec<String>> {
    let r = ring();
    let skip = mark.checked_sub(r.first)?;
    Some(
        r.lines
            .iter()
            .skip(usize::try_from(skip).unwrap_or(usize::MAX))
            .cloned()
            .collect(),
    )
}

/// Serialises [`load_window`] calls.
static LOADS: LoadLock = LoadLock::new();

/// How old the load holding the lock may be, in time awake, before a load waiting for it calls it
/// stuck inside llama.cpp, never to give the lock back (a stalled GPU, T8.4ab). An honest load may
/// take as long as the runtime lets one run before calling it stalled: 600 s, its GPU and CPU load
/// limits (`earshot_runtime::summary::GPU_LOAD_LIMIT` and `CPU_LOAD_LIMIT`, a judgement for 5 GB
/// read from a slow disk), plus a minute's margin for its thread to be left behind (T14.41). A
/// waiting load waits only for the rest of the holder's time, and gives up at once beside a holder
/// already past it, so after one stuck load every later one fails at once rather than each
/// waiting the whole limit. This crate cannot see the runtime's constants; the runtime pins the
/// relation (`model_settings` tests).
pub const LOAD_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(600 + 60);

/// What a load says when the load before it is stuck.
pub(crate) const STUCK_MESSAGE: &str = "an earlier model load is stuck; restart Earshot";

/// An earlier load never finished ([`load_window`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoadStuck;

/// A lock that knows when its holder took it, in time awake ([`earshot_proc::AwakeClock`]: the
/// runtime's load limits are time awake, so a holder going through a sleep is not older for it).
struct LoadLock {
    lock: Mutex<()>,
    /// When the current holder took the lock; `None` while it is free.
    taken_at: Mutex<Option<std::time::Duration>>,
}

/// The lock held; clears [`LoadLock::taken_at`] before the lock is given back.
struct Held<'a> {
    owner: &'a LoadLock,
    _guard: std::sync::MutexGuard<'a, ()>,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        *self.owner.taken_at() = None;
    }
}

impl LoadLock {
    const fn new() -> Self {
        Self {
            lock: Mutex::new(()),
            taken_at: Mutex::new(None),
        }
    }

    fn taken_at(&self) -> std::sync::MutexGuard<'_, Option<std::time::Duration>> {
        self.taken_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes the lock, waiting while its holder is younger than `limit` on `clock`; `None` once
    /// the holder is that old (at once when it already is). A poisoned lock is still taken.
    fn take(
        &self,
        limit: std::time::Duration,
        clock: &earshot_proc::AwakeClock,
    ) -> Option<Held<'_>> {
        loop {
            let guard = match self.lock.try_lock() {
                Ok(guard) => guard,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    // A holder that has not yet stamped its time has only just taken the lock.
                    let holder_age = self
                        .taken_at()
                        .map_or(std::time::Duration::ZERO, |at| clock.since(at));
                    if holder_age >= limit {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                }
            };
            *self.taken_at() = Some(clock.now());
            return Some(Held {
                owner: self,
                _guard: guard,
            });
        }
    }
}

/// Runs `load` with the log to itself among loads: no other `load_window` runs at the same
/// time, so the lines logged meanwhile are this load's alone (`LocalLlm::load` reads its
/// backend from them). Returns `load`'s result and those lines, or `None` when some were
/// already dropped from the ring; [`LoadStuck`] when an earlier load never finished. `on_turn` runs once
/// the lock is taken, before `load`: a caller that watches its load for a stall starts the watch
/// there, so time spent queued behind another load is not counted against it.
pub(crate) fn load_window<T>(
    on_turn: impl FnOnce(),
    load: impl FnOnce() -> T,
) -> Result<(T, Option<Vec<String>>), LoadStuck> {
    static CLOCK: std::sync::OnceLock<earshot_proc::AwakeClock> = std::sync::OnceLock::new();
    let clock = CLOCK.get_or_init(earshot_proc::AwakeClock::system);
    let _one_load = LOADS.take(LOAD_LOCK_WAIT, clock).ok_or(LoadStuck)?;
    on_turn();
    let m = mark();
    let r = load();
    Ok((r, since(m)))
}

/// Where llama.cpp put the model's weights, read from its load log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlamaBackend {
    /// All weights in host memory, no GPU device used.
    Cpu,
    /// All layers offloaded to CUDA devices.
    Cuda,
    /// All layers offloaded to Vulkan devices.
    Vulkan,
}

impl LlamaBackend {
    /// Lower-case name for reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            LlamaBackend::Cpu => "cpu",
            LlamaBackend::Cuda => "cuda",
            LlamaBackend::Vulkan => "vulkan",
        }
    }
}

/// Classifies the log lines of one model load. `Err` explains any mixed, partial or
/// unrecognisable evidence; it is never read as a CPU run.
///
/// Rules (llama.cpp `src/llama.cpp` `using device`, `src/llama-model.cpp` `load_tensors`):
/// a GPU backend needs every `using device <name>` and every non-host `model buffer` to be
/// of that backend (`CUDA<n>` or `Vulkan<n>`) and `offloaded n/n layers to GPU`; the CPU
/// needs only `CPU*` model buffers and no layer offloaded: with no device listed (a CPU
/// build, which logs no offload count), or with devices listed and `offloaded 0/n` (a GPU
/// build lists every GPU it finds, even for a CPU load).
///
/// # Errors
/// A message naming what did not match.
pub fn classify(lines: &[String]) -> Result<LlamaBackend, String> {
    let mut devices = Vec::new();
    let mut buffers = Vec::new();
    let mut offloaded = None;
    for l in lines {
        if let Some(rest) = l.split("using device ").nth(1) {
            devices.push(rest.split_whitespace().next().unwrap_or("").to_string());
        }
        if let Some(head) = l
            .split(" model buffer size")
            .next()
            .filter(|_| l.contains(" model buffer size"))
        {
            let name = head.rsplit(':').next().unwrap_or("").trim().to_string();
            buffers.push(name);
        }
        if let Some(rest) = l
            .split("offloaded ")
            .nth(1)
            .filter(|_| l.contains("layers to GPU"))
        {
            let frac = rest.split_whitespace().next().unwrap_or("");
            let mut parts = frac.split('/').map(str::parse::<u32>);
            if let (Some(Ok(n)), Some(Ok(m))) = (parts.next(), parts.next()) {
                offloaded = Some((n, m));
            }
        }
    }
    if buffers.is_empty() {
        return Err(format!(
            "no `model buffer size` line in the load log ({} lines)",
            lines.len()
        ));
    }
    let is_host = |b: &String| b.starts_with("CPU");
    let gpu_kind = |name: &str| {
        if name.starts_with("CUDA") {
            Some(LlamaBackend::Cuda)
        } else if name.starts_with("Vulkan") {
            Some(LlamaBackend::Vulkan)
        } else {
            None
        }
    };
    if devices.is_empty() {
        if buffers.iter().all(is_host) && offloaded.is_none_or(|(n, _)| n == 0) {
            return Ok(LlamaBackend::Cpu);
        }
        return Err(format!(
            "no device used but buffers {buffers:?}, offloaded {offloaded:?}"
        ));
    }
    // A GPU build lists every GPU it finds, even for a CPU load; it also always logs the
    // offload count, so a CPU verdict needs an explicit `offloaded 0/n`.
    if buffers.iter().all(is_host) {
        if matches!(offloaded, Some((0, _))) {
            return Ok(LlamaBackend::Cpu);
        }
        return Err(format!(
            "devices {devices:?} listed and only host buffers {buffers:?}, but offloaded {offloaded:?}"
        ));
    }
    let kinds: Vec<Option<LlamaBackend>> = devices.iter().map(|d| gpu_kind(d)).collect();
    let Some(Some(kind)) = kinds.first().copied() else {
        return Err(format!("unknown device {devices:?}"));
    };
    if kinds.iter().any(|k| *k != Some(kind)) {
        return Err(format!("mixed devices {devices:?}"));
    }
    if !buffers
        .iter()
        .filter(|b| !is_host(b))
        .all(|b| gpu_kind(b) == Some(kind))
        || !buffers.iter().any(|b| gpu_kind(b) == Some(kind))
    {
        return Err(format!("{} device but buffers {buffers:?}", kind.name()));
    }
    match offloaded {
        Some((n, m)) if n == m && n > 0 => Ok(kind),
        other => Err(format!(
            "{} device but offloaded {other:?} layers",
            kind.name()
        )),
    }
}

/// The description of the GPU a load used, from its `using device <name> (<description>) (`
/// line: the first such line. `None` when the load names none.
#[must_use]
pub fn gpu_device(lines: &[String]) -> Option<String> {
    lines.iter().find_map(|l| {
        let rest = l.split("using device ").nth(1)?;
        let (_, after) = rest.split_once(" (")?;
        let (description, _) = after.split_once(") (")?;
        Some(description.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_a_load_lock_held_forever_is_given_up_on() {
        let lock = LoadLock::new();
        let clock = earshot_proc::HandClock::default();
        let awake = clock.awake();
        let short = std::time::Duration::from_millis(50);
        {
            let held = lock.take(short, &awake).unwrap();
            clock.advance(short);
            assert!(lock.take(short, &awake).is_none());
            drop(held);
        }
        assert!(lock.take(short, &awake).is_some());
        // A thread that panicked with it does not hold it.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = lock.lock.lock().unwrap();
            panic!("poison");
        }));
        assert!(lock.take(short, &awake).is_some());
    }

    #[test]
    fn log_names_the_gpu_a_load_used() {
        let vulkan = lines(&[
            "llama_prepare_model_devices: using device Vulkan0 (NVIDIA GeForce RTX 5080) (0000:01:00.0) - 15209 MiB free",
        ]);
        assert_eq!(
            gpu_device(&vulkan).as_deref(),
            Some("NVIDIA GeForce RTX 5080")
        );
        let brackets = lines(&[
            "llama_prepare_model_devices: using device Vulkan0 (AMD Radeon(TM) Graphics) (0000:13:00.0) - 1 MiB free",
        ]);
        assert_eq!(
            gpu_device(&brackets).as_deref(),
            Some("AMD Radeon(TM) Graphics")
        );
        assert_eq!(
            gpu_device(&lines(&["load_tensors: CPU model buffer size"])),
            None
        );
        assert_eq!(gpu_device(&[]), None);
    }

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn log_classifies_a_cpu_load() {
        let l = lines(&[
            "load_tensors: loading model tensors, this can take a while... (mmap = true)",
            "load_tensors:   CPU_Mapped model buffer size =  1059.89 MiB",
            "load_tensors:   CPU_REPACK model buffer size =   720.00 MiB",
        ]);
        assert_eq!(classify(&l), Ok(LlamaBackend::Cpu));
    }

    #[test]
    fn log_classifies_a_cuda_load() {
        let l = lines(&[
            "llama_model_load_from_file_impl: using device CUDA0 (NVIDIA GeForce RTX 5080) (0000:01:00.0) - 14000 MiB free",
            "load_tensors: offloading 28 repeating layers to GPU",
            "load_tensors: offloaded 29/29 layers to GPU",
            "load_tensors:        CUDA0 model buffer size =   934.70 MiB",
            "load_tensors:   CPU_Mapped model buffer size =   125.19 MiB",
        ]);
        assert_eq!(classify(&l), Ok(LlamaBackend::Cuda));
        let v = lines(&[
            "llama_model_load_from_file_impl: using device Vulkan0 (NVIDIA GeForce RTX 5080) (unknown id) - 15000 MiB free",
            "load_tensors: offloaded 29/29 layers to GPU",
            "load_tensors:      Vulkan0 model buffer size =   934.70 MiB",
        ]);
        assert_eq!(classify(&v), Ok(LlamaBackend::Vulkan));
    }

    /// Recorded 2026-10-24: `gpu: false` on the CUDA build (`--features cuda`, RTX 5080).
    /// llama.cpp lists the GPU even though nothing is offloaded.
    const CPU_LOAD_ON_CUDA_BUILD: &[&str] = &[
        "llama_prepare_model_devices: using device CUDA0 (NVIDIA GeForce RTX 5080) (0000:01:00.0) - 14985 MiB free",
        "load_tensors: offloading 0 repeating layers to GPU",
        "load_tensors: offloaded 0/29 layers to GPU",
        "load_tensors:   CPU_Mapped model buffer size =  1059.89 MiB",
    ];

    #[test]
    fn log_classifies_a_cpu_load_on_a_gpu_build() {
        assert_eq!(
            classify(&lines(CPU_LOAD_ON_CUDA_BUILD)),
            Ok(LlamaBackend::Cpu)
        );
        let vulkan: Vec<String> = CPU_LOAD_ON_CUDA_BUILD
            .iter()
            .map(|l| {
                l.replace(
                    "CUDA0 (NVIDIA GeForce RTX 5080) (0000:01:00.0)",
                    "Vulkan1 (NVIDIA GeForce RTX 5080) (unknown id)",
                )
            })
            .collect();
        assert_eq!(classify(&vulkan), Ok(LlamaBackend::Cpu));
    }

    #[test]
    fn log_refuses_a_listed_gpu_without_a_zero_offload_count() {
        // A GPU build always logs the offload count; without it, or with layers offloaded
        // but only host buffers, the evidence is unclear.
        let mut no_count = lines(CPU_LOAD_ON_CUDA_BUILD);
        no_count.retain(|l| !l.contains("layers to GPU"));
        assert!(classify(&no_count).is_err(), "{:?}", classify(&no_count));
        let offloaded: Vec<String> = CPU_LOAD_ON_CUDA_BUILD
            .iter()
            .map(|l| l.replace("offloaded 0/29", "offloaded 29/29"))
            .collect();
        assert!(classify(&offloaded).is_err(), "{:?}", classify(&offloaded));
        // Zero layers offloaded but a GPU buffer: contradictory.
        let mut gpu_buffer = lines(CPU_LOAD_ON_CUDA_BUILD);
        gpu_buffer.push("load_tensors:        CUDA0 model buffer size =   10.00 MiB".into());
        assert!(
            classify(&gpu_buffer).is_err(),
            "{:?}",
            classify(&gpu_buffer)
        );
    }

    #[test]
    fn log_refuses_partial_or_contradictory_evidence() {
        let partial = lines(&[
            "x: using device CUDA0 (gpu) - 1 MiB free",
            "load_tensors: offloaded 10/29 layers to GPU",
            "load_tensors:        CUDA0 model buffer size =   10.00 MiB",
        ]);
        assert!(classify(&partial).is_err());
        let gpu_buffer_without_device =
            lines(&["load_tensors:        CUDA0 model buffer size =   10.00 MiB"]);
        assert!(classify(&gpu_buffer_without_device).is_err());
        let cpu_offload_count = lines(&[
            "load_tensors: offloaded 29/29 layers to GPU",
            "load_tensors:   CPU_Mapped model buffer size =  1.00 MiB",
        ]);
        assert!(classify(&cpu_offload_count).is_err());
        assert!(classify(&[]).is_err());
        let mixed = lines(&[
            "x: using device CUDA0 (gpu) - 1 MiB free",
            "x: using device Vulkan0 (gpu) - 1 MiB free",
            "load_tensors: offloaded 29/29 layers to GPU",
            "load_tensors:        CUDA0 model buffer size =   10.00 MiB",
        ]);
        assert!(classify(&mixed).is_err());
    }

    /// Held by every test in this binary that pushes to the process-wide ring.
    static RING_TESTS: Mutex<()> = Mutex::new(());

    fn ring_test() -> std::sync::MutexGuard<'static, ()> {
        RING_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn log_load_windows_never_overlap() {
        let _ring = ring_test();
        use std::sync::atomic::{AtomicBool, Ordering};
        static A_DONE: AtomicBool = AtomicBool::new(false);
        static B_TURN_AFTER_A: AtomicBool = AtomicBool::new(false);
        A_DONE.store(false, Ordering::SeqCst);
        let (inside, entered) = std::sync::mpsc::channel();
        let a = std::thread::spawn(move || {
            load_window(
                || {},
                || {
                    push("log_window_test a1".into());
                    inside.send(()).unwrap();
                    // Long enough for the second load to log here if it were let in.
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    push("log_window_test a2".into());
                    A_DONE.store(true, Ordering::SeqCst);
                },
            )
            .unwrap()
            .1
        });
        entered.recv().unwrap();
        let b = std::thread::spawn(|| {
            load_window(
                // The second load's turn comes only once the first has finished.
                || B_TURN_AFTER_A.store(A_DONE.load(Ordering::SeqCst), Ordering::SeqCst),
                || {
                    push("log_window_test b1".into());
                    push("log_window_test b2".into());
                },
            )
            .unwrap()
            .1
        });
        assert_eq!(
            a.join().unwrap().unwrap(),
            ["log_window_test a1", "log_window_test a2"]
        );
        assert_eq!(
            b.join().unwrap().unwrap(),
            ["log_window_test b1", "log_window_test b2"]
        );
        assert!(
            B_TURN_AFTER_A.load(Ordering::SeqCst),
            "the turn came while queued"
        );
    }

    #[test]
    fn log_a_second_load_waits_out_a_slow_load_and_gives_up_only_on_a_stuck_one() {
        // A load that waits gives up only after the one before it would have been called
        // stalled by the runtime (600 s), plus a margin.
        use std::time::Duration;
        assert_eq!(LOAD_LOCK_WAIT, Duration::from_secs(660));
        // On a lock of its own and a clock moved by hand: a slow honest holder is waited for...
        let clock = earshot_proc::HandClock::default();
        let lock: &'static LoadLock = Box::leak(Box::new(LoadLock::new()));
        let (taken, is_taken) = std::sync::mpsc::channel::<()>();
        let (go, release) = std::sync::mpsc::channel::<()>();
        let holder = {
            let awake = clock.awake();
            std::thread::spawn(move || {
                let _held = lock.take(LOAD_LOCK_WAIT, &awake).unwrap();
                taken.send(()).unwrap();
                release.recv().unwrap();
            })
        };
        is_taken.recv().unwrap();
        let waiter = {
            let awake = clock.awake();
            std::thread::spawn(move || lock.take(LOAD_LOCK_WAIT, &awake).is_some())
        };
        // The holder is nearly at its limit, and the waiter keeps waiting.
        clock.advance(LOAD_LOCK_WAIT - Duration::from_secs(1));
        clock.read_twice_more();
        assert!(
            !waiter.is_finished(),
            "a waiter beside an honest holder waits"
        );
        go.send(()).unwrap();
        holder.join().unwrap();
        assert!(waiter.join().unwrap(), "a slow load is waited for");
        assert_eq!(*lock.taken_at(), None, "given back");
        // ...and beside a holder already past its limit a load gives up at once, with no clock
        // moving while it asks.
        let held = lock.take(LOAD_LOCK_WAIT, &clock.awake()).unwrap();
        clock.advance(LOAD_LOCK_WAIT);
        let (asked, answer) = std::sync::mpsc::channel();
        let late = {
            let awake = clock.awake();
            std::thread::spawn(move || asked.send(lock.take(LOAD_LOCK_WAIT, &awake).is_none()))
        };
        assert_eq!(answer.recv_timeout(Duration::from_secs(60)), Ok(true));
        late.join().unwrap().unwrap();
        drop(held);
        // The production lock says so with the restart message.
        assert!(STUCK_MESSAGE.contains("restart Earshot"));
    }

    #[test]
    fn log_ring_windows_detect_dropped_lines() {
        let _ring = ring_test();
        let m = mark();
        push("log_ring_test a".into());
        assert_eq!(since(m).unwrap(), ["log_ring_test a"]);

        // One line more than the ring holds: the window from `m` lost its first line.
        let m = mark();
        for i in 0..=MAX_LINES {
            push(format!("log_ring_test {i}"));
        }
        assert_eq!(
            since(m),
            None,
            "a window with dropped lines must be refused"
        );
        let kept = since(m + 1).expect("the window after the dropped line is complete");
        assert_eq!(kept.len(), MAX_LINES);
        assert_eq!(kept[0], "log_ring_test 1");
        assert_eq!(kept[MAX_LINES - 1], format!("log_ring_test {MAX_LINES}"));
    }
}
