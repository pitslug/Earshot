# Security

Earshot records conversations, updates itself and can sync through a server you run, so a security problem in it matters. Thank you for telling the project about one privately first.

## Reporting a problem

Use GitHub's private vulnerability reporting on this repository:

1. Open the repository's **Security** tab: <https://github.com/pitslug/Earshot/security>.
2. Choose **Report a vulnerability** and fill in the form.

That report is visible only to you and the maintainers. Please do not open a public issue for a security problem, and do not post details anywhere public until a fix is released.

There is no email address for reports. If the form is not available to you, open a public issue that says only "I have a security report" with no details, and wait for a maintainer to open a private channel.

## What helps

- The Earshot version (Settings, General shows it) and whether the PC is x64 or ARM64.
- What you did, what happened and what you expected.
- For the relay or the summary worker: the image version you run.

Never include real meeting content, recordings, your sync key, a pairing code, a setup secret or an API key. An invented example is enough.

## What is in scope

- The Windows app and its installer.
- The in-app updater: the signed version file, the download and the install step ([docs/updates.md](docs/updates.md)).
- Sync: the encryption, the relay (`earshot-sync-server`) and the summary worker (`earshot-worker`) ([docs/sync.md](docs/sync.md), [docs/sync-spec.md](docs/sync-spec.md)).
- Anything that makes conversation content leave a computer other than as [PRIVACY.md](PRIVACY.md) describes.

## Known limits, not vulnerabilities

These are how Earshot is built today, and PRIVACY.md and README.md say so:

- The data folder is not encrypted. Anyone with access to your Windows account can read it.
- The installers are not code-signed with a certificate. In-app updates are signed with the project's own key.
- A computer you remove from sync keeps the sync key it already holds. There is no key change.
- Once a summary worker is paired, whoever controls its host can read every synced meeting.
- The relay speaks plain HTTP and expects a reverse proxy in front of it for TLS.

A way to make any of these worse than described is still worth a report.

## Which versions get fixes

Only the latest release. Earshot's own updater installs the latest release, and the relay and worker images are published with it under the same version. See [docs/compatibility.md](docs/compatibility.md) for what updating involves.
