# Sync specification

Status: approved by the owner (2026-11-14), section 14 included. It implements the design approved at GATE-08 (`docs/sync.md`, PLAN D25) and adds nothing to it except the detail needed to build. `docs/sync.md` decides what sync does; this file decides exact formats, calls and names. One deliberate difference: the call table in section 3 replaces the one in `docs/sync.md` section 5 (the same calls, as `GET` and `POST` only). Any other difference is a mistake here and is fixed.

Build work follows this file, task by task.

## 0. Scope and phases

The owner's order (2026-11-14): sync before Android, and the Surface and the desktop talking to each other before anything else is added.

| Phase | What works at the end | Tasks |
|---|---|---|
| 1 | Two computers hold the same meetings and show each other as online | T11.1a, T11.1b, T11.2, T11.2a, T11.3, T11.3a, T11.3b, T11.3c, then T11.4 and GATE-09 |
| 2 | Summary offload between devices; Dictionary sync | T11.3d, T11.3e |
| 3 | The server writes summaries | T11.5, T11.6 |

This moves T11.4 and GATE-09 ahead of T11.3d and T11.3e. Reordering tasks needs the owner's agreement (AGENTS 6.2); it was agreed with this file and PLAN.md is in that order.

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
plaintext = JSON of SyncEnvelope { entity, class, id, meta, record }, plus "format": 3
```

- `"format"` is `earshot_sync::ENVELOPE_FORMAT`: 1 until T12.77b8, 2 from then (voice samples, docs/voice-samples.md 7.3), 3 since T12.93 (the user's ignore on a speaker row). Rows written at formats 1 and 2 still open (a format-2 speaker row reads as not ignored); a row above this build's format is kept unopened (section 5).

- The stamp in the associated data is the row's `updated_at`. The relay cannot attach a ciphertext to another row key or stamp without the open failing.
- Opening checks, in order: the leading byte, the tag, that the envelope's `id` hashes to the row key, that `meta.updated_at` equals the stamp sent beside it. Any failure is `Unreadable` with a fixed reason word, never the bytes.
- Before sealing, the plaintext is refused if it is over 192 KiB or holds an audio container's signature: raw `fLaC` or `OggS`, raw `RIFF` with `WAVE` four bytes later ("RIFF" alone is an English word a transcript can hold), or any of the three magics as serde writes a byte vector (comma-separated decimals) or as the first five characters of base64 (`AudioSignature`). This is the client half of `sync.no_audio`; the type system is the first.

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
| `POST /v1/rows` | token | `{format, rows: [{key, wall_ms, logical, node, body}], seen_seq?, epoch?}` | `{accepted, superseded: [key], seq, epoch?, max_seq?}` |
| `GET /v1/rows?after=<seq>&limit=<n>[&seen=<seq>&epoch=<base64>]` | token | | `{rows: [{seq, key, wall_ms, logical, node, body}], more, vault_format, epoch?, max_seq?}` |
| `POST /v1/presence` | token | `{blob}` | `{}` |
| `GET /v1/presence` | token | | `{devices: [{device_id, age_s, blob}]}` |
| `GET /v1/vault` | token | | `{vault_format, devices: [{device_id, created_unix_s, age_s}]}` |
| `POST /v1/devices/revoke` | token | `{device_id}` | `{}` |
| `GET /v1/health` | none | | `{ok, version}` (added in T11.2: checking a deployment by hand) |

Rules:

- **Token first** (T14.46). On every call that takes a token, the token is checked before the call's body is read. A caller the relay does not know is answered `401 unknown_device` whatever its body holds; what it was sending is taken off the connection and dropped as it comes (at most 4 MiB, for at most 10 seconds), never held, so the answer reaches a removed device that is in the middle of a large push. An unknown path or method is `404 not_found` with or without a token.
- **Registration.** With `setup_secret`: accepted when it equals the container's secret (the SHA-256 digests of the two are compared in constant time, so neither a matching part nor the length shows in the time taken). The key check is compared in constant time too. With `slot`: accepted when that pairing slot exists and has been taken, and the device id is new or revoked (Coming back, below). Either way `key_check` must equal the vault's stored check, or be stored as the vault's check if it has none. A mismatch is `409 key_mismatch`: the device is told to join with a pairing code.
- **Pairing slots.** One use, 10 minutes. `take` deletes the wrapped key and marks the slot taken; the slot stays valid for one registration for the rest of its 10 minutes.
- **Push.** A row is accepted when the relay holds no row for `key` or its stamp is later; otherwise its key is listed in `superseded` and nothing changes. An accepted row replaces the old one and gets the next `seq`. `seq` in the response is the vault's highest.
- **Far stamps** (T14.44). A push holding a row whose `wall_ms` is more than 24 hours past the relay's own clock (`wire::MAX_STAMP_AHEAD_MS`, a judgement, the same figure a device holds a pulled row by, section 5.3 step 3) is refused whole with `409 clock_ahead`; nothing of it is stored. The relay keeps the copy with the latest stamp, so one row from a far future would otherwise shadow every later edit of it made where the clock is right. Exactly 24 hours ahead is accepted. A client from before this word (0.4.2 and earlier) reads an unknown word as `server`: a failed push, retried with back-off, its rows still queued.
- **Vault epoch** (T14.45). A relay restored from an older copy of its data keeps its devices' tokens and answers every call, but holds fewer rows than its devices have seen, and numbers new rows below their cursors, where they are never pulled. Three things let every device notice:
  - The vault has an `epoch`, 16 random bytes kept in the relay's database (so it survives restarts and updates; a vault from before T14.45 is given one when its relay first opens the database). Push and pull answers carry it, with `max_seq`: the vault's highest sequence number before the request.
  - A device says what it has seen: on a push `seen_seq` (the highest sequence number any answer has shown it) and `epoch` (the last it saw); on a pull the cursor `after` itself, with `seen` and `epoch` when it has them. A number above `2^62` (`MAX_SEEN_SEQ`) or an epoch that is not 16 bytes is `400`; a cursor above it is read as before and is no claim.
  - When what a device has seen is above the vault's highest number, the relay lost rows. In the same transaction as that request it moves its numbering past the claim (`next_seq = seen + 1`), so no later row is numbered at or below a cursor some device holds and the same claim is not news twice, and it takes a new epoch, unless the device named an epoch other than the vault's (that device saw another history and starts over by itself; the devices holding this epoch were told when it was made).
  Known limits: the relay learns of a restore only from a device that saw more than it holds. A device that had not synced since the copy was made says nothing new, and if such a device pushes enough rows to fill the lost numbers before any other device calls, nobody notices (rare: an up-to-date device ticks within 30 seconds of the relay coming back). A 0.4.2 client never sends its rows again, so rows only it holds stay lost at the relay.
  All five fields are optional both ways. A 0.4.2 client ignores `epoch` and `max_seq` in an answer (its response types never refused unknown fields) and sends none of the request fields; a 0.4.2 relay ignores `seen_seq`, `epoch` and the two query parameters, and answers without `epoch` or `max_seq`. With a client from before T14.45 only its cursor speaks: the relay still moves its numbering past it, which is what makes rows pushed after a restore reach that client.
- **Pull.** Rows with `seq > after`, ascending, at most `limit` (1 to 500, default 200) and at most 4 MiB of bodies. `more` says another page exists.
- **Format.** `format` on a push is the highest envelope format the device writes. The vault's format is the highest ever pushed. A push or pull from a device whose `format` (header `Earshot-Sync-Format`) is lower than the vault's is `409 update_needed`. A pull reads the vault's format in the same read transaction as its rows (T12.77b8), and a push raises it in the transaction that stores its rows, so a device below the format never receives a row of the push that raised it. A push's own format is checked against the vault's in that same transaction (T14.46; the check before the body is read is only an early answer), so a device that was let through just before another raised the vault stores nothing.
- **Limits.** 256 KiB per decoded row body (the client's 192 KiB plaintext limit plus the seal always fits), 4 MiB per request, 16 KiB per presence blob, and 16 KiB for the body of a call that takes no token (`POST /v1/devices`, `POST /v1/pairing/take`; `MAX_OPEN_REQUEST`, T14.46). Over a limit is `413`.
- **Errors.** `{error: <word>}` with a status: `400 bad_request`, `401 unknown_device`, `403 bad_secret`, `404 no_slot`, `404 not_found` (an unknown path or method), `409 key_mismatch`, `409 update_needed`, `409 device_exists` (a pairing code naming a live device id, one registered and not revoked: a code never registers over a live device; only the setup secret does), `409 clock_ahead` (a pushed stamp too far past the relay's clock, T14.44), `413 too_large`, `429 slow_down`, `500 internal`. Every answer that is not a success is one of these. The client maps each word to fixed copy; no server text reaches a window.
- **Rate limits.** `pairing/take` and `devices`: 10 failures per minute per address, then `429`. Behind the reverse proxy every caller has the proxy's address, so the limit is shared: a stranger's ten guesses make the owner wait a minute to register or pair. Accepted for calls made once per device; `X-Forwarded-For` is not trusted.
- **Revoke** (T11.6c). Any device of the vault may revoke any device id, its own or another's: the relay serves one vault, every device it registered holds the vault key, so letting one remove another adds no reach. The revoked device's token stops working at once (`401 unknown_device` on its next call, its own revokes included) and its presence is dropped; the rows it pushed stay. An id already revoked, or one the relay never registered, answers `{}` and changes nothing: revoking never registers or revives a device. A device that removes itself and one that removes another use the same call.
- **Coming back** (T11.6c, the owner, 2026-11-16). Removal stops a device reaching the relay. It can come back only through a pairing code shown on a live device of the vault: a registration with a taken, live slot and a revoked device id registers that id again (a fresh token, `revoked` cleared, no presence until it sends one, its first date kept). A registration with a slot never registers over a live id (`device_exists`), and a wrong, untaken or used slot leaves a revoked id revoked (`no_slot`). The setup secret registers any id, as before. Removal does not take back the vault key the device holds, so a lost computer is wiped as well. One residual: a rejoin the relay registers but whose key or token this device then fails to store leaves a live id nobody holds a token for, so the next code is `device_exists`; recovery is Remove from another computer, then join again (`sync_a_rejoin_that_could_not_be_kept_is_recovered_by_removing_it_again`).
- **Format on push.** The body's `format` must equal the `Earshot-Sync-Format` header, or `400`: one bad push cannot raise the vault past a format any device writes. The vault's format only rises; lowering it needs the relay's data reset (not yet a command).

## 4. Relay (T11.2)

`earshot-sync-server`: axum on tokio, SQLite through rusqlite, one process, one data volume.

```
device(device_id BLOB PK, token_sha256 BLOB, created_unix_s, last_seen_unix_s, presence BLOB, presence_unix_s, revoked INTEGER)
row(key BLOB PK, wall_ms, logical, node BLOB, seq INTEGER UNIQUE, device_id BLOB, body BLOB)
pairing(slot BLOB PK, wrapped BLOB, expires_unix_s, taken INTEGER)
vault(format INTEGER, key_check BLOB, next_seq INTEGER, epoch BLOB)
```

- The database's `user_version` is 2 since T14.45 (`vault.epoch`, added and filled by a step that touches no row). A relay refuses a database with a higher version than it writes, so a relay cannot go back to an image from before a step once a newer one has opened its data.

- Tokens are 32 random bytes; only the SHA-256 is stored.
- A revoke sets `revoked` and clears `presence`; nothing else of the device, and none of its rows, is touched.
- Rows are never deleted: a tombstone is a row like any other, so a device away for months still hears of deletions.
- Configuration by environment: `EARSHOT_SYNC_SETUP_SECRET` (required, at least 24 characters, refused otherwise), `EARSHOT_SYNC_DATA` (default `/data`), `EARSHOT_SYNC_LISTEN` (default `0.0.0.0:8787`). TLS is the reverse proxy's.
- Logs hold a method, a path (no query), a status and a duration. Never a body, a token, a secret, a slot or a device id.
- Release (T11.2b, the owner, 2026-11-14): the relay lives in this repo and ships with every Windows release under the same version, as `ghcr.io/pitslug/earshot-server:<version>` and `latest` (named `earshot-sync-server` until 0.3.11, the owner, 2026-11-17; 0.4.0 is pushed under both names, and the data volume keeps its name), built for linux/amd64 by a GitHub Actions workflow that runs only on a release tag and builds nothing else. It is a registry image, not a file on the release, so the in-app updater's files are unchanged; `docker-compose.yml` is attached to the release.
- Deliverables: the crate, a `Dockerfile` (multi-stage, non-root user, data volume), a `docker-compose.yml` with the relay only.
- Net guard (GATE-08 item 5): the crate is left out of the app's check and gets its own ci step: its graph holds no HTTP client crate other than the `hyper` and `hyper-util` axum serves with, and no workspace member depends on it through a normal or build edge. A dev-dependency is allowed (T11.3: the runtime's sync tests run the real relay in process through `LocalRelay`); a test build never ships.

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
- For every table in `SYNCED_TABLES`: `AFTER INSERT` and `AFTER UPDATE` triggers that insert `('<tbl>', NEW.id)` into `sync_outbox` only when it is absent (`INSERT ... SELECT ... WHERE NOT EXISTS`). Not `INSERT OR IGNORE`: SQLite replaces a trigger's conflict clause with the firing statement's, and the store writes with upserts, so an ignored conflict became an error on a row's second change (found by the T11.1b tests). A test pins the triggers to `SYNCED_TABLES`, so a new synced table without triggers fails.
- The outbox fills whether or not sync is on. Migration 6 also queues every row that already exists (triggers see only later changes), so turning sync on later sends everything. The table stays small: one entry per row ever changed, a table name and 16 bytes.
- `Store::outbox` reads entries in `updated_at` order across tables, so a meeting goes out before its tracks, and a deletion's cleared rows (core deletion writes children first) before its meeting tombstone.

### 5.3 `Store::apply_remote`

Input: a page of opened envelopes with their `seq` and row key, the device's kind switches, the set of meetings leased in this process, the clock. One IMMEDIATE transaction with `PRAGMA defer_foreign_keys = ON`. For each envelope, in `seq` order:

1. **Kind off.** A `voiceprint` row with voiceprint sync off, or a `dictionary_word` row with Dictionary sync off: skip. (The pull cursor is reset when a switch is turned on.)
2. **Newer format.** `format` above this build's, or an `entity` this build does not know: into `sync_pending` with reason `format`. It is kept, never dropped, and applied after an update.
3. **Future stamp.** `updated_at.wall_ms` more than 24 h past the device clock: `sync_pending`, reason `clock`.
4. **Leased.** The row's meeting (its own id for a meeting, `meeting_id` otherwise) is in the leased set: `sync_pending`, reason `busy`.
5. **Parent missing.** A row with a `meeting_id` (or `track_id`, `speaker_id`) that names no local row and no earlier envelope of this page: `sync_pending`, reason `parent`.
6. **Compare.** No local row, or the remote `updated_at` is later: write the whole row with the remote metadata unchanged. Otherwise ignore it. Exceptions: a meeting tombstone here is kept (a live meeting row never overwrites it), and two copies of a final transcript segment are compared by `text_edited_at` first, then `updated_at` (section 7, rule 4a).
7. **Side effects of what was written** (section 5.4).
8. Delete `sync_outbox` entries this transaction's own writes created, for the rows it wrote, right after each write; a row re-stamped afterwards (a kept copy, section 7 rule 4a) keeps its new entry and goes out.

Then, still inside the transaction: `Hlc::observe` every applied stamp; run the convergence rules (section 7); store the new pull cursor. Before the page, every `sync_pending` row whose reason may have cleared (`busy`, `parent`) is tried again by the same steps.

Returned: counts of applied, ignored, pending by reason and unreadable, and how many rows other rows belong to (a meeting, a track, a speaker) were written for the first time (`Applied::parents_written`, T14.42: the client then tries rows held `parent` again in the same tick, section 6.2). Never row content.

Held rows are counted (`Store::held_count`) and read a page at a time (`Store::held_page`) by reason in SQL, in row-key order from a given key (the table's own order, so no sort over the sealed bodies); nothing reads every held row into memory but tests.

### 5.4 Side effects

- **Meeting tombstone** (remote `deleted`, local row live, whatever the two stamps: a meeting row edited here after the deletion must not stay live over the children the deletion cleared; a tombstone stamped more than 24 h ahead deletes nothing and is held): before the page is applied, the client (T11.3) runs the ordinary `Services::delete_meeting` for that meeting, so audio files go before rows, device-local keys are erased and a lease refuses it (the row is then held `busy`) exactly as a local deletion. Its own tombstone, stamped here, is newer than the pulled one, which is then ignored; both say deleted. The rows the other device cleared arrive in the same pull and apply as ordinary rows.
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
| | `settings/sync` | `on`/`off`: whether the loop runs (absent is off; set on by setting up or joining) |
| | `settings/device-name` | this device's shown name (default: the computer name) |
| | `settings/sync-voiceprints` | off by default |
| | `settings/sync-dictionary` | on by default (phase 2) |
| | `sync-cursor` | `version u32 LE | JSON {pull_seq, seen_seq?, epoch?, resets?}` (the last three since T14.45, each left out while empty, so a record without them is the old one byte for byte and an older build reads a newer record as its `pull_seq`) |
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
  2. pull: try held rows again (opened anew, one page); GET rows after the cursor; open
           each; Store::apply_remote; store the cursor; if the page wrote a row others belong
           to for the first time, try the rows held `parent` again now; repeat while more.
  3. presence: every 60 s (section 8).
```

- The recording meeting's rows and switched-off kinds are left out by the outbox query itself (`Store::outbox_except`), so a long recording cannot fill every page.
- The cursor is written right after its page's transaction, not inside it (the store does not know the runtime's record keys): a page applied twice after a crash between the two changes nothing, as its rows are no longer newer.
- Local writes are found by reading the outbox's length every 2 s and ticking once it stops changing: no writer has to know about sync.
- A failed request ends the tick, records a problem word and backs off: 30 s, 1 min, 2 min, up to 15 min, back to 30 s on the first success.
- `update_needed`, `key_mismatch`, `unknown_device` (this device was removed), a refused address and a missing key or token stop the loop until the user acts; the Sync page says which. A credential store that fails to answer is a retry.
- A row refused before sealing (section 2.3: over 192 KiB, or holding an audio container's signature) stays queued and is refused again each tick; it never stops the rows around it. Each such row is counted once per tick (`TickReport.refused`), the last completed tick's count is kept in `sync-status` (`refused`, left out when 0), the Sync page shows a note with the count (`refused_note` in `fixtures/ui/sync_copy_cases.json`) and leaves those rows out of "waiting to send", as it does damaged rows (T14.43). If the row is a meeting row, the rows that belong to it are held `parent` on the other devices until it fits.
- A push response must account for every row sent (`accepted` plus `superseded`, each superseded key one that was sent); anything else is `server` and the rows stay queued.
- A row here stamped more than 24 hours past this device's clock is not pushed (T14.44): this device's clock was far ahead when it was written, and its stamps go on from there after the clock is put right. Such rows stay queued and go once time has passed them; the tick sends the older rows, pulls, counts as a sync and leaves the problem word `clock_ahead` in the status. When the relay refuses a push with `clock_ahead` (this device's clock is wrong now, or the relay's is), the tick ends as for any failed request, with the same word, and is retried with back-off. `clock_ahead` never stops the loop. Not yet solved: nothing stamps those rows again, so a device whose clock jumped far ahead cannot send what it writes afterwards until real time passes the jump (a question for the owner; the vault is no longer harmed).
- Held rows are tried again at the start of every tick when held as `busy` or `parent`, once per app start when held as `format`, and once per app start and then hourly (`CLOCK_RETRY_MS`, a judgement; at once when this device's clock reads earlier than at the last try) when held as `clock`, at most 500 a tick. The retry goes on next tick from the row key it stopped at and starts over at the end, so a store holding more than 500 tries each in turn (before T14.42 it tried the same first 500 every tick).
- Rows pulled before their parent apply in the tick that brings the parent (T14.42). The relay files a row at the end each time it changes, so a meeting edited after it was first sent sits after its own lines, and a new computer pulls most rows before the meeting row they belong to. After any page (pulled, or the retry above) that wrote a meeting, track or speaker row that was not here before, every row held `parent` is applied again, 500 per transaction, in passes that repeat until one writes no further such row (a line waits for its track, which waits for its meeting). Each pass that goes on took a row out of the held ones, so the passes end; a stop ends them at the next 500. A page that brought no new parent sets off nothing, so rows whose parent never comes cost one read per tick, not one per page. `TickReport.held` counts a row held `parent` only if it is still held when the tick ends.
- The cursor moves only from the value the page was pulled after, under the store lock (a reset made meanwhile, by turning a kind on, wins). Turning a kind on sets `pull_seq` to 0 and keeps the rest of the record; setting up or joining writes a new record (another vault).
- **A changed vault** (T14.45, `judge_vault`). Every push and pull answer is judged against the `sync-cursor` record. The relay's history changed when it names an epoch other than the stored one, or holds fewer rows than this device has seen (`max_seq` below `seen_seq`, or below the cursor when that is higher, as in a record from before T14.45, which holds only the cursor; from a relay that sends no `max_seq`, a push answer's `seq` less the rows it accepted). The device then, in one store transaction, sets its cursor to the start with the new epoch and queues every synced row again (tombstones too; rows it holds for later are kept, since the relay may no longer hold them), and the tick ends there: `TickReport.vault_reset`. The next tick sends everything (what the relay still holds comes back superseded) and reads everything (rows already here are not newer and change nothing; a meeting, voiceprint or voice sample deleted here stays deleted, section 5.3 step 6 and 10.1, and its tombstone is among the rows sent again). A device that has stored no epoch takes the first one it is given and reads nothing again, unless the numbers already say rows were lost.
- Starting over is bounded: at most 3 times in 24 hours by this device's clock (`MAX_VAULT_RESETS`, `VAULT_RESET_WINDOW_MS`, judgements). A further change inside the window resets nothing, queues nothing and stores nothing; the tick ends with `server` and is retried with back-off, so a relay that keeps answering oddly (two relays behind one address) costs three re-sends a day, not one a tick.
- Stopping the thread ends a tick at its next page.
- The thread holds the store lock only inside the two transactions and the short reads. Never across a request.
- It takes no `WorkPass`: it loads no model, and it does not hold up an update's restart or the app's quit. A tick cut off by the process exiting is safe: each of its writes is one store transaction, an accepted push not yet marked sent comes back superseded and leaves the outbox, and a page applied but not yet recorded in the cursor applies as not newer (T11.3a, which replaced the earlier plan to make `WorkGate::enter_restart` wait for it).
- The app keeps one thread (`SyncHost`): started at app start when sync is on and set up, after setting up or joining, and when turned on; stopped on its own thread when turned off; stopped and waited for before this computer is removed.
- A meeting is never delayed, failed or warned by sync. A recording computer still pulls; rows for the meeting being recorded cannot exist on another device.

### 6.3 Setting up

- **First device**: address and setup secret. Generate `K`, register with `key_check`, store key and token, start the loop.
- **Show a pairing code**: generate the code, `POST /v1/pairing`, show it with a 10 minute countdown.
- **Join**: address and code. `pairing/take`, unwrap `K`, register with `slot` and `key_check`, store, reset the cursor to 0, start the loop.
- Setting up or joining queues every synced row again and drops every held row (`Store::queue_everything`): earlier sends went to no vault or another one, and held rows were sealed under another key.
- **Turn off**: stop the loop. Local data, key and token stay, so turning it on again resumes.
- **Remove this device**: revoke its token, delete key, token, address, cursor, status and the voiceprint switch. Local meetings stay. The host stops the sync thread first and no start may run until the removal ends. A device whose last sync said `unknown_device` (another device removed it) asks the server nothing and only forgets, so it works with the server out of reach; it may then join again with a pairing code from a live device, under its own device id, and pulls from the start (T11.6c).
- **Remove another device** (T11.6c, `remove_other_device`): revoke that device's id with this device's token. Nothing here changes and the sync thread keeps running. This device's own id is refused before any request (`internal`: the page never offers it; this device leaves only through Remove this device). A device that was itself removed hears `unknown_device`. Removal does not take back the vault key the other device holds (no key rotation, docs/sync.md section 3), so the page says to wipe a lost computer too.

## 7. Convergence rules (T11.3b)

Run inside `apply_remote`'s transaction after the page's writes. Each depends only on synced rows.

1. **One Me.** If more than one live person has `is_me` and no `merged_into`: the one with the earliest `created_at` (id as tie-break) stays; each other gets `merged_into` it, as `merge_persons` does: speaker rows and voiceprints keep their link and resolve through the chain. When the kept one still has the default name and a merged one was named by its user, the kept one takes that name (the earliest-created named one). Written with this device's stamp; two devices doing it at once write the same content.
2. **One live summary per meeting.** If a meeting has more than one live summary: the latest `created_at` stays; each other is tombstoned keeping its body, as `store_summary` does to a replaced summary.
3. **Folders.** Two live folders with one name both stay.
4. **A user's line edit wins over a re-transcription that did not see it** (T12.92). The clash: computer A edits, moves or splits a final transcript line (all set `text_edited_at`) while computer B re-transcribes the same meeting before they sync. B's pass tombstones the old copy of A's line and writes new lines over the same stretch, so without a rule the edit is lost or the words appear twice. The user's edit always wins, as a local re-transcription already keeps edited lines and drops new words under them (`drop_words_under_edits`). Three parts:
   - **(a) Edit order for final segments** (section 5.3 step 6). Two copies of one `pass = Final` segment row are compared by `text_edited_at` first (absent is lowest), then `updated_at`; the higher copy is kept. Every device applies the same comparison in both directions: B's tombstone (no `text_edited_at`) never replaces A's edited copy, and A's edited copy replaces B's tombstone whatever the stamps, also when it arrives late after being held (`busy`, `parent`). The relay knows only `updated_at` and keeps the later copy, and a superseded row leaves the outbox, so whenever the copy kept here has an `updated_at` below the losing copy's, it is re-stamped with this device's stamp (`text_edited_at` unchanged; the incoming stamp is observed first, so the new stamp is above it), as the other kept-copy exceptions do; that sends it out and makes it the relay's copy. At most one re-stamp per device per losing copy, and none when the kept copy is already the later one, so nothing ping-pongs. The order applies only while the meeting is live here and only when both copies are `pass = Final`; mixed or Live copies keep plain `updated_at` order. Between two edits the later edit wins (`text_edited_at` is set only by the edit functions, never by a sync write). A write that keeps the edit (a later edit, a speaker merge on the edited copy, a meeting deletion's erasure, which keeps `text_edited_at`) still wins by `updated_at`. A meeting deleted on a computer that never saw an edit is still deleted everywhere: the pulled meeting tombstone deletes the meeting here (`meetings_deleted_elsewhere`), and that erasure keeps the edit stamp. Live (draft) segment rows keep plain `updated_at` order. A meeting deletion's erasure (a tombstone with no text, words, confidence or speaker, as `delete_meeting` leaves it) is compared with any other copy by plain `updated_at` order, so a deletion whose rows arrive over several pull pages (children before the meeting row) never keeps or re-sends an edit's text. Known cost: a speaker merge made on a copy that did not see the edit loses on that line (the edit, with its own speaker, wins).
   - **(b) Pass runs, and the span repair.** Every final segment row carries `pass_run` (a synced id, new in this task, migration 11; `None` for rows written before it): the final pass gives all rows it writes one fresh id per run; an edit, move or split keeps its row's value (a split's second part copies the first's). After the page's writes, for each meeting the page wrote (`Applied::meetings`) that is live and not leased in this process: on each track, every word of a live Final segment without `text_edited_at` whose `pass_run` is `Some` and differs from that of a live edited Final segment of the same track, and that lies under it (core `under_edit`: a timed word overlapping its span, an untimed word starting in `[start, end)`; an edited segment with `end <= start` covers nothing; a segment with no words judged by its span as one word), is dropped. A segment that loses some words is rewritten with the rest (same id, speaker and `pass_run`), one that loses all is tombstoned, each with this device's stamp (an edit of that row made elsewhere still wins by (a), and the rule then runs again over it); two devices write the same content, and a row is written only when its words change, so a second run writes nothing. Lines of the same pass as the edited line are never compared, so a meeting with no clash is never changed; rows from before `pass_run` existed (`None`) are never dropped. Edited segments are never changed. Known edge: a split's parts are at least 1 ms each, so a split at a line's very end can reach 1 ms past it, and a different run's word starting in that millisecond is dropped (accepted). The span test is one function in earshot-core, called by the final pass and by this rule (moved there from earshot-runtime unchanged).
   - **(c) The edited line's speaker.** B's pass tombstones speaker rows no kept line references, so an edited line that wins by (a) can name a tombstoned speaker. After (b), for the same meetings: every tombstoned speaker row referenced by a live edited Final segment is made live again, with its stored name and person, with this device's stamp; two devices write the same content. A "Me" row comes back with role `Room` when the meeting already has a live "Me" row (the user marked another voice as themselves after the re-transcription), so a meeting never has two; rows are revived in id order. Its name is not changed, so no name route (T12.82) is involved; two rows of a meeting can then show the same "Speaker N" until the user renames one (accepted, rare). The re-stamp reads on this device as a user edit at the next re-transcription (`speaker-identities`), so the name is kept (accepted). A speaker a merge retired on another computer comes back for the edited lines that still name it, like the merge's known cost in (a).
   - **Leased meetings.** Rows of a meeting this process is re-transcribing are held (`busy`) and applied after the lease ends, by (a) and then (b) on that later page; the final pass itself already keeps edited rows it reads in its write transaction.
   - **What does not follow.** Device-local `word-choices/<meeting>` and `unknown-lines/<meeting>` are not synced and not changed: their readers ignore entries that no longer hold, and the next local write drops them (a word choice on a rewritten line is judged by its token text). A summary written on B before the edit reached it is marked out of date only when a line it cites was tombstoned by (b) or the edit is later than the summary; otherwise it reads as current until the user re-summarises (accepted, rare).
   - **Tests** go through `LocalRelay` as well as straight `apply_remote` pages, because the relay arbitrates by `updated_at` alone: edit vs re-transcription in both push orders and three devices, a held edit, split and move vs re-transcription, a meeting with an edit and no clash (nothing written), a second run writes nothing, speaker revival, two different edits plus a tombstone on three devices (the later edit wins, then no further writes), a (b) rewrite against an edit of the same line, and a held edit reaching a third device from the re-transcribing one alone (the editing one offline).
   - **Format.** `pass_run` changes the segment row's serialised shape, so `ENVELOPE_FORMAT` goes to 4 in the same commit (format 3 has not shipped; 0.4.3 carries both), and every device runs (a) to (c) before it syncs with another; the relay's format pin, the worker and the relay image move with it. Migration 11 follows the AGENTS migration checklist.
   - **Counts.** `Applied::words_dropped_under_edits`, `Applied::lines_rewritten_under_edits`, `Applied::edited_lines_kept` (incoming copies that lost to a local edit by (a)) and `Applied::speakers_revived`, all counts only.
   - `Repo::replace_segments` (no shipped caller) would tombstone edited rows; a scan test keeps it out of shipped code.

A test delivers the same rows to three simulated devices in every order of two pages and requires identical live rows.

## 8. Presence (T11.3c)

- Blob plaintext: `{v: 1, name, app_version, kind: "app", offers_summaries: false, writer_kind: null, recording: bool}`, sealed with `k_presence` like a row (`aad = "earshot-presence-v1" | device_id`).
- Sent every 60 s and once with `gone: true` when sync is turned off, best effort (5 s). Since T11.3ca also on quit (the send starts first and is waited for at most 1 s, never past the 4 s exit limit) and when Windows is about to sleep (`PowerRegisterSuspendResumeNotification`, waited for at most 1.5 s); while away no beat goes out, every presence send holds one lock so a gone never lands before a beat already on the wire, and after a resume the device ticks at once and beats after the first tick that reaches the relay. The decision is `presence::Beats`, clock-free tested. A thread replaced by a new one says nothing, and removal relies on the relay clearing a revoked device's presence.
- The plaintext is padded to 640 bytes before sealing, so the blob's size says nothing of a flag or the name.
- Online is `age_s <= 150` and not `gone` (a judgement: two missed beats). The rule is one pure function with a clock-free test.
- Presence is advisory. A blob's associated data holds no time, and `age_s` is the relay's, so the relay could replay an old blob as fresh. Nothing decides on presence alone: offload (10.1) also needs the target's claim, which is a synced row the relay cannot forge.
- Phase 1 uses the name, `recording` and the age. Phase 2 fills `writer_kind` and acts on `offers_summaries`.
- What T11.3c ships: `earshot_runtime::sync::presence` (blob, `online`, `clean_name`, `computer_name`); the thread sends presence after its first tick without a problem and every 60 s, and says gone when it is turned off (T11.3ca adds quit and sleep). `settings/device-name` (absent: the computer's name, set by `set_device_name`) and `settings/sync-offer-summaries` (off; `set_offer_summaries`) are stored and sent; the page's switch for the latter comes with T11.3d, where it does something, and no page edits the name yet. The Sync page's computer list shows each other computer's name, Online (and recording), else when it last reached the server; a blob that cannot be opened lists the computer by its short id.

## 9. The Sync page (T11.3a)

A Settings page, "Sync", after People and before Notes and storage. What T11.3a ships:

- **Not set up**: what sync does in two sentences; "Set up the first computer" (address, setup secret, Set up) and "Join with a pairing code" (address, code, Join), each its own form. The secret and code fields are cleared after every attempt.
- **Set up**: "Sync is on" switch; the server address; last sync; rows waiting to send, held for later and that could not be read; the last problem in fixed copy (and that sync is stopped until it is fixed, for a problem that stops it); a line when sync should be running and is not; "Sync now", which starts a thread that should run and does not.
- **Add a computer**: shows a pairing code once asked, until Hide, its expiry, or leaving the page.
- **Computers**: from the server (`GET /v1/vault`): this computer first, others by the first eight hex digits of their id with last seen, until presence brings names (T11.3c).
- **Voiceprints**: "Sync voiceprints" (off by default).
- **Remove this computer**: a confirmation dialog. Removal also turns voiceprint sync off, so joining a vault again asks for it afresh.
- **Remove another computer** (T11.6c): each other computer's row has a Remove (named `Remove <computer>`), confirmed in a dialog that says its synced meetings stay, that removal does not take back the sync key it holds, and to wipe a lost computer too. The list then shows the computers left; focus goes to the list, or back to the Remove on Cancel. A refusal shows in the page's notice bar with the command copy.

Not in T11.3a: revoking another computer (T11.6c), device names, the Dictionary switch (phase 2), an event (the page reads `sync_view` when it opens and after each action).

IPC commands in `apps/desktop/src-tauri/src/sync_settings.rs`: `sync_view`, `sync_devices`, `set_up_sync`, `offer_sync_code`, `join_sync`, `set_sync_on`, `set_sync_voiceprints`, `sync_now`, `remove_sync_device`, `remove_other_sync_device` (T11.6c: by `SyncDeviceView.id`, the hyphenated UUID; returns `SyncRemovedView` with the computers left, or no list when they could not be listed after the revoke, which is still a success and the page asks again; this computer's id or text that is not an id is `invalid_setting` with `remove_other.not_another`). Every command has an `Ipc` method and a mock (`packages/ui/src/ipc/mockSync.ts`). One shared fixture, `fixtures/ui/sync_copy_cases.json`, holds the status copy (`problems`), a button press's copy (`command_problems`: nothing retries a button press, so none says Earshot will try again), the words that stop sync (`stops`), the address rule's cases, and the copy for removing another computer (`remove_other`: the row's button name, the dialog's title, text and confirm, and the refusal); both suites read every key. The key and token live in Credential Manager (`Earshot/sync-key`, `Earshot/sync-token`, `sync_vault.rs`); no response carries them, the setup secret or a used code.

Choices apply at once; Set up and Join submit typed text and are in the save-rule test's list. Screens are judged by screenshot in light and dark.

## 10. Phase 2 interfaces

### 10.1 Summary offload (T11.3d)

Superseded in part by T12.63a (docs/summary-order.md section 4): the chosen helper is now a computer step of the order of summary writers (`settings/summary-order`; `settings/summary-offload-device` is only its mirror for older builds, written by `summary_order::set_summary_order`). A run records itself in `summary-run/<meeting>`, and `ask_for_run` names the request in that record in the request's own transaction. `OffloadHost::write_here` became `continue_run`, decided by the record (`summary_order::run::hand_back`): `Continuation::Order` continues the same run, a named run ends with the computer's outcome, no record is `Continuation::Legacy` (written here as below), an unreadable record or one naming another request is settled with no job; the host answers `Started`, `Refused` or `Settled`. A `Done` request a waiting record names ends the run with that computer as its writer (`end_after_done`). The helper's writer is the first writer step of its order (`summary_order::run::helper_writer`, presence `WriterKind::of_order`), and nothing else runs for a request. The rest of this section is the T11.3da design as built.

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

What T11.3da built (`earshot_runtime::offload`), with the detail this section left open:

- **Names.** Store migration 7 adds the table, its index and its outbox triggers. `ENVELOPE_FORMAT` stays 1 (the coordinator, 2026-11-15): no existing entity changed shape, and an older build holds a row of an entity it does not know as `format` (it is applied after an update), whereas a raised format would make 0.3.7 hold every row from an updated device. States are the words `requested`, `claimed`, `done`, `declined`, `withdrawn`; `reason` is empty except for a decline. The helper's switch is the existing `settings/sync-offer-summaries` (T11.3c), not `settings/summary-offers`.
- **Presence.** `writer_kind` is sent only while the device offers summaries: `local`, `anthropic`, `openai_compatible`, `claude_code`, `codex`, or absent for "no summary". A helper with no writer, or a word this build does not know, is not asked.
- **The decision.** `offload::decide` (pure) and `choose_now` (one presence request, no store lock). It runs in `host::after_meeting` when the summary is due (after the final pass, never for a failed pass or a cancelled summary) through `AfterMeeting::offload`, not at meeting start, because presence changes during a meeting. An asked helper gives `SummaryResult::Offloaded`; a chosen helper that is not asked gives `HostOutcome::offload = HereInstead { why }` (sync off, relay unreachable, offline, not offering, recording, no writer, no final transcript, request not written) and this device's writer runs.
- **The count.** `segment_count` is the meeting's live final segments when asked. A meeting without a final transcript is not offloaded. The helper waits until it holds at least as many, and declines `no_transcript` 5 minutes after claiming (a judgement: a re-transcription on the asker can change the count for good, and a claim must not hang).
- **Steps.** `Offload::step` runs after every sync tick (`SyncThread::start_offloading`), whatever the tick's outcome: the 90 s rule needs no relay. A tick that stops sync (a problem the user must act on, such as this device having been removed) is followed by one last step, `Offload::step_sync_stopped` (T11.6c), before the thread ends: every open request this device asked is withdrawn here at once (no claim window, no presence) and its meeting handed to `write_here` as withdrawn, because nothing this device writes can reach the helper; requests asked of this device are left alone (a claim could not reach the asker, which withdraws on its own rules once this device's presence goes). While the host refuses a hand-back (a job of the meeting runs, it is in use) the step is repeated every tick until each is started or settled, for at most `STOPPED_HANDBACK_LIMIT` (30 minutes, a judgement); a stop asked meanwhile ends it at once on a stopping problem. Turning sync off runs the same hand-back (its thread is retired on a thread of its own, so the page never waits). A withdrawal is final, so if sync works again later it wins on every device; a helper that already stored its summary meanwhile is rule 7.2's case. Every timer runs on the device clock from this device's own readings (device record `offload-times`: when it asked, first saw the claim, claimed, or first found the helper's presence missing; pruned to open requests), never on HLC wall time, which can run ahead of the clock by up to the apply window. The asker withdraws an unclaimed request 90 s after asking, and a claimed one when the helper's presence is at least 300 s old, or has been missing or unreadable for 300 s since the asker first found it so (presence that could not be asked withdraws nothing), then hands the meeting to its host (`OffloadHost::write_here`); a decline is handed the same way. A host that cannot start the write (a job of the meeting runs, the meeting is in use) says so and the next step hands it again. The helper claims, or declines `not_offered` when its switch is off, and hands the request to its host (`OffloadHost::write_for`) once the transcript is there. Each request is handed once per process while it starts; after a restart, a withdrawn or declined request with no summary created since it was asked, live or replaced by an undo, is handed again. Settled requests (done, or declined or withdrawn with a summary since) are not read again in the process, and a declined or withdrawn request whose write here first started, or that a summary settled, more than 7 days ago on this device's clock (`LOOK_BACK_MS`, readings in `offload-times`) is not read at all; one whose write here never started is never aged out. A step's write of `offload-times` re-reads and merges in one transaction, so an `ask` meanwhile is kept. A meeting has at most one open request this device asked: `ask` answers with it (`ask_for` names that request's own device, which the summary step reports), and re-summarise "On <device>" for another computer is refused (`already_waiting`).
- **The helper's write.** `offload::write_for_request`: a queue ticket, the host's writer for the meeting's privacy, its turn, `lease_meeting_alone(Processing)`, then `SummaryService::summarize_answering`, whose write transaction re-reads the request (still claimed by this device, else `Conflict`, nothing stored) and marks it done with the summary. Any failure declines (`failed`, `busy`, `kept_local_no_model`, `no_writer`); a cancel leaves it claimed. In the app the write runs on thread `earshot-offload`; a panic there, or a thread that cannot start, declines `failed` (`offload::decline`).
- **A withdrawal is final.** In `apply_remote`: a pulled withdrawal replaces an open request whatever the stamps (written as it came), and on the asker a withdrawn request is never replaced by an open one (the withdrawal is stamped again by the asker and goes out). A done or declined answer stays last writer wins: a withdrawal and a stored summary can both happen, and rule 7.2 keeps the later summary.
- **Busy.** A summary request is never held as `busy`: it holds no content and must reach a device whose summary job holds the meeting.
- **Authorship.** A request whose last write came from a device not allowed to write its state (`updated_at.node`) is ignored by every reader.

What T11.3db built in the app (`apps/desktop/src-tauri/src/offload.rs`): the Sync page's "Write summaries for my other computers" (`set_sync_offer_summaries`) and the computer's name (`set_sync_device_name`, typed, with its own Save name button); the Summaries page's "Another computer" (`summary_helpers`, `set_summary_helper`), listing each computer that offers summaries as "On <device> (<writer kind>)" and the chosen one whatever it offers now; the meeting's `SummaryEvent::Offloaded` and `MeetingDetail.waiting_for` ("Waiting for <device> to write the summary."), and banner `summary-offload` when the chosen computer was not asked, which says this computer wrote the summary only when one was stored (otherwise the table's `not_asked` words, claiming nothing); re-summarise "Here" or "On <device>" (`ask_summary_helper`, refused with the reason when the computer would not be asked). `HostOffload` re-summarises a handed-back meeting with its reason as the job's first warning and writes another computer's request on thread `earshot-offload` in the meeting driver's queue with a work pass (none to be had declines `busy`). Copy comes only from `fixtures/ui/offload_copy_cases.json`, read by both suites.

### 10.2 Dictionary (T11.3e)

`dictionary_word(id, META, word TEXT NOT NULL)`, class `SyncableOptIn`. Rules as `docs/sync.md` section 9. `settings/vocabulary` stays what summaries read.

The first exception to "a new entity keeps the format" is voice samples (T12.77b8, the owner, 2026-11-18; docs/voice-samples.md 7.3): `ENVELOPE_FORMAT` is 2, because a format-1 build would apply the prints and hold the samples, reading a voiceprint whose voice now lives in its samples as empty. The vault rises at the first push from a 0.4.0 device, samples or not, and every older build and worker in it then gets `update_needed` until it updates.

The format rises again, to 3, when an existing entity changes shape (T12.93, the owner, 2026-11-25): a speaker row carries `ignored`, the user's choice to ignore that voice in that meeting, which before lived only in the recording computer's device-local `speaker-identities` entry and so never reached the other computers. A format-2 build would deserialise a pulled speaker row without the field and write it back whole, undoing the ignore everywhere, so the vault rises at the first push from a build with format 3 and every older build and worker gets `update_needed` until it updates. The field has a serde default: rows written at format 1 or 2 read as not ignored. A row counts as ignored only when it is also unnamed and not the user (`ignored && role != Me && person_id` empty), on every read, because a pulled row is a trust boundary. Each device copies its own legacy device-local ignores onto the rows once, after its first completed pull (or at start when it has no vault), so an edit another computer made arrives first.

It rises to 4 with T12.92 (section 7 rule 4): a final segment row carries `pass_run`, the final-pass run that wrote it. A format-3 build would write a pulled line back without it and without the edit order of rule 4a, so the vault rises at the first push from a build with format 4. The field has a serde default and is left out when absent: rows written at formats 1 to 3 read as having no run, which rule 4b never drops.

What T11.3e ships (store migration 8; a new entity, so no `ENVELOPE_FORMAT` change: an older build holds the rows as `format`):

- `earshot_store::SyncKinds { voiceprints, dictionary }` replaces the voiceprint flag in `outbox_except`, `outbox_len_except` and `ApplyRules`; `ApplyRules::after` runs inside the page's transaction, and the client uses it to rebuild `settings/vocabulary` from the live rows while Dictionary sync is on, so the list never differs from the rows it came from.
- Every Dictionary write keeps one row per word, whether or not sync is on, and reads the stored list and writes the rows and the list in one transaction, so no sync apply can fall between them. The page edits one word at a time (`vocabulary::add_entry`, `remove_entry`; transcript words through `add_word` and `remove_word`) and never sends a whole list, so it cannot remove a word another computer added after it read the list. The one whole-list path, `set_vocabulary(base, words)` (the benchmark's fresh scratch store), takes the list its caller read and refuses with `messages.stale_list` when the stored list is no longer that; its rows follow the change from `base`. A removal removes every live row of the word (matched by the join's own normalisation, so a row from another build goes too), each a tombstone with its word cleared.
- While sync is on and Dictionary sync is not off (`vocabulary::dictionary_syncs`, read inside each transaction), the stored list is the joined list (`vocabulary::rebuild_tx`): live rows in creation order, the same word (trimmed, end quotes dropped, read by `Vocabulary::new` and lower-cased) once with the earliest spelling, a word `Vocabulary::new` refuses on its own not in use, and words taken until the next would pass 100 words or 1000 characters (a prefix: everything after the first word that does not fit is not in use). Off, the list is the device's own, as written. While any word is not in use an add is refused (`messages.add_not_in_use`), because the new word would be stored and synced but not in use.
- A stored list this version refuses is not replaced by a pulled page or by turning the switch on: it stays and is reported by its reason; the user's own add, removal or whole-list save replaces it with the joined list.
- Words stored before rows existed get rows before every tick pulls, inside every write and when the switch is turned on (`vocabulary::reconcile_tx`: a list word with no live row), so a rebuild never loses them. Turning the switch on also resets the pull cursor and rebuilds the list.
- Live summary windows are reused only with exactly the words they were mapped with: a tick that changes no row leaves the list byte for byte, but a changed word, or turning Dictionary sync on (the list goes from the user's order to creation order), maps every window again at the summary (cost only).
- The Dictionary page shows `messages.not_in_use` from `fixtures/ui/summary_settings_cases.json`; the Sync page has "Sync Dictionary words" (`set_sync_dictionary`) before the voiceprint switch. Removing this computer leaves the Dictionary switch as it is.

## 11. Phase 3 interface

`earshot-worker` is a device with `kind: "server"` in its presence blob, `offers_summaries: true`, and the target half of 10.1. Its key comes from a file named by `EARSHOT_WORKER_KEY_FILE`. Nothing in phases 1 or 2 may assume a device has a window, a microphone or Credential Manager: the sync client takes its key and token through `SyncVault` and its store through `Services`, both of which a worker can provide.

What T11.6 built (docs/sync.md section 10, "Running the worker"): the crate `earshot-worker` (`join`, `run`, `health`; configured by `EARSHOT_WORKER_*` variables, none of them a secret), its `SyncVault` over two files (`EARSHOT_WORKER_KEY_FILE`, `EARSHOT_WORKER_TOKEN_FILE`, default under `/data/vault`), presence through `SyncClient::for_address(..).as_summary_worker(writer)` (`kind: "server"`, `writer_kind` located at every send: `local` for the configured model service resolving inside the model network's fixed subnet (`EARSHOT_WORKER_MODEL_SUBNET`), else `openai_compatible`), and `SummaryService::own_model_endpoint`, the one remote service that may write a kept-local meeting, called only by the worker.

## 12. Failure behaviour

| What happens | Result |
|---|---|
| Server unreachable | status line, back-off; nothing else changes |
| Relay restored from an older copy, or its data replaced | each device notices at its next tick (a new epoch, or fewer rows than it has seen), queues every row again and reads the vault from the start; nothing is shown unless it happens more than 3 times in a day (`server`) |
| Token revoked | loop stops; page says this computer was removed |
| Wrong pairing code | fixed copy; nothing stored |
| A row cannot be opened | counted, skipped; the page shows the count |
| A row here cannot be sealed (too large, or audio) | left queued, counted; the page shows a note with the count; everything else is sent |
| Vault format newer than this build | loop stops; page says to update Earshot |
| Row for a meeting in use here | held, applied when the meeting is free |
| Device clock far ahead | its rows are not sent (it finds them, or the relay refuses the push with `clock_ahead`); the page says so; rows that reached a relay from before T14.44 are held on other devices and reported |
| Device clock far behind | rows from the others are held `clock` and tried again hourly; they apply once its clock is right |
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
