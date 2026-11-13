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

## Round 2 results (2026-11-10): the rule removes nothing, nothing changes

The rule (`asr::echo_fragment_mask`, with unit tests for each condition and the negative control, all passing) ran in `asr_eval` on the words the run rule and the speaker check left, per scenario with its lock; the speaker-check numbers are the unchanged production path. Runs: `asr_eval --metric asr.echo_leak --leak-dev [--rumble acceptance/aec_rumble_dev.toml] --backend cpu`, base.en, release, built from 828f868 plus the rule. It was not wired into `post::clean_mic_echo`, because nothing earned it a place.

| run | condition | leak, unchanged | leak at 300 / 500 / 1000 ms | near-end kept in leak scenarios | near-end kept in near-only | words removed |
|---|---|---|---|---|---|---|
| leak-dev | aec | 18 / 1145 (1.57%) | 18 / 18 / 18 | 112 of 112 at every tolerance | 134 of 134 | 0 / 0 / 0 |
| leak-dev | raw | 110 / 1145 (9.61%) | 110 / 110 / 110 | 63 (94.0% of heard) | 137 | 0 / 0 / 0 |
| leak-dev rumble | aec | 21 / 1145 (1.83%) | 21 / 21 / 21 | 84 of 84 | 137 of 137 | 0 / 0 / 0 |
| leak-dev rumble | raw | 119 / 1145 (10.39%) | 119 / 119 / 119 | 59 (98.3% of heard) | 129 | 0 / 0 / 0 |

Point 2 holds (nothing removed, nothing lost); point 1 fails (leak held at every tolerance on both runs, lower on neither). No tolerance is chosen. The code was reverted; `accept m2` was not run, because no code changed.

### Why: what the cleaning leaves in far-only speech is not repeated text

An island count of the words left after cleaning, in scenarios with a lock (islands as defined above; "repeated" means the same normalised word on the reference within +-1 s of the delay, wider than any tolerance swept):

| run | condition | far-only: words left | islands of 1 / 2 / 3 words | islands with 1+ repeated word | with 2+ |
|---|---|---|---|---|---|
| leak-dev | aec | 9 | 3 / 3 / 0 | 1 (a single word) | 0 |
| leak-dev rumble | aec | 14 | 8 / 3 / 0 | 0 | 0 |
| leak-dev | raw | 29 | 3 / 0 / 1 (+ 4 longer) | 3 | 1 (in an island of 6) |
| leak-dev rumble | raw | 31 | 1 / 3 / 2 (+ 2 longer) | 2 | 1 (in an island of 14) |

After the canceller, the short pieces left in far-only speech are garbled residue: none of them holds two words that repeat the far end, and only one holds even one. Almost none of them is a counted leak either: of the 18 leaked words on leak-dev, 16 are in double talk and 2 are two separate one-word islands in `delay-450ms` far-only speech (one word each, which no rule may remove); all 21 on the rumble run are in double talk, as single far-end words inside near-end talk. Even in the raw condition, where the mic hears the echo at full level, no short island repeats two far-end words.

### What a rule of this kind could catch

On these dev runs, nothing: the leak this kind of text rule targets (2 in-order repeats, isolated, inside far-end talk) does not occur after the canceller, and every leaked word on dev is either inside near-end talk or a lone word. For a longer stretch of garbled far-end speech under "Me", a text rule of any run length catches only the words the recogniser reproduces exactly and in order; on dev the garbled residue repeats almost none (1 word in 23 across both aec runs). Such residue is the speaker check's and the echo-cluster rules' job (`diar::speaker_echo_spans`, `drop_echo_clusters` rules 2 and 3), which judge the voice and level, not the words. The GATE-02 fragment matched the far end word for word, which the dev simulator does not produce; finding out whether a text rule earns its place for that shape needs dev scenarios that produce it, defined before measuring, as round 1 already said.

## The Surface mic with and without Windows' own processing (T12.32)

The question: is the echo residue on the Surface's built-in mic there because Windows or the device has already processed the mic stream (noise suppression, perhaps its own echo control), which a linear canceller cannot model? `earshot-cli record-raw --mic-raw` opens the mic in raw mode (`AUDCLNT_STREAMOPTIONS_RAW`); without the flag the stream is opened as the app opens it. Measurement only: nothing shipped changes here, and the result goes to the owner.

### The rule (written and committed 2026-11-14, before any take was recorded)

Setup:

- The Surface's built-in mic and speakers, on mains, the lid and the machine where the owner uses it, the speaker volume as the owner set it and not touched between takes.
- The far end is public speech: the first 60 s of the bundled benchmark meeting's reference track (AMI ES2004a, CC BY 4.0), played on the default output. Each take is 70 s of `earshot-cli record-raw`: 5 s with nothing playing, the 60 s of speech, 5 s with nothing playing.
- Four takes: far end only (nobody talking in the room) and talk-over (the owner reads a fixed public text, the `fixtures/smoke/transcripts.txt` lines of speaker 121, starting about 10 s into the playback), each recorded once as the app opens the mic ("default") and once raw. A refused raw open ends the task with the refusal as the result.
- The takes are private recordings: they stay under `fixtures/private/`, and only counts, levels and times are written here.

Measured per take:

1. **Mic floor**: the level of the recorded mic over the first 4 s (nothing playing, nobody talking), as the 10th and 50th percentile of 100 ms RMS windows, in dBFS.
2. **Echo at the mic**: the median level of the recorded mic while the far end plays, and its difference from the reference's median level.
3. **Canceller, committed tool**: `aec_eval --private <take>`: median segmental ERLE and the final delay.
4. **Canceller over time**: from `earshot-cli replay --timeline` of the take (the production pipeline on the take's real device stamps): the time of the first echo lock, the number of lock stretches and the share of the 60 s of playback inside one; and ERLE above 100 Hz per 10 s of playback, the recorded mic's power over the cleaned mic's (the replay's stored FLAC), both high-passed at 100 Hz, over the 100 ms windows where the reference plays. The levels in 1, 2 and this ERLE come from a scratch script that prints levels only; they are not reproducible from a committed tool and are labelled so in the result.
5. **Mic words**: from the same replay, the mic words the live echo filter removed and the mic words stored after the final pass (counts only). On the far-end-only takes every stored mic word is echo.
6. **Talk-over**: of the words the owner read, how many are in the stored mic transcript after Whisper normalisation (in order, counted once each), and how many stored mic words are not from the text.

What counts as raw helping (judgements, not derived): on the far-end-only takes raw leaves at most half the stored mic words the default take leaves and at least 3 fewer, its median ERLE above 100 Hz is at least 3 dB higher, and on the talk-over takes raw keeps at least as many of the read words as the default take less 2. Raw hurting is the mirror image of the first two, or 3 or more read words fewer. Anything between is "no clear difference".

One take per condition cannot separate a real difference from take-to-take variation, so a "helps" or "hurts" verdict here is a reason for the owner to choose a next step (raw by default, raw on listed devices, a setting, or no change, each needing its own task), never a default by itself. A measurement that contradicts this rule is reported as it is; the rule is not changed after measuring.

### Result (2026-11-14): raw helps with the far end alone and hurts in talk-over

The rule above unchanged. One take per condition, recorded back to back on mains with the volume untouched; the raw open was accepted. Canceller state and word counts come from `earshot-cli replay --timeline` of each take (transcription on the NPU) and `aec_eval --private`; the levels and the ERLE above 100 Hz come from a scratch script (levels only, not reproducible from a committed tool). The 5 s status lines are the replay's own, 12 per take from 11 s to 66 s.

Far end only (nobody talking):

| | default | raw |
|---|---|---|
| mic floor, first 4 s (p10 / median, dBFS) | -52.3 / -49.3 | -46.0 / -44.9 |
| mic while the far end plays (median dBFS; reference minus mic) | -43.7; 15.8 dB | -35.6; 7.8 dB |
| canceller delay | 61 ms | 21 ms |
| status lines locked / converged / bypass / double talk (of 12) | 11 / 2 / 4 / 6 | 11 / 11 / 1 / 7 |
| lock stretches; first status line locked | 1; 16 s | 1; 16 s |
| ERLE above 100 Hz per 10 s of the take (dB) | 4.5, 7.7, 4.2, 2.1, 10.2, 5.8 | 13.2, 24.6, 24.9, 29.4, 10.3, 8.7 |
| ERLE above 100 Hz, median over windows | 5.0 dB | 32.2 dB |
| `aec_eval` median segmental ERLE (whole take, full band) | 9.8 dB | 8.3 dB |
| mic words removed by the live echo filter | 14 | 0 |
| mic words stored after the final pass (all echo) | 11 | 0 |

Talk-over (53 read words):

| | default | raw |
|---|---|---|
| mic floor, first 4 s (p10 / median, dBFS) | -52.9 / -50.2 | -44.7 / -43.6 |
| canceller delay | 61 ms | 21 ms |
| status lines locked / converged / bypass / double talk (of 12) | 9 / 4 / 5 / 4 | 10 / 7 / 2 / 6 |
| lock stretches | 2 | 2 |
| mic words removed (live filter and final pass) | 0 | 0 |
| mic words stored | 53 | 52 |
| read words kept, in order | 46 | 40 |
| stored mic words not from the text | 7 | 12 |

Under the rule:

- **Far end only: raw helps.** 0 stored mic words against 11 (at most half and at least 3 fewer) and 27 dB more median ERLE above 100 Hz (at least 3 dB).
- **Talk-over: raw hurts.** 6 fewer read words kept (3 or more), and 5 more stored words that are not from the text.
- So the verdict is neither "helps" nor "no clear difference": the two halves point opposite ways, and raw is not a default on this evidence.

What the numbers say beside the verdict (not a decision):

- The default stream is processed before the app sees it: the echo arrives 8 dB lower and 40 ms later than on the raw stream, the floor is about 5 dB lower, and the canceller converged on 2 of 12 status lines against 11 of 12 on raw. That fits the T12.11 finding that the residue is not something a linear canceller can model. It is the cause of the echo words under "Me" with the far end alone.
- The whole-take full-band ERLE does not show it (9.8 against 8.3 dB): it includes the 10 s with nothing playing and the raw mic's higher floor. Full-band ERLE hiding a working canceller is the rumble caveat already recorded for real rooms.
- On the raw far-end-only take the ERLE fell to about 10 dB over the last 15 s of playback and the canceller called double talk on 7 of 12 status lines with nobody talking. Not explained here; no word came through.
- In talk-over the raw mic lost read words. Two candidate reasons, not separated by these takes: the raw mic's own-voice level and noise are what Windows' processing would have cleaned for the recogniser, and the canceller in double talk cuts near-end speech (the known cost measured on `--leak-dev`). The readings were also two separate performances by one reader, so a difference of 6 words in 53 is within what a second pair of takes could reverse.

Reproduce (the takes are private; from the repo root on the Surface): `earshot-cli record-raw --seconds 70 --out <take> [--mic-raw]` while the speech plays, then `earshot-cli replay --mic <take>\mic.wav --ref <take>\loopback.wav --timeline <take>\timestamps.json --out <dir> --no-summary` and `cargo run --release -p earshot-core --example aec_eval -- . --private <take>`.

**The owner's reading (2026-11-14, after the measurement, so not part of the rule):** people in a meeting mostly take turns, so talking over the far end is the rare case and the far end alone is the common one. On that weighting the far-end-only half matters most, and the half these takes did not measure is the other common case: the owner talking with the far end silent, default against raw (does the recogniser lose words without Windows' processing when there is no echo to cancel?). That take pair, under a rule written before it, is what a decision on raw needs next.
