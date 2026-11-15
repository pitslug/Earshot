//! `sync_eval`: the two simulated devices `cargo xtask accept m11` runs against a relay
//! (T11.4; PLAN.md 4.2 `sync.convergence` and `sync.no_audio`).
//!
//! `sync_eval --relay <url> --secret <setup secret>` sets up two devices with temporary
//! stores (the first with the setup secret, the second with a pairing code), gives each a
//! meeting of its own with a recording on disk, makes concurrent edits (both rename one
//! meeting, each adds a note to it), ticks both until nothing moves, then prints one JSON
//! object of counts: whether the devices hold the same synced rows, and what audio crossed.
//!
//! The audio half reads every row the relay holds, opened with the vault key (the relay
//! cannot), and checks each plaintext with the client's own guard: no `audio_file` or
//! `export_record` entity, no audio signature in any form. Nothing of any row is printed.

use std::path::PathBuf;
use std::sync::Arc;

use earshot_core::asr::{MeetingTime, Segment, Word};
use earshot_core::capture::TrackId;
use earshot_core::diar::SpeakerRole;
use earshot_core::identity::{EntityId, HlcTimestamp};
use earshot_core::model::{
    AudioFile, AudioFileRepo, AudioFormat, ClockDomain, Meeting, MeetingRepo, Note, SegmentRow,
    Speaker, Stamp, Summary, Track, TranscriptPass,
};
use earshot_core::prompt::Template;
use earshot_core::summary::SummaryV1;
use earshot_core::voiceprint::VoiceprintRepo;
use earshot_runtime::Services;
use earshot_runtime::sync::{
    MemoryVault, SyncClient, SyncVault, join_with_code, offer_pairing_code, set_up_first_device,
};
use earshot_store::{DataDir, Store};
use earshot_sync::wire::{FORMAT_HEADER, PullResponse};
use earshot_sync::{ENVELOPE_FORMAT, Opened, RowKey, check_plaintext, open_row};
use rand_chacha::ChaCha8Rng;
use rand_core::SeedableRng;
use serde_json::json;

/// The most tick rounds before the devices must have settled.
const MAX_ROUNDS: usize = 8;

struct Device {
    svc: Services,
    vault: Arc<MemoryVault>,
    _dir: TempDir,
}

/// A temporary folder removed on drop.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn device(tag: &str) -> Device {
    let dir = std::env::temp_dir().join(format!("earshot-sync-eval-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp folder");
    let data = DataDir::new(dir.join("data"));
    data.create().expect("data folder");
    let store = Store::open(&data.db_path()).expect("store");
    let svc = Services::new(store, data, Arc::new(earshot_net::SystemClock)).expect("services");
    Device {
        svc,
        vault: Arc::new(MemoryVault::default()),
        _dir: TempDir(dir),
    }
}

impl Device {
    fn vault(&self) -> Arc<dyn SyncVault> {
        Arc::clone(&self.vault) as Arc<dyn SyncVault>
    }
}

/// An invented meeting with a final transcript, a note, a summary and a recording on disk
/// (an `audio_file` row and the file: never synced).
fn meeting(dev: &Device, title: &str, seed: u64) -> EntityId {
    let clock = Arc::clone(&dev.svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let audio_dir = dev.svc.data.root().join("audio");
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let now = clock.now_unix_ms();
    let mut m = Meeting::new(title, now, 600, Template::General, &mut stamp);
    m.duration_ms = Some(60_000);
    let mic = Track::new(m.id, TrackId::Mic, None, ClockDomain::Replay, &mut stamp);
    let me = Speaker::new(m.id, SpeakerRole::Me, "Me", None, &mut stamp);
    let words: Vec<Word> = "shall we ship the invented release on friday"
        .split(' ')
        .enumerate()
        .map(|(i, w)| Word::new(w, 1_000 + 400 * i as u64, 1_300 + 400 * i as u64, 0.9))
        .collect();
    let mut seg = Segment::from_words(
        EntityId::generate(stamp.clock, &mut *stamp.rng),
        mic.id,
        words,
        MeetingTime::from_millis(1_000),
    );
    seg.speaker_id = Some(me.id);
    let note = Note::new(
        m.id,
        MeetingTime::from_millis(2_000),
        "an invented note",
        &mut stamp,
    );
    let summary = Summary::new(
        m.id,
        Template::General,
        None,
        SummaryV1 {
            title: title.into(),
            tldr: "An invented summary.".into(),
            enhanced_notes: vec![],
            decisions: vec![],
            action_items: vec![],
            open_questions: vec![],
            topics: vec![],
        },
        &mut stamp,
    );
    let audio_id = EntityId::generate(stamp.clock, &mut *stamp.rng);
    store.put_meeting(m.clone()).expect("meeting");
    store.put_track(mic.clone()).expect("track");
    store.put_speaker(me).expect("speaker");
    let row = SegmentRow::new(m.id, TranscriptPass::Final, seg, stamp.hlc, stamp.clock);
    store.put_segment(row).expect("segment");
    store.put_note(note).expect("note");
    store.put_summary(summary).expect("summary");
    // A recording: FLAC-looking bytes on disk and its row, which must never leave.
    let rel = format!("audio/{}/mic.flac", m.id);
    std::fs::create_dir_all(audio_dir.join(m.id.to_string())).expect("audio folder");
    let mut bytes = b"fLaC".to_vec();
    bytes.resize(64 * 1024, 0x5A);
    std::fs::write(dev.svc.data.root().join(&rel), &bytes).expect("audio file");
    store
        .put_audio_file(AudioFile {
            id: audio_id,
            meeting_id: m.id,
            track_id: mic.id,
            path: rel,
            format: AudioFormat::Flac,
            sample_rate_hz: 16_000,
            frames: 16_000,
            size_bytes: bytes.len() as u64,
            pipeline_version: "eval".into(),
            aec_version: None,
            created_unix_ms: now,
        })
        .expect("audio row");
    m.id
}

fn rename(dev: &Device, id: EntityId, title: &str) {
    let clock = Arc::clone(&dev.svc.clock);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    let mut m = store.meeting(id).expect("read").expect("meeting");
    m.title = title.into();
    m.meta.touch(hlc, &*clock);
    store.put_meeting(m).expect("rename");
}

fn add_note(dev: &Device, id: EntityId, text: &str, seed: u64) {
    let clock = Arc::clone(&dev.svc.clock);
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut db = dev.svc.store();
    let (store, hlc) = db.parts();
    let mut stamp = Stamp {
        clock: &*clock,
        hlc,
        rng: &mut rng,
    };
    let note = Note::new(id, MeetingTime::from_millis(3_000), text, &mut stamp);
    store.put_note(note).expect("note");
}

/// Every synced row a device holds, in a fixed order, as text (compared, never printed).
fn synced_rows(dev: &Device) -> Vec<String> {
    let db = dev.svc.store();
    let mut out = Vec::new();
    let mut meetings = db.meetings().expect("meetings");
    meetings.sort_by_key(|m| m.id);
    for m in meetings {
        out.push(format!("{m:?}"));
        let mut parts: Vec<String> = Vec::new();
        parts.extend(
            db.tracks(m.id)
                .expect("tracks")
                .iter()
                .map(|r| format!("{r:?}")),
        );
        parts.extend(
            db.speakers(m.id)
                .expect("speakers")
                .iter()
                .map(|r| format!("{r:?}")),
        );
        parts.extend(
            db.segments(m.id)
                .expect("segments")
                .iter()
                .map(|r| format!("{r:?}")),
        );
        parts.extend(
            db.notes(m.id)
                .expect("notes")
                .iter()
                .map(|r| format!("{r:?}")),
        );
        parts.extend(
            db.summaries(m.id)
                .expect("summaries")
                .iter()
                .map(|r| format!("{r:?}")),
        );
        parts.sort();
        out.extend(parts);
    }
    let mut people: Vec<String> = db
        .persons()
        .expect("persons")
        .iter()
        .map(|p| format!("{p:?}"))
        .collect();
    people.sort();
    out.extend(people);
    out
}

/// Whether any string value in `v` (decoded, at any depth) holds an audio container's
/// signature at any offset: the spec's literal rule, independent of the client's guard.
fn holds_signature(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) => ["RIFF", "fLaC", "OggS"].iter().any(|m| s.contains(m)),
        serde_json::Value::Array(a) => a.iter().any(holds_signature),
        serde_json::Value::Object(o) => o.values().any(holds_signature),
        _ => false,
    }
}

/// What the relay holds, every row opened with the vault key and its plaintext checked twice
/// (the spec's literal scan of decoded strings, and the client's guard):
/// (rows, unreadable, audio entities, literal hits, guard hits).
fn scan_relay(dev: &Device, relay: &str) -> (u64, u64, u64, u64, u64) {
    let secrets = dev.vault.load().expect("vault").expect("secrets");
    let keys = secrets.key.keys();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let base = earshot_net::Url::parse(relay).expect("relay address");
    let mut allow = earshot_net::Allowlist::new();
    allow.allow_url(&base).expect("allowlist");
    let net = earshot_net::NetClient::new(earshot_net::NetConfig::new(
        allow,
        Arc::new(earshot_net::SystemClock),
    ))
    .expect("client");
    let (mut rows, mut unreadable, mut audio_entities, mut literal, mut signatures) =
        (0, 0, 0, 0, 0);
    let mut after = 0u64;
    loop {
        let mut url = base.join("v1/rows").expect("url");
        url.query_pairs_mut()
            .append_pair("after", &after.to_string())
            .append_pair("limit", "500");
        let req = earshot_net::NetRequest::get(url)
            .header(FORMAT_HEADER, ENVELOPE_FORMAT.to_string())
            .header(
                "Authorization",
                format!("Bearer {}", secrets.token.as_str()),
            );
        let resp = runtime.block_on(net.send(req)).expect("pull");
        assert_eq!(resp.status, 200, "the relay refused a pull");
        let page: PullResponse = serde_json::from_slice(&resp.body).expect("pull JSON");
        for r in &page.rows {
            rows += 1;
            after = after.max(r.seq);
            let node: Option<[u8; 16]> = r.node.0.as_slice().try_into().ok();
            let key: Option<[u8; 16]> = r.key.0.as_slice().try_into().ok();
            let (Some(node), Some(key)) = (node, key) else {
                unreadable += 1;
                continue;
            };
            let stamp = HlcTimestamp {
                wall_ms: r.wall_ms,
                logical: r.logical,
                node: earshot_core::identity::DeviceId::from_bytes(node),
            };
            match open_row(&keys, RowKey(key), stamp, &r.body.0) {
                Ok(Opened::Row(row)) => {
                    if row.entity == "audio_file" || row.entity == "export_record" {
                        audio_entities += 1;
                    }
                    let plain = serde_json::to_vec(&json!({
                        "entity": row.entity,
                        "record": row.record,
                    }))
                    .expect("plaintext");
                    if check_plaintext(&plain).is_err() {
                        signatures += 1;
                    }
                    if holds_signature(&row.record) {
                        literal += 1;
                    }
                }
                Ok(Opened::NewerFormat(_)) | Err(_) => unreadable += 1,
            }
        }
        if !page.more || page.rows.is_empty() {
            break;
        }
    }
    (rows, unreadable, audio_entities, literal, signatures)
}

/// Whether the check the relay scan uses flags audio smuggled into a field (raw FLAC, a WAV
/// header, serde's number array, base64) and passes text with the word "riff" in it.
fn scanner_detects_audio() -> bool {
    let flagged = [
        json!({"entity": "note", "record": {"text": "fLaC and more"}}),
        json!({"entity": "note", "record": {"text": "RIFF\u{24}\u{8}\u{0}\u{0}WAVEfmt "}}),
        json!({"entity": "note", "record": {"bytes": [102, 76, 97, 67, 0]}}),
        json!({"entity": "note", "record": {"text": "ZkxhQwAAACIQAA"}}),
    ];
    let clean = json!({"entity": "note", "record": {"text": "a guitar riff on friday"}});
    let refused =
        |v: &serde_json::Value| check_plaintext(&serde_json::to_vec(v).expect("json")).is_err();
    let literal = [
        json!({"text": "RIFF\u{24}\u{8}\u{0}\u{0}WAVEfmt "}),
        json!({"nested": [{"text": "x fLaC"}]}),
        json!({"text": "OggS"}),
    ];
    flagged.iter().all(refused)
        && !refused(&clean)
        && literal.iter().all(holds_signature)
        && !holds_signature(&json!({"text": "a guitar riff on friday"}))
}

fn arg(name: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| panic!("missing {name}"))
}

fn main() {
    let relay = arg("--relay");
    let secret = arg("--secret");
    let a = device("a");
    let b = device("b");
    set_up_first_device(&a.svc, &*a.vault, &relay, &secret).expect("set up the first device");
    let offer = offer_pairing_code(&a.svc, a.vault()).expect("pairing code");
    join_with_code(&b.svc, &*b.vault, &relay, offer.code()).expect("join");
    drop(offer);

    let shared = meeting(&a, "Invented planning", 1);
    let own_b = meeting(&b, "Invented review", 2);
    let (ca, cb) = (
        SyncClient::new(a.svc.clone(), a.vault()).expect("client a"),
        SyncClient::new(b.svc.clone(), b.vault()).expect("client b"),
    );
    let mut refused = 0usize;
    for c in [&ca, &cb, &ca] {
        refused += c.tick().expect("first exchange").refused;
    }
    // Concurrent edits: both rename the shared meeting and add a note before either syncs;
    // B edits later by the wall clock, so its title wins.
    rename(&a, shared, "Renamed on the first device");
    add_note(&a, shared, "a note from the first device", 3);
    std::thread::sleep(std::time::Duration::from_millis(5));
    rename(&b, shared, "Renamed on the second device");
    add_note(&b, shared, "a note from the second device", 4);
    // A second conflict on B's meeting: the later editor (A) syncs first, so its push lands
    // before B's older one, which the relay supersedes; A's title must still win on both.
    rename(&b, own_b, "Renamed first, on the second device");
    std::thread::sleep(std::time::Duration::from_millis(5));
    rename(&a, own_b, "Renamed later, on the first device");
    let mut rounds = 0;
    loop {
        rounds += 1;
        let ra = ca.tick().expect("tick a");
        let rb = cb.tick().expect("tick b");
        refused += ra.refused + rb.refused;
        let moved = ra.pushed + ra.written + rb.pushed + rb.written;
        if (moved == 0 && rounds > 1) || rounds >= MAX_ROUNDS {
            break;
        }
    }
    let (rows_a, rows_b) = (synced_rows(&a), synced_rows(&b));
    let title = |d: &Device| {
        d.svc
            .store()
            .meeting(shared)
            .ok()
            .flatten()
            .map(|m| m.title)
    };
    let notes_on = |d: &Device| d.svc.store().notes(shared).map(|n| n.len()).unwrap_or(0);
    let meetings_on = |d: &Device| d.svc.store().meetings().map(|m| m.len()).unwrap_or(0);
    let audio_rows_on = |d: &Device| {
        d.svc
            .store()
            .all_audio_files()
            .map(|f| f.len())
            .unwrap_or(0)
    };
    let title_of =
        |d: &Device, id: EntityId| d.svc.store().meeting(id).ok().flatten().map(|m| m.title);
    // Presence from both devices, so the relay's presence sizes are checked on real blobs.
    ca.send_presence(false).expect("presence a");
    cb.send_presence(false).expect("presence b");
    let (relay_rows, unreadable, audio_entities, literal, signatures) = scan_relay(&a, &relay);
    let out = json!({
        "rounds": rounds,
        "converged": rows_a == rows_b,
        "rows_compared": rows_a.len(),
        "meetings_on_each": [meetings_on(&a), meetings_on(&b)],
        "later_rename_won": title(&a).as_deref() == Some("Renamed on the second device")
            && title(&b).as_deref() == Some("Renamed on the second device"),
        "notes_on_shared": [notes_on(&a), notes_on(&b)],
        "own_meeting_reached_other": b.svc.store().meeting(shared).ok().flatten().is_some()
            && a.svc.store().meeting(own_b).ok().flatten().is_some(),
        "audio_rows_on_each": [audio_rows_on(&a), audio_rows_on(&b)],
        "outbox_left": [
            a.svc.store().outbox_len().unwrap_or(u64::MAX),
            b.svc.store().outbox_len().unwrap_or(u64::MAX),
        ],
        "refused_by_guard": refused,
        "relay_rows_opened": relay_rows,
        "relay_rows_unreadable": unreadable,
        "relay_audio_entities": audio_entities,
        "relay_audio_signatures": signatures,
        "relay_literal_signatures": literal,
        "later_editor_first_won": title_of(&a, own_b).as_deref() == Some("Renamed later, on the first device")
            && title_of(&b, own_b).as_deref() == Some("Renamed later, on the first device"),
        // The scan's own control: the guard it uses flags audio in a plaintext.
        "scanner_detects_audio": scanner_detects_audio(),
    });
    println!("{out}");
}
