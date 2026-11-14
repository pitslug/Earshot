# Follow-ups
Advisory review findings, one per line. Each line says what it waits on: `[owner]` a decision or a run only the owner can make, `[waits: <task> merge]` a file another host has claimed, `[open]` work nobody has picked up yet (a sweep task takes these; see AGENTS.md conventions). Research and hardware runs that need no decision live in PLAN.md, section "Parked".
- [open] acceptance/results/m2.json records `7bafff9-dirty`: re-run `cargo xtask accept m2` on a clean tree for a clean stamp, on an idle host.
- [open] acceptance/results/bench-*.json: these figures carry a shared-host note and a `-dirty` commit: re-run `cargo xtask bench asr|diar|aec --backend cpu` and `asr --backend vulkan`, no `--note`, on an idle host for a baseline.
- [open] apps/desktop/src-tauri/src/summary_settings.rs: the endpoint model name limit counts UTF-8 bytes while the agent CLI names now count characters (T12.36); aligning it changes what is accepted.
- [open] crates/earshot-runtime/src/live/preview_tests.rs: no test covers far-end words leaking into the mic's own previews in a call under Everyone (T12.36).
- [open] crates/earshot-runtime/src/live/preview.rs `previews_off`: a stop for another reason (fell behind, battery, transcript stopped) still overwrites a crash that `finish` detects in the same moment; T12.33's join catches most (T12.36).
