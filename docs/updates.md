# In-app updates

How a release reaches an installed Earshot (T9.1b, PLAN D24).

## What the app does

Nothing happens until the user presses **Check for updates** in Settings, General, or has turned on "Check for updates when Earshot starts". Then:

1. **Check.** It asks `https://github.com/pitslug/Earshot/releases/latest/download/earshot-update-<arch>.json` for one small file. Only `github.com` and, on a redirect, `release-assets.githubusercontent.com` can be contacted.
2. **Verify.** The file's Ed25519 signature is checked against the public key built into the app before anything in it is read. A release must also be newer than the running version, for this architecture, with an installer name that follows from its version. Nothing in the file can name a host or a path.
3. **Download.** Only when the user presses Download. The installer goes to `%LOCALAPPDATA%\Earshot\updates`, by the same rules as a model download: streaming, resume, size and SHA-256 before the rename.
4. **Install.** Only after the user confirms the restart, and never during a meeting, a meeting job, the model check, the voice sample or the audio check. A model download already running refuses it too; one started in the same moment is cut and resumes later. The file is read and checked again, then started with `/P /UPDATE /R`, and the app exits. The installer replaces the program and starts the new version. Meetings and settings are not touched.

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

4. Publish both files it prints in one normal release (not a pre-release) tagged `v<version>`, so it takes the Latest mark, which is what the app asks for.

`sign-update` refuses a key that does not match the one built into the app, because installed copies would refuse the file. A later release with no version file makes a check say no update information is available.
