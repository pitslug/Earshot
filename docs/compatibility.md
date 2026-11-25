# Compatibility: what an update changes

This page says how Earshot's versions work together today: the app, its data, sync, the relay and the worker. It describes what the code does. It is not a promise about future releases, and where a rule is only a habit so far it says so.

The short version:

- Updating the app is one-way. A newer Earshot may change the database, and an older one then cannot open it.
- With sync, keep every part on the same Earshot version. Update the relay and your computers first and the summary worker last, and pin the version in the compose files so a container does not jump ahead by itself.

## 1. The update file

What it is: `earshot-update-x64.json` or `earshot-update-arm64.json` on this project's latest GitHub release, the one small file an update check asks for ([updates.md](updates.md)).

- It is signed with a release key of the project's (Ed25519). The app checks the signature against a key built into it before it reads anything in the file.
- It must name a version newer than the one running, for this PC's architecture. So an update never moves a copy backwards.
- Installed copies up to 0.4.2 read the file strictly: the six fields they know (`product`, `arch`, `version`, `installer`, `size`, `sha256`) and nothing else. A file with any other field is refused by those copies as malformed. So the file keeps exactly that shape for as long as those copies are expected to update.
- Copies from the release after 0.4.2 are built to ignore fields they do not know (PLAN T14.34), so that the file can grow later without stranding them. Every field they do know is still checked, and the signature still covers the file's exact bytes.
- A copy trusts only the keys it was built with. Copies up to 0.4.2 hold one. From the release after 0.4.2 the built-in list has a second place, for a backup key kept offline (PLAN T14.34); until that key exists the place is empty and verifies nothing. If a key is ever replaced, copies that hold only the old one cannot verify later releases and have to be updated by hand from the Releases page ([updates.md](updates.md), "If the key is lost").
- A release with no update file makes a check say that no update information is available. Nothing else breaks.

## 2. The database

`earshot.db` in `%LOCALAPPDATA%\Earshot` has a schema number (`SCHEMA_VERSION` in `crates/earshot-store/src/db.rs`). 0.4.2 is schema 9. The code after 0.4.2 is schema 11.

- **Upgrading.** The first start of a newer Earshot applies each missing step in its own transaction, in order. Steps are only ever added, never edited.
- **No downgrade.** An older Earshot that finds a newer schema does not touch the database. It shows "This data was saved by a newer version of Earshot. Install the latest version to open it." Checking for an update by hand still works from there.
- **No automatic copy.** Earshot does not copy the database before it upgrades. This is deliberate: a copy kept by the app would still hold meetings you delete afterwards, which would break what [PRIVACY.md](../PRIVACY.md) says about deleting. If you want a way back, quit Earshot and copy `earshot.db` (with any `earshot.db-wal` and `earshot.db-shm` beside it) yourself before updating, and delete the copy when you no longer need it. README.md, "Where your data is", has the details.
- If a step fails, that step's transaction is rolled back and the app shows that its data folder could not be opened. Earlier steps stay applied.

## 3. Stored settings

Settings live in the same database, per computer. They do not sync, except the Dictionary.

- A stored value that this version does not know is not deleted. The setting's page says a stored value is not known, and the default is used until you choose again.
- Three secrets are outside the database, in Windows Credential Manager: `Earshot/anthropic-api-key`, `Earshot/sync-key` and `Earshot/sync-token`. Updating and uninstalling leave them alone.
- The theme choice is kept by the app's window (its browser engine's storage), not in the database.

## 4. Sync between versions

Every synced row carries a format number (`ENVELOPE_FORMAT` in `crates/earshot-sync/src/seal.rs`). It is 2 in 0.4.0 and 0.4.2, and 4 in the code after 0.4.2. It rises when a release changes what a synced row holds in a way an older build would mishandle.

The rule, enforced by the relay and the app ([sync-spec.md](sync-spec.md) section 3, "Format"):

- The relay remembers the highest format any device has pushed to it. That number only rises.
- A device whose own format is lower is refused on its next push or pull, and its Sync page says "Another device uses a newer Earshot. Update Earshot on this computer to keep syncing." It keeps working on its own; it just stops syncing until it is updated. Nothing is lost: what it could not send goes when it is updated.
- A newer device still reads rows written at every older format.

What that means in practice:

- **The first updated computer raises the format for everyone.** As soon as one computer on the new version syncs, every computer and worker still on the old version stops syncing. Update all your computers close together.
- **Update the worker last.** The worker is a sync device like any other. If its image moves to a new version before your computers do, its first push raises the format and every one of your computers stops syncing until it is updated. If it is the one left behind instead, only the worker stops (its health check says `unhealthy: stopped: update_needed`) until you pull its new image, and nothing in its volume needs changing.
- So far the format has risen three times: once in 0.4.0 and twice in the code after 0.4.2. Expect a release that touches sync to need every device updated. CHANGELOG.md says which releases do.

## 5. The relay and the worker

Both are published with each Earshot release under the same version number: `ghcr.io/pitslug/earshot-server:<version>` and `ghcr.io/pitslug/earshot-worker:<version>`, and also as `latest`. The compose files are attached to the release.

- **Pin the version.** Set `EARSHOT_VERSION` in each compose file's `.env` to the version your computers run. Left out, the compose files fall back to `latest`, and a container that is recreated then takes whatever is newest. For the worker that is the failure described above.
- **The relay does not open rows**, so it has no format of its own: it stores sealed rows and the highest format number pushed. An older relay accepts rows from newer computers.
- **Newer computers can need a newer relay**, because the app may use a relay call that an older relay does not have (removing another computer from the Sync page arrived in 0.3.9, for example). The combination the project's own sync test runs is every part built from the same commit, so the same version everywhere is the one known to work.
- **A sensible order for an update:** the relay, then every computer, then the worker.
- **Image names.** The relay's image was `earshot-sync-server` up to 0.3.11 and is `earshot-server` from 0.4.0. The release workflow also pushed 0.4.0 and 0.4.2 under the old name; later releases are pushed as `earshot-server` only, so a compose file that still names the old image stays on 0.4.2 until it is edited. The relay's data volume is still called `earshot-sync-data`: keep that name, or the relay starts empty.
- **The relay's port.** Up to 0.4.2 the relay's compose file published port 8787 on every interface of its host. It now publishes it on the host's loopback address only, because the relay speaks plain HTTP and is meant to sit behind a reverse proxy. A proxy that is not on that host itself needs one of the other ways the compose file's comments describe.
- **The relay's data** is in its volume, so replacing the image keeps it. Restoring that volume from an older backup is not something your computers detect today: rows they pushed after the backup was taken may not be sent again. Avoid it, or set sync up again afterwards.
- **Pairing across versions.** Pairing itself does not look at versions. A device on an older format than the vault's can join and is then refused at its first sync, as in section 4, so update it first.

## 6. What is not covered

- No release has been tested as an upgrade from every earlier one. The in-app updater always installs the latest release, and the database steps apply in order from any older schema.
- There is no way to lower a vault's format or to change its key. Starting again means a new relay volume and setting up sync again on each computer.
- Android and other clients do not exist yet; nothing here speaks for them.
