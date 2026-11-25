# In-app updates

How a release reaches an installed Earshot (T9.1b, PLAN D24).

## What the app does

Nothing happens until the user presses **Check for updates** in Settings, General, or has turned on "Check for updates automatically". That switch is the one consent: with it on, Earshot checks when it starts and then on the schedule picked under it (every hour, 4 hours, 12 hours, day or week; every day by default), counted from the last finished check, which is kept across restarts (T12.22). A computer that slept through several intervals checks once on waking, not once per interval. With it off, the schedule asks nothing. An automatic check that finds a new version shows it on the General page and, when the window is not in front, one notification per version. Then:

1. **Check.** It asks `https://github.com/pitslug/Earshot/releases/latest/download/earshot-update-<arch>.json` for one small file. Only `github.com` and, on a redirect, `release-assets.githubusercontent.com` can be contacted.
2. **Verify.** The file's Ed25519 signature is checked against the public key built into the app before anything in it is read. A release must also be newer than the running version, for this architecture, with an installer name that follows from its version. Nothing in the file can name a host or a path.
3. **Download.** Only when the user presses Download. The installer goes to `%LOCALAPPDATA%\Earshot\updates`, by the same rules as a model download: streaming, resume, size and SHA-256 before the rename.
4. **Install.** Only after the user confirms the restart, and never during a meeting, a meeting job, the model check, the voice sample or the audio check. A model download already running refuses it too; one started in the same moment is cut and resumes later. The installer is opened shared for reading only, so while Earshot holds it no program can write, replace, rename or delete that file; an installer that is itself a link, an `updates` folder that is a link, or a path that does not resolve to the installer's own name directly inside that folder is refused. The file is read and checked again through that handle, then started with `/P /UPDATE /R` by its path before the handle is let go. A link above the `updates` folder (a redirected profile, say) is followed, not refused, and the start looks the path up again, so a program able to re-point such a link in that moment could still swap what starts: the hold protects the checked file itself, not every folder above it. Then the app exits. The installer replaces the program and starts the new version. Meetings and settings are not touched.

A failed or refused step leaves the installed app and the data as they were. The next start removes any leftover installer.

## The release key

- **Public half:** `RELEASE_PUBLIC_KEY` in `crates/earshot-update/src/lib.rs`, built into every copy.
- **Private half:** a file the owner keeps outside this repo, with a backup somewhere safe. Never commit it. `cargo xtask` refuses a path inside the repo.

Make it once, from your own shell:

```powershell
cargo xtask update-key --out "$env:USERPROFILE\.earshot-private\update-signing.key"
```

### If the key is lost

Installed copies can no longer take an update, because nothing new can be signed for them. Make a new key, put its public half in `RELEASE_PUBLIC_KEY`, publish a release, and tell people to install that one by hand from the Releases page. From then on, updates work again. Do the same if the key may have leaked, and say so in the release notes, since anyone holding the old key could sign a file for old copies.

### The backup key

Copies from 1.0 on accept a version file signed by any key in `RELEASE_KEYS` (`crates/earshot-update/src/lib.rs`). Today the list holds only the release key. Its second place is for an offline backup key, so that losing the release key strands only copies built before the backup's public half went in. Copies up to 0.4.2 know only the release key.

To make it, from your own shell, on a machine or drive kept offline if you can. This is the same command as the release key, with a different file:

```powershell
cargo xtask update-key --out "<offline drive>\earshot-backup-signing.key"
```

It prints the public half in hex. Then:

1. Add that public half as the second entry of `RELEASE_KEYS`, and its hex to the pinned list in the test `update_the_release_keys_are_pinned`, in one commit. Every copy built from then on accepts it.
2. Keep the private half off this computer and out of the repo, with its own copy somewhere safe. Never put it in `.earshot-private` beside the release key: one loss should not take both.

`sign-update` still signs only with the release key (`RELEASE_PUBLIC_KEY`), and refuses any other key. Signing with the backup key needs a change to `sign-update` in the release that first uses it.

### The version file can grow

Copies from 1.0 on ignore a field they do not know, in the signed part and beside it, and still check every field they do know. Copies up to 0.4.2 refuse such a file. `sign-update` writes exactly the fields 0.4.2 reads, so do not add a field while 0.4.x copies should still update.

### What a signature does not cover

A version file has no date. Someone who controls what GitHub serves for this repo could re-serve an older signed file, and a copy older than that release would install it. It can never move a copy backwards, and it cannot install anything unsigned (TODO.md).

## Publishing a release

1. The first time, make the key and put its public half in `RELEASE_PUBLIC_KEY` (above), or the build cannot update. Then bump `version` in the root `Cargo.toml`.
2. Run the checks and build:

```powershell
cargo xtask ci
```

```powershell
cargo xtask build --bundle
```

```powershell
cargo xtask bundle-check
```

3. Sign it, from a shell that can read the key:

```powershell
cargo xtask sign-update --key "$env:USERPROFILE\.earshot-private\update-signing.key"
```

   The ARM64 installer is built and signed here but checked on the Surface: see [arm64-release.md](arm64-release.md).

4. Publish both files it prints in one normal release (not a pre-release) tagged `v<version>`, so it takes the Latest mark, which is what the app asks for.
5. Attach the two compose files to the same release: the relay's as `docker-compose.yml` and the summary worker's as `earshot-worker-compose.yml` (release files need different names). Pushing the tag builds and pushes both images, `earshot-server` and `earshot-worker`, under the version and as `latest` (the `relay-image` workflow); neither is pushed until both started on the runner and answered their checks. The relay's old image name, `earshot-sync-server`, is no longer pushed (T14.4). Say in the release notes when a release raises the sync row format, so people update their computers together and the worker last ([compatibility.md](compatibility.md)).

```powershell
Copy-Item crates\earshot-worker\docker-compose.yml target\earshot-worker-compose.yml
```

```powershell
gh release upload "v<version>" crates\earshot-sync-server\docker-compose.yml target\earshot-worker-compose.yml
```

`sign-update` refuses a key that does not match the one built into the app, because installed copies would refuse the file. A later release with no version file makes a check say no update information is available.
