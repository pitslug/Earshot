# Short echo fragments under "Me" (T12.11)

## What was found

At GATE-02 (2026-11-08) a call in which the user said nothing kept one 2-word mic segment under "Me" in the final transcript: 0.7 s long, wholly inside far-end speech. Both words also appear in the far-end transcript 0.27 s earlier (counts and offsets only were read; no text is kept). `asr::dedupe_echo` removes only runs of 3 or more in-order identical words at the canceller's delay, by design: shorter runs risk deleting what the user really said.

## The decision rule (written and committed 2026-11-09, before any candidate was designed or measured)

A candidate rule for shorter echo runs is designed and tuned only on the development split (`asr_eval --leak-dev`, and `--leak-dev --rumble acceptance/aec_rumble_dev.toml`). It is kept only if all of these hold:

1. **Leak falls or holds on dev:** the aec-condition leak (far-end words in the near-end transcript after cleaning) is no higher than today's on both dev runs, and lower on at least one.
2. **No near-end word is lost on dev:** near-end recall after cleaning, pooled and in double talk alone, is no lower than today's on both dev runs (today 100%: no word may be lost).
3. **Acceptance holds:** `cargo xtask accept m2` passes, with `asr.echo_leak` at or below today's 0.36% and both near-end recall checks at 100%.
4. **Existing tests stand:** every existing echo, dedupe and post test passes unchanged; new tests cover the new rule and a negative control (a near-end 2-word reply during far-end speech that does not repeat the far end is kept).

The private recording that found this is never used to choose or tune anything. If no candidate passes, nothing changes and the measurements go here.
