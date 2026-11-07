//! What Windows lets us hear of a Teams call (T6.15a, private notes).
//!
//! `teams_probe call [--seconds N]`, run during a Teams test call, measures at the same time:
//! process loopback of every `ms-teams.exe` and of any WebView2 process with a render session,
//! loopback of every output, and the meter of every render session.
//!
//! `teams_probe category` plays a quiet tone through this process's own render stream, declared
//! as `Other` and as `Communications` (A B B A), and measures it under process loopback of this
//! process, with the default output's loopback and the session meter as controls. It repeats a
//! report made on another machine (Windows-classic-samples#414) on this machine's driver.
//!
//! `teams_probe sessions [--seconds N]` (T9.4) prints a timeline of every render session: when each
//! becomes active or stops, when its meter starts or stops showing sound, and when a known call app
//! has an active session. Start it during a call and hang up, or go on hold, while it runs. It opens
//! no capture and plays nothing.
//!
//! It prints levels and process names only. Samples are read for their level and dropped: no
//! audio is kept, nothing is written and nothing reaches the network.

#[cfg(not(windows))]
fn main() {
    eprintln!("teams_probe runs on Windows only");
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("call") => match seconds_from(&args[1..]) {
            Ok(seconds) => probe::call(seconds),
            Err(e) => Err(e),
        },
        Some("category") => probe::category(),
        Some("sessions") => match seconds_from(&args[1..]) {
            Ok(seconds) => probe::sessions(seconds),
            Err(e) => Err(e),
        },
        _ => Err(
            "usage: teams_probe call [--seconds N] | teams_probe category | teams_probe sessions [--seconds N]"
                .to_owned(),
        ),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `--seconds N` (default 45: a Teams test call plays, records you and plays it back).
#[cfg_attr(not(windows), allow(dead_code))]
fn seconds_from(args: &[String]) -> Result<u64, String> {
    match args {
        [] => Ok(45),
        [flag, n] if flag == "--seconds" => match n.parse::<u64>() {
            Ok(s) if (1..=600).contains(&s) => Ok(s),
            _ => Err(format!("--seconds takes 1 to 600, not {n}")),
        },
        _ => Err("this mode takes only --seconds N".to_owned()),
    }
}

/// At or above this peak (-60 dBFS) a track has sound in it.
#[cfg_attr(not(windows), allow(dead_code))]
const AUDIBLE: f32 = 1e-3;

/// What a capture heard.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Level {
    packets: u64,
    silent_packets: u64,
    samples: u64,
    nonzero: u64,
    peak: f32,
    sum_squares: f64,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Level {
    fn add(&mut self, silent: bool, samples: &[f32]) {
        self.packets += 1;
        self.silent_packets += u64::from(silent);
        self.samples += samples.len() as u64;
        for &s in samples {
            if s != 0.0 {
                self.nonzero += 1;
            }
            self.peak = self.peak.max(s.abs());
            self.sum_squares += f64::from(s) * f64::from(s);
        }
    }

    /// The part of the samples that are not exact zeros.
    fn sound_share(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.nonzero as f64 / self.samples as f64
        }
    }

    fn rms(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            (self.sum_squares / self.samples as f64).sqrt()
        }
    }

    fn line(&self) -> String {
        format!(
            "peak {:.5}  rms {:.6}  nonzero {} of {}  packets {} ({} flagged silent)",
            self.peak,
            self.rms(),
            self.nonzero,
            self.samples,
            self.packets,
            self.silent_packets
        )
    }
}

/// The reading of a `call` run: the loudest output and the Teams process with the most sound,
/// `meter` the loudest Teams session meter.
///
/// A process counts as carrying the call only if it has sound for at least half as much of the
/// time as the output does. This rule was added after the first real run (2026-11-06), where a
/// WebView2 process played 2% of the time beside an output at 71% while the process with the
/// loud call sessions delivered exact zeros.
#[cfg_attr(not(windows), allow(dead_code))]
fn call_verdict(output: &Level, process: &Level, meter: f32) -> &'static str {
    if output.peak < AUDIBLE {
        "NO SOUND: no output played anything. Run this again while the call is talking."
    } else if process.peak >= AUDIBLE && process.sound_share() >= output.sound_share() / 2.0 {
        "PROCESS HEARS IT: a Teams process delivers about as much sound as the output. Per-app \
         capture can work on this machine; the process above with the most sound is the one."
    } else if process.peak >= AUDIBLE {
        "HIDDEN, EXCEPT SHORT SOUNDS: a Teams process delivers a little sound, far less than the \
         output carries, so the call itself is not reaching process loopback."
    } else if meter >= AUDIBLE {
        "HIDDEN: Teams' own session is playing and the output hears it, but every Teams process \
         delivers silence. No process to find: Windows is hiding it."
    } else {
        "NOT TEAMS: an output played sound, but no Teams session shows any. Was the sound from \
         the call?"
    }
}

/// The reading of a `category` run from the peaks under process loopback of each `Other` and
/// `Communications` run, given the tone reached the output in every run.
#[cfg_attr(not(windows), allow(dead_code))]
fn category_verdict(other: &[f32], communications: &[f32], outputs: &[f32]) -> &'static str {
    let lowest = |v: &[f32]| v.iter().copied().fold(f32::INFINITY, f32::min);
    let highest = |v: &[f32]| v.iter().copied().fold(0.0, f32::max);
    if lowest(outputs) < AUDIBLE {
        "NO RESULT: the tone did not reach the output in every run."
    } else if lowest(other) < AUDIBLE {
        "NO RESULT: a normal stream was not heard either, so this says nothing about the category."
    } else if highest(communications) < AUDIBLE {
        "CONFIRMED: Windows hides Communications streams from process loopback on this machine."
    } else if lowest(communications) >= AUDIBLE {
        "NOT THE CAUSE: Communications streams are heard here, so something else hides Teams."
    } else {
        "MIXED: some Communications runs were heard and some were not."
    }
}

/// `tasklist /FO CSV /NH` output -> the pids of `image` (lower case, with `.exe`).
#[cfg_attr(not(windows), allow(dead_code))]
fn pids_in_tasklist(csv: &str, image: &str) -> Vec<u32> {
    let mut pids: Vec<u32> = csv
        .lines()
        .filter_map(|line| {
            let mut fields = line.split("\",\"");
            let name = fields.next()?.trim_matches('"').to_ascii_lowercase();
            let pid = fields.next()?.trim_matches('"').parse().ok()?;
            (name == image).then_some(pid)
        })
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(windows)]
mod probe {
    use std::collections::BTreeMap;
    use std::f64::consts::TAU;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use earshot_capture_wasapi::wasapi::{
        CaptureStream, ComApartment, Endpoint, RenderCategory, RenderStream, render_session_peaks,
    };
    use earshot_capture_wasapi::{call_app_active, list_audio_devices};

    use super::{Level, call_verdict, category_verdict, pids_in_tasklist};

    /// Time for every capture to start before the sound or the measuring window counts.
    const SETTLE: Duration = Duration::from_millis(500);
    const POLL: Duration = Duration::from_millis(10);
    const METER_POLL: Duration = Duration::from_millis(100);
    const TONE_SECONDS: f64 = 3.0;
    /// -30 dBFS.
    const TONE_LEVEL: f64 = 0.0316;

    #[derive(Clone)]
    enum Target {
        Process(u32),
        Output { id: Option<String> },
    }

    /// Captures `target` until `stop`, keeping only the level.
    fn capture(target: Target, stop: Arc<AtomicBool>) -> JoinHandle<Result<Level, String>> {
        thread::spawn(move || {
            let com = ComApartment::init_mta().map_err(|e| e.to_string())?;
            let mut stream = match &target {
                Target::Process(pid) => {
                    CaptureStream::open(&com, Endpoint::ProcessLoopback { pid: *pid }, None, None)
                }
                Target::Output { id } => {
                    CaptureStream::open_on(&com, Endpoint::Loopback, id.as_deref(), None, None)
                }
            }
            .map_err(|e| e.to_string())?;
            stream.start().map_err(|e| e.to_string())?;
            let mut level = Level::default();
            while !stop.load(Ordering::Acquire) {
                stream
                    .drain(|info, samples| level.add(info.silent, samples))
                    .map_err(|e| e.to_string())?;
                thread::sleep(POLL);
            }
            stream
                .drain(|info, samples| level.add(info.silent, samples))
                .map_err(|e| e.to_string())?;
            Ok(level)
        })
    }

    /// One session's readings over a run.
    #[derive(Debug, Default)]
    struct SessionRow {
        ever_active: bool,
        peak: f32,
        meter_unreadable: bool,
    }

    type Sessions = BTreeMap<(String, u32, String, usize), SessionRow>;

    /// Samples every session's meter until `stop`.
    fn meters(stop: Arc<AtomicBool>) -> JoinHandle<Result<Sessions, String>> {
        thread::spawn(move || {
            let com = ComApartment::init_mta().map_err(|e| e.to_string())?;
            let mut rows = Sessions::new();
            while !stop.load(Ordering::Acquire) {
                let mut seen: BTreeMap<(String, u32), usize> = BTreeMap::new();
                for s in render_session_peaks(&com).map_err(|e| e.to_string())? {
                    let n = seen.entry((s.endpoint.clone(), s.pid)).or_default();
                    let row = rows
                        .entry((s.endpoint, s.pid, s.process_name, *n))
                        .or_default();
                    *n += 1;
                    row.ever_active |= s.active;
                    match s.peak {
                        Some(p) => row.peak = row.peak.max(p),
                        None => row.meter_unreadable = true,
                    }
                }
                thread::sleep(METER_POLL);
            }
            Ok(rows)
        })
    }

    fn is_teams(name: &str) -> bool {
        name.eq_ignore_ascii_case("ms-teams.exe") || name.eq_ignore_ascii_case("msedgewebview2.exe")
    }

    fn teams_pids_running() -> Result<Vec<u32>, String> {
        let root = std::env::var("SystemRoot").map_err(|_| "SystemRoot is not set".to_owned())?;
        let out = std::process::Command::new(format!(r"{root}\System32\tasklist.exe"))
            .args(["/FI", "IMAGENAME eq ms-teams.exe", "/FO", "CSV", "/NH"])
            .output()
            .map_err(|e| format!("tasklist: {e}"))?;
        if !out.status.success() {
            return Err(format!("tasklist exited with {}", out.status));
        }
        Ok(pids_in_tasklist(
            &String::from_utf8_lossy(&out.stdout),
            "ms-teams.exe",
        ))
    }

    /// The `sessions` mode: a line for every change, nothing in between.
    pub(super) fn sessions(seconds: u64) -> Result<(), String> {
        let com = ComApartment::init_mta().map_err(|e| e.to_string())?;
        let started = std::time::Instant::now();
        let mut was: BTreeMap<(String, u32, String, usize), (bool, bool)> = BTreeMap::new();
        let mut call_active = None;
        println!("watching render sessions for {seconds} s: hang up or go on hold now");
        while started.elapsed() < Duration::from_secs(seconds) {
            let at = started.elapsed().as_secs_f64();
            let peaks = render_session_peaks(&com).map_err(|e| e.to_string())?;
            let active = call_app_active(&peaks);
            if call_active != Some(active) {
                println!(
                    "{at:7.1} s  a known call app has an active session: {}",
                    if active { "YES" } else { "NO" }
                );
                call_active = Some(active);
            }
            let mut seen: BTreeMap<(String, u32), usize> = BTreeMap::new();
            let mut now = BTreeMap::new();
            for s in peaks {
                let n = seen.entry((s.endpoint.clone(), s.pid)).or_default();
                let key = (s.endpoint, s.pid, s.process_name, *n);
                *n += 1;
                let state = (s.active, s.peak.is_some_and(|p| p >= super::AUDIBLE));
                if was.get(&key) != Some(&state) {
                    println!(
                        "{at:7.1} s  {} pid {} on {}: {}, {}",
                        key.2,
                        key.1,
                        key.0,
                        if state.0 { "active" } else { "not active" },
                        if state.1 { "sound" } else { "no sound" }
                    );
                }
                now.insert(key, state);
            }
            for key in was.keys().filter(|k| !now.contains_key(*k)) {
                println!(
                    "{at:7.1} s  {} pid {} on {}: session gone",
                    key.2, key.1, key.0
                );
            }
            was = now;
            thread::sleep(METER_POLL);
        }
        Ok(())
    }

    /// The `call` mode.
    pub(super) fn call(seconds: u64) -> Result<(), String> {
        let mut pids = teams_pids_running()?;
        let com = ComApartment::init_mta().map_err(|e| e.to_string())?;
        for s in render_session_peaks(&com).map_err(|e| e.to_string())? {
            if is_teams(&s.process_name) && !pids.contains(&s.pid) {
                pids.push(s.pid);
            }
        }
        drop(com);
        pids.sort_unstable();
        if pids.is_empty() {
            return Err("no ms-teams.exe is running: start the test call first".to_owned());
        }
        let outputs = list_audio_devices().map_err(|e| e.to_string())?.outputs;

        println!("measuring for {seconds} s: keep the call talking");
        let stop = Arc::new(AtomicBool::new(false));
        let process_runs: Vec<_> = pids
            .iter()
            .map(|&pid| (pid, capture(Target::Process(pid), stop.clone())))
            .collect();
        let output_runs: Vec<_> = outputs
            .iter()
            .map(|o| {
                let target = Target::Output {
                    id: Some(o.id.clone()),
                };
                (o.name.clone(), o.is_default, capture(target, stop.clone()))
            })
            .collect();
        let meter_run = meters(stop.clone());
        thread::sleep(SETTLE + Duration::from_secs(seconds));
        stop.store(true, Ordering::Release);

        println!("\nTeams processes, process loopback (each with its children):");
        let mut loudest_process = Level::default();
        for (pid, run) in process_runs {
            match run.join().map_err(|_| "a capture thread panicked")? {
                Ok(l) => {
                    if l.nonzero > loudest_process.nonzero {
                        loudest_process = l;
                    }
                    println!("  pid {pid}: {}", l.line());
                }
                Err(e) => println!("  pid {pid}: could not capture: {e}"),
            }
        }
        println!("\noutputs, loopback:");
        let mut loudest_output = Level::default();
        for (name, is_default, run) in output_runs {
            let mark = if is_default { " (Windows default)" } else { "" };
            match run.join().map_err(|_| "a capture thread panicked")? {
                Ok(l) => {
                    if l.peak > loudest_output.peak {
                        loudest_output = l;
                    }
                    println!("  {name}{mark}: {}", l.line());
                }
                Err(e) => println!("  {name}{mark}: could not capture: {e}"),
            }
        }
        println!("\nrender sessions, meter:");
        let mut loudest_meter = 0.0_f32;
        let rows = meter_run
            .join()
            .map_err(|_| "the meter thread panicked")??;
        for ((endpoint, pid, name, _), row) in &rows {
            if is_teams(name) {
                loudest_meter = loudest_meter.max(row.peak);
            }
            println!(
                "  {name} pid {pid} on {endpoint}: peak {:.5}{}{}",
                row.peak,
                if row.ever_active { ", active" } else { "" },
                if row.meter_unreadable {
                    ", meter unreadable"
                } else {
                    ""
                },
            );
        }
        println!(
            "\nresult: {}",
            call_verdict(&loudest_output, &loudest_process, loudest_meter)
        );
        Ok(())
    }

    /// A tone with soft ends, so starting and stopping does not click.
    fn tone(rate: u32) -> Vec<f32> {
        let rate = f64::from(rate);
        let n = (TONE_SECONDS * rate) as usize;
        let fade = 0.05 * rate;
        (0..n)
            .map(|i| {
                let t = i as f64;
                let edge = (t / fade).min((n as f64 - t) / fade).min(1.0);
                (TONE_LEVEL * edge * (TAU * 440.0 * t / rate).sin()) as f32
            })
            .collect()
    }

    /// Plays the tone once; the peaks under process loopback and on the output.
    fn play(category: RenderCategory) -> Result<(f32, f32), String> {
        let me = std::process::id();
        let com = ComApartment::init_mta().map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let own = capture(Target::Process(me), stop.clone());
        let output = capture(Target::Output { id: None }, stop.clone());
        let meter = meters(stop.clone());
        thread::sleep(SETTLE);
        let mut render = RenderStream::open_as(&com, None, category).map_err(|e| e.to_string())?;
        let signal = tone(render.rate());
        let never = AtomicBool::new(false);
        let played = render.play(&signal, &never).map_err(|e| e.to_string());
        stop.store(true, Ordering::Release);
        let own = own.join().map_err(|_| "a capture thread panicked")??;
        let output = output.join().map_err(|_| "a capture thread panicked")??;
        let rows = meter.join().map_err(|_| "the meter thread panicked")??;
        played?;
        let session = rows
            .iter()
            .filter(|((_, pid, _, _), _)| *pid == me)
            .fold(0.0_f32, |p, (_, r)| p.max(r.peak));
        println!(
            "  {category:?}:\n    this process's loopback: {}\n    default output's loopback: {}\n    session meter peak: {session:.5}",
            own.line(),
            output.line()
        );
        Ok((own.peak, output.peak))
    }

    /// The `category` mode.
    pub(super) fn category() -> Result<(), String> {
        println!(
            "plays a quiet tone four times, {TONE_SECONDS} s each; while a Communications stream \
             plays, Windows may lower other sounds"
        );
        let (mut other, mut communications, mut outputs) = (Vec::new(), Vec::new(), Vec::new());
        for category in [
            RenderCategory::Other,
            RenderCategory::Communications,
            RenderCategory::Communications,
            RenderCategory::Other,
        ] {
            let (peak, output) = play(category)?;
            outputs.push(output);
            match category {
                RenderCategory::Other => other.push(peak),
                RenderCategory::Communications => communications.push(peak),
            }
        }
        println!(
            "\nresult: {}",
            category_verdict(&other, &communications, &outputs)
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teams_probe_seconds_default_and_limits() {
        assert_eq!(seconds_from(&[]), Ok(45));
        let args = |n: &str| ["--seconds".to_owned(), n.to_owned()];
        assert_eq!(seconds_from(&args("3")), Ok(3));
        assert!(seconds_from(&args("0")).is_err());
        assert!(seconds_from(&args("601")).is_err());
        assert!(seconds_from(&args("x")).is_err());
        assert!(seconds_from(&["--other".to_owned()]).is_err());
    }

    #[test]
    fn teams_probe_level_counts_zeros_and_keeps_the_peak() {
        let mut l = Level::default();
        l.add(true, &[]);
        l.add(false, &[0.0, 0.5, -0.75, 0.0]);
        assert_eq!(
            (l.packets, l.silent_packets, l.samples, l.nonzero),
            (2, 1, 4, 2)
        );
        assert_eq!(l.peak, 0.75);
        assert!((l.rms() - (0.8125_f64 / 4.0).sqrt()).abs() < 1e-9);
        assert_eq!(Level::default().rms(), 0.0);
    }

    fn heard(peak: f32, nonzero: u64, samples: u64) -> Level {
        Level {
            peak,
            nonzero,
            samples,
            ..Level::default()
        }
    }

    #[test]
    fn teams_probe_call_verdict_needs_the_output_to_have_sound() {
        let silent = Level::default();
        let loud = heard(0.5, 700, 1000);
        assert!(call_verdict(&silent, &silent, 0.5).starts_with("NO SOUND"));
        assert!(call_verdict(&heard(0.0009, 700, 1000), &loud, 0.5).starts_with("NO SOUND"));
        assert!(call_verdict(&loud, &heard(0.2, 350, 1000), 0.5).starts_with("PROCESS HEARS IT"));
        assert!(call_verdict(&loud, &silent, 0.5).starts_with("HIDDEN:"));
        assert!(call_verdict(&loud, &heard(0.0009, 700, 1000), 0.5).starts_with("HIDDEN:"));
        assert!(call_verdict(&loud, &silent, 0.0).starts_with("NOT TEAMS"));
    }

    #[test]
    fn teams_probe_call_verdict_reads_the_first_real_run_as_hidden() {
        // The counts of the real test call on 2026-11-06.
        let output = heard(0.60986, 2_409_809, 3_385_920);
        let webview = heard(0.21942, 95_420, 4_363_200);
        assert!(call_verdict(&output, &webview, 0.60986).starts_with("HIDDEN, EXCEPT SHORT"));
    }

    #[test]
    fn teams_probe_category_verdict_needs_its_controls() {
        let ok = [0.03; 4];
        assert!(category_verdict(&[0.03, 0.03], &[0.0, 0.00001], &ok).starts_with("CONFIRMED"));
        assert!(category_verdict(&[0.03, 0.03], &[0.03, 0.03], &ok).starts_with("NOT THE CAUSE"));
        assert!(category_verdict(&[0.03, 0.03], &[0.03, 0.0], &ok).starts_with("MIXED"));
        // A silent normal stream or a tone that never played says nothing.
        assert!(category_verdict(&[0.03, 0.0], &[0.0, 0.0], &ok).starts_with("NO RESULT"));
        assert!(
            category_verdict(&[0.03, 0.03], &[0.0, 0.0], &[0.03, 0.0, 0.03, 0.03])
                .starts_with("NO RESULT")
        );
    }

    #[test]
    fn teams_probe_reads_pids_from_tasklist_output() {
        let csv = "\"ms-teams.exe\",\"4052\",\"Console\",\"1\",\"300,000 K\"\r\n\
                   \"MS-TEAMS.EXE\",\"24184\",\"Console\",\"1\",\"9 K\"\r\n\
                   \"other.exe\",\"7\",\"Console\",\"1\",\"9 K\"\r\n\
                   INFO: No tasks are running which match the specified criteria.\r\n";
        assert_eq!(pids_in_tasklist(csv, "ms-teams.exe"), [4052, 24184]);
        assert!(pids_in_tasklist("INFO: nothing\r\n", "ms-teams.exe").is_empty());
    }
}
