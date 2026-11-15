//! `accept m11`: sync between two devices through the relay (T11.4; PLAN.md 4.2
//! `sync.convergence` and `sync.no_audio`, docs/sync-spec.md section 13).
//!
//! 1. **Relay.** The relay's binary (`earshot-sync-server`, a debug build of the code the
//!    image runs) is started on a free loopback port with a fresh data folder and a generated
//!    setup secret. Docker is not used, even where it exists: the container itself is checked
//!    by hand at GATE-09 (a deviation from T11.4's "Docker if available", put to the owner).
//!    The check `server` says which ran.
//! 2. **Devices.** earshot-runtime's `sync_eval` example sets up two devices against it (one
//!    with the setup secret, one with a pairing code), gives each a meeting with a recording
//!    on disk, makes concurrent edits, ticks both until nothing moves, opens every row the
//!    relay holds with the vault key and prints counts (never content).
//! 3. **Relay contents.** `earshot-sync-server inspect` on the stopped relay's data: row and
//!    presence sizes against their limits.
//! 4. **Type-level check.** earshot-core's tests that no NeverSync type (audio files, export
//!    records) can be sync-serialised.
//!
//! Each verdict is a pure function of what the steps printed ([`convergence`], [`no_audio`]),
//! tested below on good output and on each way it can be bad.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::{Check, Ctx, Measurement, RunOutput};
use crate::ci::cargo_command;
use crate::proc::run_with_timeout;

/// Deadline for each build.
const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Deadline for the two devices' run.
const EVAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long the relay may take to listen.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// The executable named `name` in cargo's JSON build messages.
#[must_use]
pub fn executable(messages: &str, name: &str) -> Option<PathBuf> {
    messages
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == name)
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
}

fn build(root: &Path, args: &[&str], name: &str) -> Result<PathBuf> {
    let mut cmd = cargo_command();
    cmd.args(["build", "--message-format=json"])
        .args(args)
        .current_dir(root);
    let out = run_with_timeout(&mut cmd, BUILD_TIMEOUT)
        .with_context(|| format!("building {name} did not finish"))?;
    out.require_complete(&format!("building {name}"))?;
    if !out.success {
        bail!("building {name} failed: {}", out.describe());
    }
    executable(&out.stdout, name).with_context(|| format!("cargo named no {name} executable"))
}

/// A folder removed when dropped (the relay's data and log, on every way out).
struct TempFolder(PathBuf);

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The relay process, killed when dropped.
struct RelayProcess(Child);

impl Drop for RelayProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

/// A setup secret for this run only: not a secret worth keeping, only long enough for the relay.
fn run_secret() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("accept-m11-{}-{nanos}", std::process::id())
}

/// The verdict on `sync.convergence` from the devices' report.
#[must_use]
pub fn convergence(eval: &Value) -> (bool, String) {
    let pair = |k: &str, want: u64| {
        eval[k]
            .as_array()
            .is_some_and(|a| a.len() == 2 && a.iter().all(|v| v.as_u64() == Some(want)))
    };
    let rows = eval["rows_compared"].as_u64().unwrap_or(0);
    let checks = [
        ("same synced rows on both", eval["converged"] == true),
        ("rows compared", rows > 0),
        ("both meetings on each", pair("meetings_on_each", 2)),
        (
            "the later rename won on both",
            eval["later_rename_won"] == true,
        ),
        (
            "the later rename won when its editor synced first",
            eval["later_editor_first_won"] == true,
        ),
        ("both notes on each", pair("notes_on_shared", 3)),
        (
            "each meeting reached the other",
            eval["own_meeting_reached_other"] == true,
        ),
        ("nothing left to send", pair("outbox_left", 0)),
    ];
    let failed: Vec<&str> = checks
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| *n)
        .collect();
    let detail = if failed.is_empty() {
        format!("{rows} rows the same on both devices after concurrent edits")
    } else {
        format!("failed: {}", failed.join(", "))
    };
    (failed.is_empty(), detail)
}

/// The verdict on `sync.no_audio` from the devices' report, the relay's inspection and the
/// type-level tests.
#[must_use]
pub fn no_audio(eval: &Value, inspection: &Value, type_level: bool) -> (bool, String) {
    let zero = |v: &Value, k: &str| v[k].as_u64() == Some(0);
    let opened = eval["relay_rows_opened"].as_u64().unwrap_or(0);
    let stored = inspection["rows"].as_u64();
    let own_audio_only = eval["audio_rows_on_each"]
        .as_array()
        .is_some_and(|a| a.len() == 2 && a.iter().all(|v| v.as_u64() == Some(1)));
    let checks = [
        ("no NeverSync type can be sync-serialised", type_level),
        (
            "every stored row opened",
            opened > 0 && stored == Some(opened),
        ),
        ("no unreadable row", zero(eval, "relay_rows_unreadable")),
        (
            "no audio entity at the relay",
            zero(eval, "relay_audio_entities"),
        ),
        (
            "no audio signature in any plaintext",
            zero(eval, "relay_audio_signatures"),
        ),
        (
            "no RIFF, fLaC or OggS in any decoded string",
            zero(eval, "relay_literal_signatures"),
        ),
        (
            "the scan flags audio (control)",
            eval["scanner_detects_audio"] == true,
        ),
        (
            "nothing refused by the guard",
            zero(eval, "refused_by_guard"),
        ),
        ("each device keeps only its own recording", own_audio_only),
        (
            "no stored row over the limit",
            zero(inspection, "rows_over_limit"),
        ),
        (
            "no presence blob over the limit",
            zero(inspection, "presence_over_limit"),
        ),
        (
            "presence was stored (the size check saw blobs)",
            inspection["largest_presence_bytes"].as_u64().unwrap_or(0) > 0,
        ),
    ];
    let failed: Vec<&str> = checks
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| *n)
        .collect();
    let detail = if failed.is_empty() {
        format!(
            "{opened} stored rows opened, none audio; largest row {} of {} bytes",
            inspection["largest_row_bytes"], inspection["row_limit_bytes"]
        )
    } else {
        format!("failed: {}", failed.join(", "))
    };
    (failed.is_empty(), detail)
}

fn parse(out: &crate::proc::CmdOutput, name: &str) -> Result<Value> {
    out.require_complete(name)?;
    if !out.success {
        bail!("{name} failed: {}", out.describe());
    }
    serde_json::from_str(out.stdout.trim()).with_context(|| format!("{name} printed no JSON"))
}

fn docker_present() -> bool {
    let mut cmd = Command::new("docker");
    cmd.arg("--version");
    run_with_timeout(&mut cmd, Duration::from_secs(20)).is_some_and(|o| o.success)
}

pub(super) fn run(ctx: &Ctx) -> Result<RunOutput> {
    let mut checks = Vec::new();
    let relay_exe = build(
        ctx.root,
        &["-p", "earshot-sync-server", "--bin", "earshot-sync-server"],
        "earshot-sync-server",
    )?;
    let eval_exe = build(
        ctx.root,
        &["-p", "earshot-runtime", "--example", "sync_eval"],
        "sync_eval",
    )?;
    let docker = docker_present();
    checks.push(Check::new(
        "server",
        true,
        if docker {
            "the relay binary (Docker is on this host but not used: the image runs the same binary; the container is GATE-09's)"
        } else {
            "the relay binary (Docker is not on this host)"
        },
    ));

    let data = std::env::temp_dir().join(format!("earshot-accept-m11-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data)?;
    let _folder = TempFolder(data.clone());
    let secret = run_secret();
    let port = free_port()?;
    let log_path = data.join("relay.log");
    let log = std::fs::File::create(&log_path)?;
    let mut relay = RelayProcess(
        Command::new(&relay_exe)
            .env("EARSHOT_SYNC_SETUP_SECRET", &secret)
            .env("EARSHOT_SYNC_DATA", &data)
            .env("EARSHOT_SYNC_LISTEN", format!("127.0.0.1:{port}"))
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
            .context("starting the relay")?,
    );
    let started = Instant::now();
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    while std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err() {
        if relay.0.try_wait()?.is_some() {
            bail!("the relay exited before it listened");
        }
        if started.elapsed() > START_TIMEOUT {
            bail!("the relay did not listen within {START_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let mut cmd = Command::new(&eval_exe);
    cmd.args([
        "--relay",
        &format!("http://127.0.0.1:{port}"),
        "--secret",
        &secret,
    ]);
    let eval_out = run_with_timeout(&mut cmd, EVAL_TIMEOUT).context("sync_eval did not finish")?;
    drop(relay);
    let eval = parse(&eval_out, "sync_eval")?;

    let mut cmd = Command::new(&relay_exe);
    cmd.arg("inspect").env("EARSHOT_SYNC_DATA", &data);
    let inspection = parse(
        &run_with_timeout(&mut cmd, Duration::from_secs(60)).context("inspect did not finish")?,
        "earshot-sync-server inspect",
    )?;

    // The relay logs a method, a path, a status and a time: never the secret.
    let log_text = std::fs::read_to_string(&log_path).unwrap_or_default();
    checks.push(Check::new(
        "relay_log_holds_no_secret",
        !log_text.is_empty() && !log_text.contains(&secret),
        format!("{} log lines", log_text.lines().count()),
    ));

    let mut cmd = cargo_command();
    cmd.args([
        "test",
        "-p",
        "earshot-core",
        "--test",
        "model_never_sync",
        "--test",
        "identity_sync_class",
    ])
    .current_dir(ctx.root);
    // In a job: a timeout stops the test binaries and trybuild's own cargo too.
    let types = crate::proc::run_in_job_with_timeout(&mut cmd, BUILD_TIMEOUT)
        .context("the type-level tests did not finish")?;
    let type_level = types.complete() && types.success;
    checks.push(Check::new(
        "type_level",
        type_level,
        "earshot-core model_never_sync and identity_sync_class",
    ));

    let (converged, converged_detail) = convergence(&eval);
    let (clean, clean_detail) = no_audio(&eval, &inspection, type_level);
    checks.push(Check::new(
        "convergence_detail",
        converged,
        converged_detail,
    ));
    checks.push(Check::new("no_audio_detail", clean, clean_detail));
    Ok(RunOutput {
        measurements: vec![
            Measurement::passed("sync.convergence", converged),
            Measurement::passed("sync.no_audio", clean),
        ],
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good_eval() -> Value {
        json!({
            "rounds": 3, "converged": true, "rows_compared": 14,
            "meetings_on_each": [2, 2], "later_rename_won": true, "notes_on_shared": [3, 3],
            "own_meeting_reached_other": true, "later_editor_first_won": true,
            "audio_rows_on_each": [1, 1], "relay_literal_signatures": 0,
            "outbox_left": [0, 0], "refused_by_guard": 0, "relay_rows_opened": 14,
            "relay_rows_unreadable": 0, "relay_audio_entities": 0,
            "relay_audio_signatures": 0, "scanner_detects_audio": true
        })
    }

    fn good_inspection() -> Value {
        json!({"rows": 14, "largest_row_bytes": 1631, "rows_over_limit": 0,
               "row_limit_bytes": 262144, "devices": 2, "largest_presence_bytes": 700,
               "presence_over_limit": 0})
    }

    #[test]
    fn m11_good_output_passes_both() {
        assert!(convergence(&good_eval()).0);
        assert!(no_audio(&good_eval(), &good_inspection(), true).0);
    }

    #[test]
    fn m11_convergence_fails_on_each_bad_field() {
        for (k, bad) in [
            ("converged", json!(false)),
            ("rows_compared", json!(0)),
            ("meetings_on_each", json!([2, 1])),
            ("later_rename_won", json!(false)),
            ("notes_on_shared", json!([3, 2])),
            ("own_meeting_reached_other", json!(false)),
            ("later_editor_first_won", json!(false)),
            ("outbox_left", json!([0, 1])),
        ] {
            let mut e = good_eval();
            e[k] = bad;
            assert!(!convergence(&e).0, "{k}");
        }
        assert!(!convergence(&json!({})).0, "an empty report never passes");
    }

    #[test]
    fn m11_no_audio_fails_on_each_bad_field() {
        for (k, bad) in [
            ("relay_rows_opened", json!(13)),
            ("relay_rows_unreadable", json!(1)),
            ("relay_audio_entities", json!(1)),
            ("relay_audio_signatures", json!(1)),
            ("relay_literal_signatures", json!(1)),
            ("scanner_detects_audio", json!(false)),
            ("refused_by_guard", json!(1)),
            ("audio_rows_on_each", json!([1, 2])),
        ] {
            let mut e = good_eval();
            e[k] = bad;
            assert!(!no_audio(&e, &good_inspection(), true).0, "{k}");
        }
        for (k, bad) in [
            ("rows", json!(15)),
            ("rows_over_limit", json!(1)),
            ("presence_over_limit", json!(1)),
            ("largest_presence_bytes", json!(0)),
        ] {
            let mut i = good_inspection();
            i[k] = bad;
            assert!(!no_audio(&good_eval(), &i, true).0, "{k}");
        }
        assert!(
            !no_audio(&good_eval(), &good_inspection(), false).0,
            "type level"
        );
        let mut none = good_eval();
        none["relay_rows_opened"] = json!(0);
        let mut empty = good_inspection();
        empty["rows"] = json!(0);
        assert!(
            !no_audio(&none, &empty, true).0,
            "nothing stored proves nothing"
        );
        assert!(!no_audio(&json!({}), &json!({}), true).0);
    }

    #[test]
    fn m11_executable_is_read_from_cargo_messages() {
        let msgs = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"earshot_runtime"},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"sync_eval"},"executable":"C:/t/sync_eval.exe"}"#,
        );
        assert_eq!(
            executable(msgs, "sync_eval"),
            Some(PathBuf::from("C:/t/sync_eval.exe"))
        );
        assert_eq!(executable(msgs, "earshot-sync-server"), None);
    }
}
