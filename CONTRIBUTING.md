# Contributing

Thanks for looking at Earshot. This page is the short version. The working rules are in [AGENTS.md](AGENTS.md), and [PLAN.md](PLAN.md) holds the design and the task list. Both are long because they record what was learned while building: read AGENTS.md sections 1 (ground rules), 3 (working a task), 8 (git conventions) and 10 (definition of done) first, then search section 11 for the part of the code you are changing.

## Before you start

- **A bug:** open an issue saying what you did, what happened and what you expected, with the Earshot version. Never attach a real recording or paste real meeting text.
- **A security problem:** do not open an issue. See [SECURITY.md](SECURITY.md).
- **A change:** open an issue first for anything larger than a small fix, so the design can be agreed before you write it. PLAN.md section 1 says what is in scope.

## Building and checking

There is no hosted Windows build for this repository. The checks run on your own PC:

```powershell
cargo xtask doctor
```

```powershell
cargo xtask fetch-models --set ci
```

```powershell
cargo xtask fetch-fixtures --set smoke,librispeech
```

```powershell
cargo xtask ci
```

`doctor` lists what your machine is missing. `ci` is formatting, clippy with warnings as errors, the tests, the UI build and tests, and the project's own guards (the core purity check, the network guard and others). Run `cargo xtask` from the repo root. In a pull request, say that `cargo xtask ci` passed on your machine and paste its last lines, or say which step you could not run and why.

The only hosted workflow that builds code is `linux-check` (`.github/workflows/linux-check.yml`): the relay, the summary worker and the Linux builds of the crates they use. It runs by hand or on a push to a `linux/**` branch, never on a pull request.

Some checks need hardware or private files that only the maintainer has (the hardware gates in PLAN.md section 5, tests marked `#[ignore]` that name private recordings). Skip those and say so in the pull request.

## Rules that are not negotiable

These come from AGENTS.md section 1 and PLAN.md section 3.9:

- **Privacy.** Audio never leaves the device. No telemetry. No new network destination: all HTTP goes through `earshot-net` and its allowlist, and the `net` step of `ci` fails otherwise.
- **Tests are not weakened to pass.** No loosened thresholds, no deleted or ignored failing tests, no special cases for fixture inputs.
- **The core stays pure.** `earshot-core` has no operating system access: put a trait in core and the implementation in an adapter crate (PLAN.md section 3.2).
- **Licensing.** Earshot's own code is MIT. Do not add a dependency that would put the whole under copyleft terms, and say where any new model, fixture or data file comes from and under what licence.
- **Nothing private in the repo.** No models, large audio, secrets or API keys, and no name, date or phrase taken from a real recording or meeting. Test names and sample text are invented.
- **No em dashes** in Markdown, UI text or commit messages. Use hyphens, colons or parentheses.

## Commits and pull requests

- One change per pull request, with its tests.
- Commit messages follow AGENTS.md section 8: a short imperative subject, a few lines on what changed and why, and the check you ran. If your change has no task id in PLAN.md, say so in the pull request.
- Everything you push is public, commit messages included.
- The pre-push hook in `.githooks` is the maintainer's: it needs a private list that you do not have, so leave it off in your fork. `cargo xtask ci` runs the same guard without that list.

By contributing you agree that your contribution is under the MIT licence in [LICENSE](LICENSE).
