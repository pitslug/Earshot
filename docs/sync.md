# Sync design (T11.1, for GATE-08)

Status: approved at GATE-08 (the owner, 2026-11-14), as written. Nothing here is built yet. PLAN D25 records the decision.

Decisions already taken by the owner (2026-11-14):

- A self-hosted container is the meeting point, not a direct link between devices.
- Payloads are end-to-end encrypted. The server stores ciphertext.
- A device can hand a summary to another device that is online, and writes it itself when that device is not.

## 1. What it does

Two or more of one person's devices hold the same meetings: titles, transcripts, speakers, people, summaries and folders. Audio stays on the device that recorded it. A device may ask another to write a summary, which comes back as an ordinary synced summary.

Also decided by the owner (2026-11-14, second round):

- The Dictionary and voiceprints can each sync, each by its own switch.
- The server can be a summary device too, on a graphics card or the processor. It is a later addition to the container, designed for now (section 10, "The server as a summary device").

- The summary device on the server is a separate paired worker container, and a vault key on that host is accepted (section 13 item 9, agreed 2026-11-14).

Out of scope for this design: multi-user sharing, audio sync of any kind, syncing settings other than the Dictionary, key rotation, a phone client (the design does not block one).

## 2. Shape

```
 device A  <-- HTTPS -->  earshot-sync-server (container)  <-- HTTPS -->  device B
 full copy                ciphertext rows + presence                     full copy
 merges locally           merges nothing                                 merges locally
```

- Every device keeps a complete local store and works offline. Sync is a background job, never on the path of a recording.
- The server is a relay with storage. It keeps the newest ciphertext per row, hands out rows after a cursor, and keeps a last-seen time per device. It cannot read, merge or validate content.
- All merging happens on the clients, by the rules in section 6.

## 3. Entity coverage

| Entity | Class | Synced | Notes |
|---|---|---|---|
| Meeting | Syncable | yes | gains `summary_kept_local` (section 8) |
| Track | Syncable | yes | metadata only |
| Segment | Syncable | yes | live and final passes, words included |
| Speaker | Syncable | yes | |
| Person | Syncable | yes | `merged_into` handling in section 6.4 |
| Note | Syncable | yes | |
| Summary | Syncable | yes | tombstones carry bodies until the meeting is deleted; they travel encrypted like any row |
| Folder | Syncable | yes | |
| SummaryRequest | Syncable | yes | new, section 10 |
| DictionaryWord | SyncableOptIn | only while the device's Dictionary switch is on | new, section 9 |
| Voiceprint | SyncableOptIn | only when the user turns it on | section 9 |
| AudioFile, audio data, PCM, ExportRecord | NeverSync | never | section 11 |

Device-local records (`local_state`) do not sync. What that means on the other device:

- `speaker-identities/*` (biometric centroids): stays on the device with the audio, which is the only device that can run a final pass. An ignored voice is not shown as ignored elsewhere.
- `word-choices/*`: suggestion marks the user answered on one device show again on the other.
- Settings (writer, style, templates, devices): each device keeps its own. The Dictionary is the exception (section 9).
- `summary-kept-local/*`: the one record that must not stay local. Section 8.

On a device without a meeting's audio, `MeetingDetail.audio_kept` is already false, so re-transcribe and samples are disabled with no new rule.

## 4. Encryption and keys

- One vault key, 32 random bytes from the operating system's generator (not `os_seeded_rng`, which is not cryptographic), made on the first device.
- Each row is sealed with XChaCha20-Poly1305: a random 24-byte nonce, and the row key, the row's `updated_at` and the format number as associated data, so the server cannot move a ciphertext to another row or stamp.
- The plaintext is the existing `SyncEnvelope` (entity, class, id, meta, record) as JSON. `sync_envelope` stays the only way from a domain row to a payload.
- The row key the server sees is `HMAC-SHA256(vault key, entity id)` cut to 16 bytes, so it does not learn creation times from UUIDv7 ids.
- The vault key and the device token live in Windows Credential Manager (`Earshot/sync-key`, `Earshot/sync-token`), reached the way the Anthropic key is: saved, used, removed, never read back to the UI or carried in an IPC response.
- What the server can see: how many rows exist, each row's size and `updated_at`, which device pushed it and when, and when each device was last active. It cannot see titles, text, names, entity kinds or device names.
- Crypto lives in a new adapter crate, `earshot-sync` (RustCrypto `chacha20poly1305`, `hkdf`, `hmac`, `sha2`: MIT or Apache-2.0). Nothing is added to `earshot-core`.

Not in this version: key rotation. A removed device that kept the key could read rows only if it also regained access to the server; its token is revoked. Rotation (a new vault and a re-upload) is a later task.

Losing every device loses the key, and the server's data is then unreadable. Any one surviving device holds everything and can start a new vault.

## 5. Server and API

`earshot-sync-server`: axum and SQLite in one container, one vault per container, behind the owner's reverse proxy for TLS. Tables: `device(token_hash, created, last_seen, presence blob)`, `row(row_key, updated_at, seq, device, ciphertext)`, `pairing(slot, wrapped key, expires)`, `vault(format)`.

| Call | Does |
|---|---|
| `POST /v1/devices` | registers the first device with the container's setup secret (`EARSHOT_SYNC_SETUP_SECRET`), or a later one with a pairing slot; returns a device token |
| `POST /v1/pairing` | an existing device stores the wrapped vault key in a one-use slot that expires after 10 minutes |
| `GET /v1/pairing/<slot>` | the joining device takes the wrapped key; the slot is deleted |
| `POST /v1/rows` | push a page of rows; a row older than or equal to the stored one is answered `superseded` and ignored |
| `GET /v1/rows?after=<seq>` | pull rows with a server sequence above the cursor, in pages |
| `PUT /v1/presence` | store this device's encrypted presence blob and the server's receive time |
| `GET /v1/presence` | every device's blob and its age in seconds, computed by the server |
| `DELETE /v1/devices/<id>` | revoke a device's token |

- Auth is a bearer token per device; the server stores only its hash. OIDC is not in this version.
- Limits: 256 KiB per row, 4 MiB per request. Nothing in the synced set comes near the row limit; audio would.
- The server assigns `seq` when it accepts a row, so a pull cursor never depends on a client clock.
- Tombstones are kept for good, so a device that was away for months still hears about deletions.
- The vault's `format` is the highest row format any device has pushed. A device whose own format is lower refuses to sync and says to update Earshot: an older client would otherwise write a whole row back without the fields it does not know.

### Pairing

1. First device: the user enters the server address and the setup secret. The device registers and makes the vault key.
2. Adding a device: the first device shows a pairing code of 128 random bits (26 characters in groups) and stores the vault key wrapped under a key derived from the code (HKDF) in a pairing slot.
3. The new device takes the server address and the code, fetches the slot, unwraps the key and registers.

The code carries full entropy, so the server cannot guess it from the wrapped key, and no password-stretching or key-exchange dependency is needed. The cost is a longer code, typed once or pasted.

## 6. Client

A `sync` module in `earshot-runtime`, one background thread, all HTTP through `earshot-net` with the sync server's exact host added to the allowlist (https only, loopback http for tests). Every request is in the audit log like any other.

### 6.1 Finding local changes

A device-local `sync_outbox(table, id)` table, filled by SQLite triggers on insert and update of every table in `SYNCED_TABLES` (an appended migration). No writer has to know about sync, and a write cannot be missed. Applying remote rows removes the outbox entries its own writes caused, in the same transaction, so a pulled row is not pushed back.

Turning sync on fills the outbox with every existing row.

Rejected: comparing `updated_at` with a high-water mark. Applied remote rows carry other devices' stamps, so the mark cannot tell local changes from remote ones.

### 6.2 The loop

Push the outbox in pages, pull after the cursor, apply. Run after local writes (debounced) and every 30 s; presence every 60 s. The cursor is the device-local record `sync-cursor`. A meeting with a `Recording` lease is not pushed until `RecordingSaved`, so a live meeting produces no traffic.

Failures never reach a meeting: a server that cannot be reached is a status line on the Sync page and a retry with back-off.

### 6.3 Applying remote rows (write and lock discipline)

- One `Store::transaction` per pulled page, under the services lock, held for the page only. Never across a network call.
- A row is written only when its `updated_at` is later than the local row's (HLC order, device id as the tie-break). Whole-row last writer wins. The local `Hlc` observes every remote stamp.
- A stamp more than 24 h ahead of this device's wall clock is set aside and reported, not applied: one bad clock must not win every later edit.
- Rows arrive in any order. A row whose parent is not here yet (a segment before its meeting) waits in a device-local pending set and is tried again after the next pull.
- Rows of a meeting that has a job lease on this device (recording, processing, a line edit) wait the same way until the lease ends.
- A row that cannot be decrypted or read is counted, shown on the Sync page and skipped; it never stops the page.
- A meeting tombstone runs this device's own deletion: `Services::delete_meeting`'s audio files and local keys, not just the row. Deleting a meeting on one device deletes its audio on the device that holds it.
- A person tombstone runs `forget_identity_biometrics` for that identity here.

Whole-row merging loses one of two edits made to different fields of the same row on two devices inside one sync interval (a title here, a folder there). Accepted for one person's devices.

A synced write to a speaker row reads as a user edit to `speaker-identities` (`given_stamp` differs). That is the wanted reading: the name is kept by the next final pass.

### 6.4 Rules that keep devices converged

Each runs inside the apply transaction and depends only on synced data, so every device reaches the same answer.

- Two "Me" people (each device made its own): the `is_me` person with the earliest `created_at` stays; the others get `merged_into` it. `identity_of` already follows the chain, so each device's own voiceprint still names Me.
- Two live summaries for one meeting (two devices wrote at once): the one with the latest `created_at` stays, the other is tombstoned with its body, which makes it the undo.
- Two live folders with one name: both stay. The user moves meetings and deletes one.
- Other people with the same name on two devices arrive as two people. Merging them is by hand.

## 7. Settings and UI

A Sync page in Settings: server address, pair or add a device, the device list with online state, last sync, rows waiting, rows that could not be read, the voiceprint switch, turn off, remove this device. Turning sync off stops every request and leaves local data alone.

## 8. Kept-local meetings

`summary-kept-local/<meeting>` is device-local today. Once meetings sync, another device would not know a meeting is kept and could send it to a remote writer. So before any sync ships:

- `meeting.summary_kept_local` becomes a synced column (an appended migration, filled from the existing records).
- `keep_local::summary_privacy` reads kept when either the column or the old record says so, and an unreadable value counts as kept.
- The flag is set when the meeting is created, so every copy has it. `allow_writer` clears it. A stale whole-row write can only put it back to kept, which is the safe direction.

## 9. Voiceprints and the Dictionary

Two switches on the Sync page, each per device. A row of a switched-off kind is neither pushed nor applied on that device, and turning a switch on resets the pull cursor so earlier rows of that kind are read.

### Voiceprints

`settings/sync-voiceprints`, off by default (D6: biometric).

- Forgetting a person's voice tombstones and zeroes the rows as now, and the tombstones sync, so a voice forgotten on one device is forgotten on all that sync voiceprints.
- A voiceprint carries its `EmbeddingModelId`; a print from a model this device does not use is stored and skipped when matching, as an unscorable print is today.
- With the Me rule of 6.4, each device's own voice sample ends up on the one Me, so a meeting on either device recognises the user.
- `speaker-identities` centroids and live speaker windows never sync.
- A summary worker (section 10) never applies voiceprint rows, whatever any switch says: it has no use for them.

### Dictionary

`settings/sync-dictionary`, on by default once sync is on (words are not biometric, and the point of the list is that it is the same everywhere). Off keeps this device's own list.

- One synced row per word (`DictionaryWord`), not one row for the list: a word added on each of two devices keeps both, and a removal is a tombstone that syncs.
- `settings/vocabulary` stays the list a device reads at summary time. With the switch on it is rebuilt from the live rows after every apply, and `vocabulary::add_word` / `remove_word` / `set_vocabulary` write rows in the same transaction as the record. Turning the switch on makes a row for each word the device already has.
- Two rows for the same word (case-folded) count once, the earliest-created staying live.
- The limits of `Vocabulary::new` still hold on each device. If the joined list passes them, words are taken in creation order until a limit is reached; the rest stay stored and the Dictionary page says how many are not in use. Nothing is deleted to make room.
- An offloaded summary uses the writing device's list, which is the same list when both sync it.

## 10. Presence and summary offload

### Presence

Each device sends an encrypted blob every 60 s: device name, app version, whether it writes summaries for other devices, the kind of writer it would use (local model, Anthropic, OpenAI-compatible, Claude Code, Codex), and whether it is recording. The server adds its own receive time and returns ages, so device clocks do not matter. Online means an age of at most 150 s (a judgement: two missed beats). On sleep or quit a device sends "gone", best effort.

### Settings

- Helper: "Write summaries for my other devices", off by default.
- Asker: the Summaries page lists each paired device that offers it, as "On <device> (<writer kind>)". The page's existing writer stays and is what runs when the device is not available.

### At summary time

Decided once, where `host::summary_from_settings` runs today:

1. If a device is chosen, and presence says it is online, offers summaries and is not recording: write a `SummaryRequest { meeting, target device, asking device, segment count, state }` and sync. The meeting shows "Waiting for <device>".
2. Otherwise run this device's own writer, with a banner saying the other device was not available. This is a named, shown choice, not a silent fallback.

### The request

- States: `Requested`, `Claimed`, `Done`, `Declined(reason word)`, `Withdrawn`. Only the target writes `Claimed`, `Done` and `Declined`; only the asker writes `Withdrawn`.
- The target claims it, waits until it holds the transcript the request counted (`segment count`), then runs its ordinary summary path with its own writer, style and Dictionary, in its meeting queue behind its own work. The summary row syncs back like any other.
- Not claimed within 90 s (a judgement: one poll each way with room): the asker writes `Withdrawn` and runs its own writer.
- Claimed, then the target's presence goes stale for 5 minutes with no `Done`: the same.
- The target re-reads the request in its summary's write transaction and stores nothing for a withdrawn one. If both still write, rule 6.4 keeps the later summary and the other becomes the undo.
- A kept-local meeting is written by the target's local model only. With none installed it answers `Declined(kept_local_no_model)` and the asker uses its own local model.
- Re-summarise by hand offers "Here" and "On <device>".

The transcript reaches whatever writer the helper uses, which may be a remote one. The label on the asker names the writer kind so the user knows where the text goes, and a kept-local meeting never goes to a remote writer on any device.

### The server as a summary device

A later addition (after GATE-09). The rest of the design leaves room for it: nothing in the protocol assumes a device is a desktop app, and the presence blob carries a device kind.

It cannot be the relay itself. To write a summary a program must read the transcript, so it must hold the vault key, and the relay holding the key would undo section 4. So:

- A second container, `earshot-worker`, joins the vault as an ordinary paired device with its own pairing code and token. The relay container stays unable to read anything. A user who does not want a key on the server does not start the worker.
- The cost, stated plainly: once the worker is paired, whoever controls that host can read every synced meeting. End-to-end encryption then protects the relay's database, its backups and the network path, not the host.
- The worker's key is a Docker secret or a file readable only by the container's user, never an environment variable or an image layer.
- It runs the sync client, sends presence (always online while the container runs, kind "server") and answers `SummaryRequest`s like any helper. It has no UI, records nothing, never applies voiceprints, and holds no audio.
- The model runs in a stock llama.cpp server container beside it, which the worker reaches as an OpenAI-compatible endpoint on the compose network through the existing `earshot-llm` client. The compose file offers two profiles: `cpu` (the plain image) and `gpu` (the CUDA image with the host's NVIDIA runtime). Moving from one to the other is a profile change, with no Earshot build involved.
- An endpoint on the worker's own compose network counts as that device's local model, so the worker may write kept-local meetings. Any other endpoint is remote and a kept-local request is declined.
- The presence label says which it is ("On <server> (local model, processor)" or "graphics card"), read from the profile the worker was started with.
- Prompts, the summary schema, validation and the store transaction are the app's own code paths, so a worker summary is checked exactly like a desktop one.

Rejected for the first version: llama.cpp built into the worker image. It needs its own CPU and CUDA builds of the DLL set for Linux, where the stock server image already has both.

Measured on Linux (T11.5, 2026-11-15): `earshot-store` and the summary path of `earshot-runtime` had only ever been built for Windows, and the store's `cfg(not(windows))` branches had never run. The spike built and tested them on `x86_64-unknown-linux-gnu` (GitHub's `ubuntu-24.04` runner, toolchain 1.99.1, debug builds, `--locked`) with the workflow `.github/workflows/linux-check.yml`, which runs only by hand or on a push to a `linux/**` branch. No source file changed.

| Crate | Builds | Tests |
|---|---|---|
| `earshot-core` (lib) | yes | 655 passed, 1 failed: a dev-wide echo test whose LibriSpeech fixture is not fetched on the runner (the missing-fixture rule, not a Linux fault) |
| `earshot-proc` | yes | 2 passed, 7 ignored (they spawn `cmd.exe`, `ping.exe` or `findstr`), so `run_with` itself is untested on Linux |
| `earshot-net` | yes | 47 passed |
| `earshot-store` | yes | 155 passed, 2 ignored (Windows only: total memory, sharing-mode in-use check). The non-Windows `rename_noreplace` (hard link, then remove) and `sync_dir` ran in the export and audio tests and passed. `total_memory()` is `None` off Windows |
| `earshot-sync` | yes | 11 passed |
| `earshot-llm` | yes | 78 passed, 39 ignored (35 of them the Claude Code and Codex fake-CLI binaries, Windows only) |
| `earshot-sync-server` | yes | 17 passed |
| `earshot-runtime` (lib) | yes, about 2 min 40 s cold, compiling whisper.cpp (`whisper-rs-sys`, static), llama.cpp (`llama-cpp-sys-2`, shared `libllama.so` and `libggml*.so`), `ort-sys` (load-dynamic, no native library) and `earshot-hardware` | filter `summary`: 157 passed, 10 ignored (agent-CLI stand-ins are `cmd.exe`; the local model is not fetched). Filter `sync`: 33 passed. Everything else: 858 passed, 10 failed, 14 ignored; 9 failures need the models folder (`LOCALAPPDATA` unset, as without fetched models on Windows) and 1 asserts hardware facts that `earshot-hardware` reads on Windows only |

Two build notes, both for the workflow, not the crates: `.cargo/config.toml` sets `CMAKE_C_FLAGS_RELEASE` and `CMAKE_CXX_FLAGS_RELEASE` to MSVC flags for every target, so the workflow overrides them with gcc flags (not measured without); and `llama-cpp-sys-2` 0.1.160 hard-links its shared libraries and their `.so` symlinks into `target/debug`, `deps` and `examples` each time its build script runs, then panics (`File exists`) on a dangling link left by an earlier run with other features, so the workflow removes those links between the library build and the test build. Runs: https://github.com/pitslug/Earshot/actions/runs/43692740570 (first complete run, the runtime's test build stopped in `llama-cpp-sys-2`) and https://github.com/pitslug/Earshot/actions/runs/43694560256 (the results above).

Recommendation for T11.6: the worker reuses `earshot-runtime`; it does not get a crate of its own.

- It builds and its summary and sync tests pass on Linux as they are, so reuse costs no port.
- What the worker needs exists once there and AGENTS.md says it must: the sync engine (`earshot_runtime::sync`), the meeting reader (`MeetingView`), `summary::store_summary`'s transaction and conflict checks, `keep_local::summary_privacy` and the handover and request rules. A separate crate over the store, `earshot-sync` and `earshot-llm` would copy all of them, and a worker summary would no longer be checked exactly like a desktop one.
- The cost is weight, not correctness: the worker binary carries whisper.cpp statically and, because `summary.rs` names `earshot_llm_local` directly, is likely to need `libllama.so` and `libggml*.so` beside it in the image even though it never loads a model in process [likely: not yet checked with `ldd` on a built worker]. A cargo feature in the runtime that leaves out the in-process engines would remove that, but it is a refactor of the runtime and should follow a working worker, not precede it.

## 11. Audio exclusion

- Types: only `SyncPermitted` classes implement `SyncSerialize`; the trybuild case stays. The sync client accepts nothing but a `SyncEnvelope`.
- Client: every plaintext is scanned for `fLaC`, `RIFF` and `OggS` and checked against the size limit before it is sealed.
- Server: the per-row limit, and no endpoint that takes anything but row pages, presence blobs and pairing slots.
- `accept m11` scans every plaintext at the client boundary and every server table for oversized blobs. The server cannot scan content it cannot read, so `sync.no_audio`'s wording changes (section 13).

## 12. Testing

- `earshot-sync`: seal and open round trips, a changed row key, stamp or byte is refused, pairing code wrap and unwrap.
- Store: apply rules as table-driven cases (later wins, earlier ignored, parent missing, leased meeting, future stamp, tombstone side effects).
- Runtime: two and three simulated devices against an in-process server; concurrent edits converge (`sync.convergence`); the Me and duplicate-summary rules; a kept-local meeting never reaches a remote writer through a request.
- Offload: fake presence and a fake clock; claim window, stale target, withdrawn then finished, declined.
- Server: `cargo test -p earshot-sync-server`; limits, token checks, slot expiry and single use.

## 13. What GATE-08 approved

Changes outside this document that needed the owner's agreement (AGENTS 6.2). All ten were approved on 2026-11-14 and the PLAN edits were made in the gate's commit. Item 5 was approved with both options open; the one taken is its own check (the server crate is left out of the shipped check and may hold `hyper` only as axum's server).

1. PLAN 3.9 item 2: conversation content may also leave the device as ciphertext to the sync server the user configured, readable only by devices holding the vault key.
2. PLAN 3.9 item 3: "(future) the configured sync server" becomes current.
3. PLAN 4.2 `sync.no_audio`: "no audio types or bytes in any sync plaintext; no server row over the size limit".
4. PLAN section 1 scope: presence and summary offload join the sync feature.
5. Net guard: `earshot-sync-server` pulls in `hyper` through axum. It is not part of the desktop app, so it needs an exemption like xtask's, or its own check that it holds no HTTP client.
6. The three judgements: online at 150 s, claim within 90 s, stale target at 5 minutes.
7. Hosting: one container on the owner's server behind the existing reverse proxy, reached away from home over the owner's VPN.
8. The Dictionary syncs by default once sync is on; voiceprints stay off by default.
9. The summary worker holds the vault key on the server. This is the one place content is readable off the owner's own computers, and it exists only if the owner pairs the worker.
10. An endpoint on the worker's compose network counts as a local model for kept-local meetings.

## 14. Task order

T11.1 (this) -> GATE-08 -> T11.1a kept-local as a synced field -> T11.1b outbox and apply in the store -> T11.2 server -> T11.2a `earshot-sync` crate -> T11.3 client -> T11.3a Sync page and pairing -> T11.3b convergence rules -> T11.3c presence -> T11.3d summary offload -> T11.3e Dictionary sync -> T11.4 acceptance -> GATE-09 -> T11.5 Linux spike for the worker -> T11.6 summary worker.
