# Sync specification

Status: draft for the owner's review (2026-11-14). It implements the design approved at GATE-08 (`docs/sync.md`, PLAN D25) and adds nothing to it except the detail needed to build. `docs/sync.md` decides what sync does; this file decides exact formats, calls and names. One deliberate difference: the call table in section 3 replaces the one in `docs/sync.md` section 5 (the same calls, as `GET` and `POST` only). Any other difference is a mistake here and is fixed.

No code is written until the owner has reviewed this file.

## 0. Scope and phases

The owner's order (2026-11-14): sync before Android, and the Surface and the desktop talking to each other before anything else is added.

| Phase | What works at the end | Tasks |
|---|---|---|
| 1 | Two computers hold the same meetings and show each other as online | T11.1a, T11.1b, T11.2, T11.2a, T11.3, T11.3a, T11.3b, T11.3c, then T11.4 and GATE-09 |
| 2 | Summary offload between devices; Dictionary sync | T11.3d, T11.3e |
| 3 | The server writes summaries | T11.5, T11.6 |

This moves T11.4 and GATE-09 ahead of T11.3d and T11.3e. Reordering tasks needs the owner's agreement (AGENTS 6.2); it is asked for in section 14 and not yet made in PLAN.md.

Phase 1 is specified to the byte. Phases 2 and 3 are specified to the interface, enough that phase 1 leaves room for them; each gets its detail added to this file before its first task starts.

## 1. Terms

- **Vault**: one person's synced data. One vault per server container.
- **Vault key**: 32 random bytes shared by the vault's devices. Never sent to the server unwrapped.
- **Device**: an Earshot install, identified by the store's existing `DeviceId` (the HLC node, `origin_device`).
- **Relay**: `earshot-sync-server`. It never holds the vault key.
- **Row**: one row of a table in `SYNCED_TABLES`, carried as a `SyncEnvelope`.
- **Sealed row**: a row encrypted for the relay.

## 2. Cryptography (`earshot-sync` crate, T11.2a)

A new adapter crate. No HTTP, no store, no file access. Dependencies: `chacha20poly1305`, `hkdf`, `hmac`, `sha2` (RustCrypto, MIT or Apache-2.0), `getrandom` for the operating system's generator. None of these enters `earshot-core`.

### 2.1 Keys

All derived from the vault key `K` with HKDF-SHA256, empty salt, these `info` strings:

| Key | info | Use |
|---|---|---|
| `k_row` | `earshot-sync v1 row` | sealing rows |
| `k_id` | `earshot-sync v1 row-key` | the row key the relay sees |
| `k_presence` | `earshot-sync v1 presence` | sealing presence blobs |
| `k_check` | `earshot-sync v1 check` | the vault's key check |

`VaultKey` has a redacted `Debug`, is wiped on drop (`earshot_net::wipe_string`'s approach, no `unsafe`), and is not `Clone` outside the crate. `os_seeded_rng` is never used for key material.

### 2.2 Row key

`row_key = HMAC-SHA256(k_id, entity id bytes)[0..16]`. Entity ids are unique across tables, so the entity name is not part of it.

### 2.3 Sealed row

```
0x01 | nonce (24 bytes, random) | XChaCha20-Poly1305(k_row, nonce, plaintext, aad)
aad = "earshot-row-v1" | row_key (16) | wall_ms (u64 BE) | logical (u32 BE) | node (16)
plaintext = JSON of SyncEnvelope { entity, class, id, meta, record }, plus "format": 1
```

- The stamp in the associated data is the row's `updated_at`. The relay cannot attach a ciphertext to another row key or stamp without the open failing.
- Opening checks, in order: the leading byte, the tag, that the envelope's `id` hashes to the row key, that `meta.updated_at` equals the stamp sent beside it. Any failure is `Unreadable` with a fixed reason word, never the bytes.
- Before sealing, the plaintext is refused if it is over 192 KiB or contains `fLaC`, `RIFF` or `OggS` at any offset (`AudioSignature`). This is the client half of `sync.no_audio`.

### 2.4 Key check

`key_check = HMAC-SHA256(k_check, "earshot-vault")`, 32 bytes. The relay stores the first one it is given and refuses a device that presents another. It proves two devices hold the same key without the relay learning it.

### 2.5 Pairing code

- 16 random bytes shown as 26 Crockford base32 characters in groups (`XXXXX-XXXXX-XXXXX-XXXXX-XXXXXX`), case-insensitive on entry, `I L O U` mapped as Crockford does.
- `slot = HKDF(code, "earshot-sync v1 pair-slot")[0..16]`, `k_wrap = HKDF(code, "earshot-sync v1 pair-wrap")` (32 bytes).
- `wrapped = 0x01 | nonce (24) | XChaCha20-Poly1305(k_wrap, nonce, K, aad = "earshot-pair-v1")`.
- The code has full entropy, so the relay cannot recover `K` from `slot` and `wrapped`.

## 3. Wire protocol (T11.2, T11.3)

HTTPS, JSON bodies, `Content-Type: application/json`. Only `GET` and `POST`. Binary fields are base64 (standard, padded). Every call but the two marked carries `Authorization: Bearer <token>`; `earshot-net` already treats that header as secret.

Stamps are three fields wherever they appear: `wall_ms` (u64), `logical` (u32), `node` (base64, 16 bytes), ordered as `HlcTimestamp` orders them.

| Call | Auth | Request | Response |
|---|---|---|---|
| `POST /v1/devices` | none | `{device_id, key_check, setup_secret}` or `{device_id, key_check, slot}` | `{token}` |
| `POST /v1/pairing` | token | `{slot, wrapped}` | `{expires_in_s}` |
| `POST /v1/pairing/take` | none | `{slot}` | `{wrapped}` |
| `POST /v1/rows` | token | `{format, rows: [{key, wall_ms, logical, node, body}]}` | `{accepted, superseded: [key], seq}` |
| `GET /v1/rows?after=<seq>&limit=<n>` | token | | `{rows: [{seq, key, wall_ms, logical, node, body}], more, vault_format}` |
| `POST /v1/presence` | token | `{blob}` | `{}` |
| `GET /v1/presence` | token | | `{devices: [{device_id, age_s, blob}]}` |
| `GET /v1/vault` | token | | `{vault_format, devices: [{device_id, created_unix_s, age_s}]}` |
| `POST /v1/devices/revoke` | token | `{device_id}` | `{}` |

Rules:

- **Registration.** With `setup_secret`: accepted when it equals the container's secret (constant-time compare). With `slot`: accepted when that pairing slot exists and has been taken. Either way `key_check` must equal the vault's stored check, or be stored as the vault's check if it has none. A mismatch is `409 key_mismatch`: the device is told to join with a pairing code.
- **Pairing slots.** One use, 10 minutes. `take` deletes the wrapped key and marks the slot taken; the slot stays valid for one registration for the rest of its 10 minutes.
- **Push.** A row is accepted when the relay holds no row for `key` or its stamp is later; otherwise its key is listed in `superseded` and nothing changes. An accepted row replaces the old one and gets the next `seq`. `seq` in the response is the vault's highest.
- **Pull.** Rows with `seq > after`, ascending, at most `limit` (1 to 500, default 200) and at most 4 MiB of bodies. `more` says another page exists.
- **Format.** `format` on a push is the highest envelope format the device writes. The vault's format is the highest ever pushed. A push or pull from a device whose `format` (header `Earshot-Sync-Format`) is lower than the vault's is `409 update_needed`.
- **Limits.** 256 KiB per decoded row body (the client's 192 KiB plaintext limit plus the seal always fits), 4 MiB per request, 16 KiB per presence blob. Over a limit is `413`.
- **Errors.** `{error: <word>}` with a status: `401 unknown_device`, `403 bad_secret`, `404 no_slot`, `409 key_mismatch`, `409 update_needed`, `413 too_large`, `429 slow_down`. The client maps each word to fixed copy; no server text reaches a window.
- **Rate limits.** `pairing/take` and `devices`: 10 failures per minute per address, then `429`.

## 4. Relay (T11.2)

`earshot-sync-server`: axum on tokio, SQLite through rusqlite, one process, one data volume.

```
device(device_id BLOB PK, token_sha256 BLOB, created_unix_s, last_seen_unix_s, presence BLOB, presence_unix_s, revoked INTEGER)
row(key BLOB PK, wall_ms, logical, node BLOB, seq INTEGER UNIQUE, device_id BLOB, body BLOB)
pairing(slot BLOB PK, wrapped BLOB, expires_unix_s, taken INTEGER)
vault(format INTEGER, key_check BLOB, next_seq INTEGER)
```

- Tokens are 32 random bytes; only the SHA-256 is stored.
- Rows are never deleted: a tombstone is a row like any other, so a device away for months still hears of deletions.
- Configuration by environment: `EARSHOT_SYNC_SETUP_SECRET` (required, at least 24 characters, refused otherwise), `EARSHOT_SYNC_DATA` (default `/data`), `EARSHOT_SYNC_LISTEN` (default `0.0.0.0:8787`). TLS is the reverse proxy's.
- Logs hold a method, a path, a status, a device id and byte counts. Never a body, a token, a secret or a slot.
- Deliverables: the crate, a `Dockerfile` (multi-stage, non-root user, data volume), a `docker-compose.yml` with the relay only.
- Net guard (GATE-08 item 5): the crate is left out of the app's check and gets its own ci step: its graph holds no HTTP client crate other than the `hyper` and `hyper-util` axum serves with, and no shipped crate depends on it.

## 5. Store changes (T11.1a, T11.1b)

Two appended migrations, each with its `APPLIED` pin and a migration test from the committed older fixture.

### 5.1 Kept-local (T11.1a)

- `ALTER TABLE meeting ADD COLUMN summary_kept_local INTEGER NOT NULL DEFAULT 0`, then set to 1 for every meeting with a `summary-kept-local/<meeting>` record.
- `Meeting` gains `summary_kept_local: bool` with `serde(default)`.
- Written 1 in the transaction that creates the meeting when `LiveConfig::keep_summary_local`; cleared only by `keep_local::allow_writer`, which also deletes the old record.
- `keep_local::summary_privacy` answers kept when the column is 1, or the record exists, or either cannot be read.
- Both existing decision points (`host::summary_from_settings`, `SummaryService::summarize`) keep their tests, each with a new case where only the column says kept.

### 5.2 Outbox and pending (T11.1b)

```
sync_outbox(tbl TEXT NOT NULL, id BLOB NOT NULL, PRIMARY KEY (tbl, id))
sync_pending(key BLOB PRIMARY KEY, seq INTEGER NOT NULL, wall_ms, logical, node BLOB, body BLOB NOT NULL, reason TEXT NOT NULL)
```

- Both are device-local (no sync metadata, not in `SYNCED_TABLES`).
- For every table in `SYNCED_TABLES`: `AFTER INSERT` and `AFTER UPDATE` triggers doing `INSERT OR IGNORE INTO sync_outbox VALUES ('<tbl>', NEW.id)`. A test pins the triggers to `SYNCED_TABLES`, so a new synced table without triggers fails.
- The outbox fills whether or not sync is on. Turning sync on therefore needs no backfill, and the table stays small: one entry per row ever changed, a table name and 16 bytes.

### 5.3 `Store::apply_remote`

Input: a page of opened envelopes with their `seq` and row key, the device's kind switches, the set of meetings leased in this process, the clock. One IMMEDIATE transaction with `PRAGMA defer_foreign_keys = ON`. For each envelope, in `seq` order:

1. **Kind off.** A `voiceprint` row with voiceprint sync off, or a `dictionary_word` row with Dictionary sync off: skip. (The pull cursor is reset when a switch is turned on.)
2. **Newer format.** `format` above this build's, or an `entity` this build does not know: into `sync_pending` with reason `format`. It is kept, never dropped, and applied after an update.
3. **Future stamp.** `updated_at.wall_ms` more than 24 h past the device clock: `sync_pending`, reason `clock`.
4. **Leased.** The row's meeting (its own id for a meeting, `meeting_id` otherwise) is in the leased set: `sync_pending`, reason `busy`.
5. **Parent missing.** A row with a `meeting_id` (or `track_id`, `speaker_id`) that names no local row and no earlier envelope of this page: `sync_pending`, reason `parent`.
6. **Compare.** No local row, or the remote `updated_at` is later: write the whole row with the remote metadata unchanged. Otherwise ignore it.
7. **Side effects of what was written** (section 5.4).
8. Delete `sync_outbox` entries this transaction's own writes created, for the rows it wrote.

Then, still inside the transaction: `Hlc::observe` every applied stamp; run the convergence rules (section 7); store the new pull cursor. Before the page, every `sync_pending` row whose reason may have cleared (`busy`, `parent`) is tried again by the same steps.

Returned: counts of applied, ignored, pending by reason and unreadable. Never row content.

### 5.4 Side effects

- **Meeting tombstone** (remote `deleted`, local row live): this device's audio files for the meeting are deleted first (file before row, as `Store::delete_meeting`), then in the transaction every local child row still live is cleared and tombstoned with the meeting tombstone's own `updated_at`, the device-local keys of the meeting are erased, and the meeting row is written with the remote metadata. No new local stamp is made, so two devices applying the same deletion do not re-stamp each other.
- **Person tombstone**: `forget_identity_biometrics` for that identity.
- **Voiceprint tombstone**: the local row's `sum` is zeroed as `forget_voiceprints` does.
- After a page that erased anything: `Store::flush_erasures`.

A `Busy` after the commit means the page is committed.

## 6. Client (T11.3)

`earshot_runtime::sync`, one thread `earshot-sync`, started at app start when sync is set up. All requests through one `NetClient` whose allowlist is exactly the sync server's host; every hop is in the audit log.

### 6.1 Stored state

| Where | Key | Holds |
|---|---|---|
| Credential Manager | `Earshot/sync-key` | the vault key |
| Credential Manager | `Earshot/sync-token` | the device token |
| `local_state` (`DeviceRecord`) | `settings/sync-server` | the server address |
| | `settings/device-name` | this device's shown name (default: the computer name) |
| | `settings/sync-voiceprints` | off by default |
| | `settings/sync-dictionary` | on by default (phase 2) |
| | `sync-cursor` | `version u32 LE | JSON {pull_seq}` |
| | `sync-status` | last success, last problem word, counts; never content |

The address follows the summary endpoint's rule: https, or http only for a loopback host; no query, fragment or userinfo; a length limit. A refused stored address is reported by reason, never by its text.

The key and token are reached only through a `SyncVault` trait (save, has, remove, and `with_key` for the sync thread). No IPC response carries either, the pairing code after it is used, or the setup secret; tests assert on the serialised JSON.

### 6.2 The loop

```
every tick (30 s, or at once after a local write, debounced 2 s):
  1. push: read up to 200 outbox entries; for each, read the row, seal it, send.
           A meeting with a Recording lease and its rows are left in the outbox.
           A row of a kind whose switch is off (voiceprints, Dictionary words) is left too.
           After the response, in one transaction: delete each entry whose row still has
           the updated_at that was sent.
  2. pull: GET rows after the cursor; open each; Store::apply_remote; repeat while more.
  3. presence: every 60 s (section 8).
```

- A failed request ends the tick, records a problem word and backs off: 30 s, 1 min, 2 min, up to 15 min, back to 30 s on the first success.
- `update_needed` and `key_mismatch` stop the loop until the user acts; the Sync page says which.
- The thread holds the store lock only inside the two transactions and the short reads. Never across a request.
- It takes no `WorkPass`: it loads no model. `WorkGate::enter_restart` must wait for a tick in progress to end (a new long job joins the restart rule).
- A meeting is never delayed, failed or warned by sync. A recording computer still pulls; rows for the meeting being recorded cannot exist on another device.

### 6.3 Setting up

- **First device**: address and setup secret. Generate `K`, register with `key_check`, store key and token, start the loop.
- **Show a pairing code**: generate the code, `POST /v1/pairing`, show it with a 10 minute countdown.
- **Join**: address and code. `pairing/take`, unwrap `K`, register with `slot` and `key_check`, store, reset the cursor to 0, start the loop.
- **Turn off**: stop the loop. Local data, key and token stay, so turning it on again resumes.
- **Remove this device**: revoke its token, delete key, token, address, cursor and status. Local meetings stay.

## 7. Convergence rules (T11.3b)

Run inside `apply_remote`'s transaction after the page's writes. Each depends only on synced rows.

1. **One Me.** If more than one live person has `is_me` and no `merged_into`: the one with the earliest `created_at` (id as tie-break) stays; each other gets `merged_into` it and its speaker rows are relinked as `merge_into_me` does. Written with this device's stamp; two devices doing it at once write the same content.
2. **One live summary per meeting.** If a meeting has more than one live summary: the latest `created_at` stays; each other is tombstoned keeping its body, as `store_summary` does to a replaced summary.
3. **Folders.** Two live folders with one name both stay.

A test delivers the same rows to three simulated devices in every order of two pages and requires identical live rows.

## 8. Presence (T11.3c)

- Blob plaintext: `{v: 1, name, app_version, kind: "app", offers_summaries: false, writer_kind: null, recording: bool}`, sealed with `k_presence` like a row (`aad = "earshot-presence-v1" | device_id`).
- Sent every 60 s and once on sleep or quit with `gone: true`, best effort.
- Online is `age_s <= 150` and not `gone` (a judgement: two missed beats). The rule is one pure function with a clock-free test.
- Phase 1 uses the name and the age only. Phase 2 fills `offers_summaries` and `writer_kind`.

## 9. The Sync page (T11.3a)

A new Settings page, "Sync", after Notes and storage.

- **Not set up**: what sync does in two sentences; "Set up the first computer" (address, setup secret) and "Join with a code" (address, code).
- **Set up**: the server address; status line (Up to date, Syncing, or the problem in fixed copy); last sync; rows waiting to send; rows waiting to apply with the reason; rows that could not be read.
- **Devices**: name, Online or last seen, this device marked; "Add a computer" shows a pairing code and its countdown; "Remove" on another device revokes it (a confirmation dialog).
- **Switches**: "Sync voiceprints" (off), "Sync the Dictionary" (phase 2).
- **Turn off sync** and **Remove this computer**.

IPC commands in `apps/desktop/src-tauri/src/sync_settings.rs`: `sync_view`, `sync_set_up`, `sync_join`, `sync_pairing_code`, `sync_set_switch`, `sync_set_name`, `sync_turn_off`, `sync_remove_this`, `sync_revoke`, `sync_now`; event `SyncEvent` (joins `EVENT_METHODS`). Every command has an `Ipc` method and a mock; the mock mirrors the address rule and the code format from one shared fixture (`fixtures/ui/sync_cases.json`), run by both suites.

Choices apply at once; the address, secret, code and name are typed text and need their button, which joins the save-rule test's list. Failures are fixed copy by kind. Screens are judged by screenshot in light and dark.

## 10. Phase 2 interfaces

### 10.1 Summary offload (T11.3d)

New synced table and entity (`syncable_row!(SummaryRequest, "summary_request")`):

```
summary_request(id, META, meeting_id BLOB NOT NULL REFERENCES meeting(id),
                target_device BLOB NOT NULL, asking_device BLOB NOT NULL,
                segment_count INTEGER NOT NULL, state TEXT NOT NULL, reason TEXT NOT NULL)
```

States and who may write them:

```
Requested --(target)--> Claimed --(target)--> Done
     |                     |  \--(target)--> Declined(reason)
     \--(asker, 90 s)--\   \--(asker, target stale 5 min)--\
                        `------------------> Withdrawn <----'
```

- Asker, where `host::summary_from_settings` runs: a chosen device that is online, offers summaries and is not recording gets a request; otherwise the device's own writer runs with banner `summary-offload`.
- Target: claims; waits until it holds `segment_count` live segments of the transcript; runs `host::run_summary` under `lease_meeting_alone(Processing)` in its meeting queue; re-reads the request in the summary's write transaction and stores nothing unless it is still `Claimed`.
- A kept-local meeting: the target uses a local model or answers `Declined(kept_local_no_model)`.
- The asker shows "Waiting for <device>" in the meeting status; on `Withdrawn` or `Declined` it runs its own writer.
- Setting: `settings/summary-offload-device` (a device id or empty) on the asker; "Write summaries for my other devices" (`settings/summary-offers`, off) on the helper.

### 10.2 Dictionary (T11.3e)

`dictionary_word(id, META, word TEXT NOT NULL)`, class `SyncableOptIn`. Rules as `docs/sync.md` section 9. `settings/vocabulary` stays what summaries read.

## 11. Phase 3 interface

`earshot-worker` is a device with `kind: "server"` in its presence blob, `offers_summaries: true`, and the target half of 10.1. Its key comes from a file named by `EARSHOT_WORKER_KEY_FILE`. Nothing in phases 1 or 2 may assume a device has a window, a microphone or Credential Manager: the sync client takes its key and token through `SyncVault` and its store through `Services`, both of which a worker can provide.

## 12. Failure behaviour

| What happens | Result |
|---|---|
| Server unreachable | status line, back-off; nothing else changes |
| Token revoked | loop stops; page says this computer was removed |
| Wrong pairing code | fixed copy; nothing stored |
| A row cannot be opened | counted, skipped; the page shows the count |
| Vault format newer than this build | loop stops; page says to update Earshot |
| Row for a meeting in use here | held, applied when the meeting is free |
| Device clock far ahead | its rows are held on other devices and reported |
| Sync thread panics | caught; one status event; the loop restarts on the next app start |
| Store busy | the tick ends and is retried |

## 13. Tests and acceptance

- **T11.2a** `cargo test -p earshot-sync`: seal and open round trip; a changed byte, key, stamp or id refused; audio signatures and oversize refused; pairing code round trip, case and look-alike letters; `Debug` shows no key bytes.
- **T11.1a** store and runtime `kept_local` tests: migration from the committed fixture; the two decision points with the column alone.
- **T11.1b** `cargo test -p earshot-store sync`: one table-driven case per step of 5.3 and per side effect of 5.4; triggers pinned to `SYNCED_TABLES`; an apply leaves no outbox entry of its own; on-disk erasure after a remote deletion.
- **T11.2** `cargo test -p earshot-sync-server`: every call; limits; slot expiry and single use; token hashing; supersede; paging; format refusal; nothing secret in the log.
- **T11.3** `cargo test -p earshot-runtime sync`: two simulated devices and an in-process relay converge after concurrent edits; a recording meeting is not pushed; back-off is clock-free; every audit entry is the relay.
- **T11.3b** the three-device ordering test of section 7.
- **T11.3c** the online rule; presence round trip.
- **T11.3a** desktop IPC tests (no secret in any response) and UI tests.
- **T11.4** `accept m11`: `sync.convergence` and `sync.no_audio` as PLAN 4.2 words them. Reviewer on harness honesty.
- **GATE-09** by hand: the container on the owner's server, the desktop and the Surface paired, a meeting recorded on each appears on the other, a deletion on one removes it and its audio on the other, the relay's database holds no readable text.

Local test servers end connections through the lingering close. No test uses the real Credential Manager except one round trip with a per-run target.

## 14. Decisions for the owner

1. Reorder: T11.4 and GATE-09 after phase 1, before T11.3d and T11.3e (section 0).
2. Device names default to the computer name and travel only inside the encrypted presence blob.
3. The outbox fills even while sync is off (5.2), so turning sync on later needs no scan.
4. A remote deletion deletes this device's audio for that meeting without asking (5.4). It is the same person's deletion, made on another device.
5. Judgements carried from the design: online at 150 s, claim within 90 s, stale target at 5 minutes, a stamp more than 24 h ahead is held.
