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

## Results (2026-11-09): no candidate passes, nothing changes

### Candidate 1: isolated echo pairs (final pass only)

Designed before measuring; the constants are judgements, not tuned. Two adjacent mic words repeating two adjacent reference words in order were echo only when all of these held:

- each word's offset within 300 ms of the stretch's lock delay (the run rule allows 1 s);
- no mic word the run rule kept within 1.5 s before or after the pair (the pair is all the mic heard there);
- the far end saying another word starting within 1 s before the first repeated word and within 1 s after the second (the pair lies wholly inside far-end talk, never at its start or end, where a reply that repeats it would sit);
- inside a cleaning stretch, with a delay lock (as the run rule).

It ran in the final pass's text rule only (`text_echo_mask`) and in `asr_eval`; the live filter kept the 3-word minimum, because it does not know the mic words after a final. Its unit tests, the negative control (a 2-word near-end reply during far-end speech that does not repeat the far end is kept), and every existing echo, dedupe and post test passed.

Dev runs (`asr_eval --metric asr.echo_leak --leak-dev [--rumble acceptance/aec_rumble_dev.toml] --backend cpu`, base.en, release, both builds from commit 376e019, the only difference the candidate), aec condition, margin 0.5:

| run | leak before | leak after | near-end kept in leak scenarios (double talk) before / after | near-end kept overall before / after |
|---|---|---|---|---|
| leak-dev | 18 / 1145 (1.57%) | 18 / 1145 (1.57%) | 112 of 112 -> 112 of 112 | 246 of 246 -> 246 of 246 |
| leak-dev rumble | 21 / 1145 (1.83%) | 21 / 1145 (1.83%) | 84 of 84 -> 84 of 84 | 221 of 221 -> 221 of 221 |

The raw-mic condition (informational) was also unchanged: 110 / 1145 and 119 / 1145 leaked. The candidate removed no word on either run: point 2 holds, point 1 fails (leak held, not lower on either run), so it is not kept and the code was reverted.

### Why no second candidate was tried

On both dev runs the leaked far-end words in the aec condition are single words inside near-end talk, not repeated pairs. Ignoring timing altogether, the only kept mic bigrams that also occur in the same scenario's reference transcript are function-word pairs (3 distinct bigrams across both runs, such as "of the"), and they sit inside the near-end speaker's own sentences. Any looser pair rule could therefore only remove near-end words on dev, never lower the leak. The dev split holds no instance of the shape found at GATE-02, so it cannot show a short-run rule earning its place; a rule for that shape would need dev scenarios that produce it (an isolated short echo fragment in far-only speech), defined before measuring.

## Round 2: isolated echo islands, judged after the speaker check (written and committed 2026-11-10, before this rule was measured)

What was seen before writing this: the unchanged dev run's numbers and kept text (`--leak-dev`; the rumble run not yet). In the aec condition the kept mic words of far-only dev scenarios are a few short pieces, most of them garbled or not repeating the far end. No measurement of this rule existed when this was committed.

Candidate 1 judged isolation against the words the run rule kept, before the speaker check had removed anything, so echo the speaker check would remove could shield a pair. It also needed the offset within 300 ms. This round tests whether that is why it never fired.

### The rule

It runs in the final pass only, on the mic words the run rule and the speaker check left, once per cleaning stretch with that stretch's delay, and only on words whose start that stretch covers; no lock, nothing removed. The live filter keeps the 3-word minimum, because it does not know the mic words after a final.

1. **Island:** a maximal run of those mic words in which each word starts at most 1.5 s after the previous word ends.
2. **Short:** the island holds at most 3 words.
3. **Repeats the far end:** a chain of at least 2 words repeats reference words in order. Each pair has identical normalised text, an offset within the tolerance of the delay, a reference index at most 2 past the previous pair's, and an offset within 500 ms of the previous pair's. The chain starts at the island's first word and ends at its last, so only a garbled word between repeats goes with it, never a word beside them.
4. **Inside far-end talk:** the far end starts another word at most 1 s before the chain's first reference word and at most 1 s after its last.

A qualifying island is removed whole and counted in `EchoReport::text_removed`. Single words are never removed. The constants are judgements fixed here, except the tolerance.

### Tolerance sweep

300, 500 and 1000 ms, on both dev runs. The chosen value is the one with the lowest summed aec leak over both runs among those meeting point 2 below; ties go to the smaller value. If none lowers the aec leak, no value is chosen and the rule fails.

### Pass criteria

The decision rule above stands unchanged, in full:

1. The aec leak is no higher than the unchanged code's on both dev runs (18 / 1145 and 21 / 1145 when last measured; this round's own baseline run is the comparison), and **lower on at least one. This is the only thing that counts as a gain.** A removal in the raw condition, or a removal that does not lower the aec leak, is reported but is not a gain.
2. Near-end recall is no lower on both dev runs, pooled and in double talk alone, in the aec condition: no near-end word heard may be lost. The raw condition is reported beside it.
3. `cargo xtask accept m2` passes, with `asr.echo_leak` at or below its last value (0.36%) and both `ECHO_NEAR_RECALL_FLOOR_PERCENT` checks at 100%.
4. Every existing echo, dedupe and post test passes unchanged; new tests cover the rule and the negative control.

### On failure

The code is reverted, the numbers and the reason go here, and T12.11's result is no change. No constant is moved after seeing a result to make a point pass, and the dev set is not changed within this round. A new shape or a new dev set would be another round, with its own rule written first. `fixtures/private/` and the acceptance scenarios choose nothing.
