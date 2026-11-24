# A sentence's edge word stays with its speaker (T12.88)

Found (the owner, two in-room meetings on 0.4.0): the last word of one person's sentence often becomes the first word of the next person's line (invented example: "Speaker 2: Friday. I think we can..." where "Friday." ended the previous speaker's sentence). The other direction is rare.

Diagnosis (read from a private copy of the second meeting; its numbers stay private): Parakeet emits `.`, `,`, `?` and `!` as tokens of their own (ids 839, 841, 854 and 885 of `parakeet-tdt-0.6b-v2`'s `tokens.txt`), usually a moment after the word's last sound, and `asr::words_from_tokens` sets a word's end from its last token, punctuation included. A word that closes a sentence or clause therefore ends inside the pause, where the next speaker's turn starts. `diar::assign` gives each word to the turn that overlaps it most, so a short final word whose end has been stretched can overlap the next turn more than its own. On that meeting short words with punctuation lasted more than twice as long as the same-length words without; `edge_words` reports the same comparison on the public data below (`durations`). Word starts are accurate (60 ms median error on AMI, `docs/model-eval/cuda.json`), which fits errors going in one direction only. T6.9a (`word-changes.md`) never saw this: its AMI words were the reference words with their hand-aligned times.

## Stage 1 rule (written before any run, 2026-11-24)

**Candidate.** In `asr::words_from_tokens`, a token that continues a word and consists only of ASCII punctuation adds its text to the word but leaves the word's end where it was (the end of the last token that is not punctuation). Starts, text and probabilities are unchanged. Nothing else changes; `diar::assign` stays as it is.

**Tool.** `cargo run --release -p earshot-runtime --example edge_words -- . --models <dir> --out <file> [--baseline <report>]`, counts only (no word text is printed or written). It is run once on the commit before the candidate (the baseline report) and once on the candidate with `--baseline`, which reuses the baseline's speaker maps and computes the verdict below. The report records which word ends the build produces (a check on a fixed token list), so a report cannot be filed under the wrong side.

**Recogniser and diarisation**, as the final pass on the CPU: `parakeet-tdt-0.6b-v2` through `backends::final_pass_asr`, `asr::transcribe_in_pieces` with `PieceConfig::default()` and `TranscribeOptions::live()` (the stretch final pass's options), Silero through `load_silero`, the kept words (`refs::kept_words`); `load_diarisation` with `PostConfig` defaults (calibrated clustering threshold and regrouping); words to speakers by `diar::assign_words`. Audio quantised as the stored FLAC.

**Data.** Dev data only.

- `scenes`: T6.9a's 20 LibriSpeech dev-clean two-speaker scenes of 240 s, built exactly as `word-changes.md` describes (same pairs, seeds and turn rules). A word's truth is the speaker whose true turn holds the word's **start** (T6.9a used the midpoint, which would move with the candidate); a start in an overlap or in no turn has no truth.
- `ami_mix`: AMI dev meetings ES2003a, ES2011a, IS1008a, TS3004a, TS3006a, `Mix-Headset` (set `ami-dev`), recognised and diarised whole.
- `ami_sdm`: the same meetings' `Array1-01` far-field recording (set `ami-dev-sdm`), the in-room case.
- AMI truth: each recognised word is matched to a reference word (the AMI manual words v1.6.2 under `word-changes.md`'s word rules) whose text is equal after Whisper's English normaliser (the whole word's normalised text, non-empty) and whose start is within 1.0 s, matched one to one in time order, the nearest unused start first. A matched word's truth is that reference word's speaker; an unmatched word has no truth and is left out of every rate (the count is reported). Matching uses starts only, so it does not depend on the candidate.

**Measures**, per set, pooled over its items. Each item maps its diarised speakers one to one onto true speakers to maximise the baseline's agreement; the candidate is scored with the same map.

- **WDER**: words with truth given the wrong speaker, over words with truth.
- **Edge errors** (the symptom): a word that is wrong, is the first word of its run of equal labels, and whose truth is the mapped speaker of the run just before it; or the last word of its run, with the truth the mapped speaker of the run just after it.
- **Short true turns kept**: words in true runs of at most 2 words (over words with truth, in order) labelled right.
- **Change recall**: as T6.9a (a true change between neighbouring words with truth, found if the labels change within one word either way).
- `durations` (informational, no part of the decision): median and 90th percentile length of recognised words of at most four letters (counted without the punctuation), with and without trailing punctuation.

**Decision.** The candidate ships only if all of these hold:

1. `ami_mix` and `ami_sdm` each: fewer edge errors and a lower WDER than the baseline.
2. `scenes`: edge errors not more and WDER not higher than the baseline.
3. Every set: short true turns kept not fewer.
4. Every set: change recall at most 1 percentage point lower.
5. Every item's diarisation turns are identical in the two runs (the report holds a hash per item); otherwise the comparison is void and is re-run.

If all hold, the candidate ships with unit tests, `STRETCH_RECORD_VERSION` raised (stored stretch pieces hold word ends), and `cargo xtask accept m2` and `cargo xtask accept m3` re-run and logged. If any fails, nothing ships from stage 1 and the result is written below.

**Stage 2** (the merge rule the task first named, in `diar::assign`) is written as its own section before any run of it, only if stage 1 fails or leaves the symptom common.

**Limits.** Five AMI meetings; AMI truth is only for words the recogniser got right, so the hardest words (mis-heard) are left out; one model.

## Result

Everything above was committed unchanged before the runs. Raw reports, unmodified: `edge-words-baseline.json` (word ends from the punctuation token) and `edge-words-candidate.json` (spoken ends, run with `--baseline`, verdict inside), both on the ARM64 Surface's CPU, full data, every item's diarisation identical in the two runs.

**The decision fails: nothing ships from stage 1.** The far-field set gets slightly worse.

| Set | Measure | Baseline | Spoken ends |
|---|---|---|---|
| `scenes` (12762 words with truth) | wrong / WDER | 81 / 0.635% | 80 / 0.627% |
| `scenes` | edge errors | 6 | 5 |
| `ami_mix` (9577 / 9597) | wrong / WDER | 633 / 6.610% | 631 / 6.575% |
| `ami_mix` | edge errors | 102 | 98 |
| `ami_mix` | change recall | 59.47% | 59.39% |
| `ami_mix` | short true turns kept | 136 of 306 | 137 of 306 |
| `ami_sdm` (8238 / 8237) | wrong / WDER | 566 / 6.871% | 573 / 6.956% |
| `ami_sdm` | edge errors | 81 | 81 |
| `ami_sdm` | change recall | 55.31% | 55.59% |
| `ami_sdm` | short true turns kept | 69 of 206 | 70 of 204 |

| Condition | Result |
|---|---|
| 1. `ami_mix` and `ami_sdm` fewer edge errors and lower WDER | **fails** (`ami_sdm`: edge errors equal, WDER higher) |
| 2. `scenes` not worse | holds |
| 3. Short true turns kept not fewer | holds |
| 4. Change recall within 1 point | holds |
| 5. Diarisation identical in both runs | holds |

`durations` confirm the mechanism on public data: short words with punctuation lasted 400 ms at the median (800 ms at the 90th percentile) on both AMI sets and 560 ms (880 ms) in the scenes, against 160 ms (320 ms) without; with spoken ends they lasted 240 ms (`ami_mix`), 160 ms (`ami_sdm`) and 320 ms (`scenes`) at the median. The stretch is real, but taking it away moves few words: about one wrong AMI word in seven is an edge error (102 of 633 and 81 of 566), and almost all of those stay wrong with spoken ends. On these recordings the turn boundaries the diarisation draws, not the word ends, decide most edge errors. The word counts differ slightly between the runs because uncovered-speech recovery (piece step 5) reads word spans.

Next: stage 2 (a rule in `diar::assign`), written as its own section before any run. The edge errors in the baseline report are the count it has to lower.

## Stage 2 rule (written before any run, 2026-11-24)

T6.9a found that snapping a speaker change to the nearest sentence end or pause did the work of its rule on AMI dev (unsupported changes 173 to 47), while merging short runs lost real changes; it suggested trying snapping alone under a new rule. This is that rule, judged with recognised words.

**Candidate.** After every word has its speaker by `diar::assign`'s overlap rule, over a track's words in time order (across segments): a boundary between words i-1 and i is **supported** when word i-1 ends a sentence (its text ends in `.`, `?` or `!`, ignoring closing quotes and brackets; in the `clause` variants also `,`) or the pause from word i-1's end to word i's start is at least 300 ms (T6.9's `NO_PAUSE_MS`). Left to right, a speaker change on an unsupported boundary moves to the nearest supported boundary at most S words away either way (the earlier on equal distance), if there is one inside the two runs it separates that leaves both runs non-empty (T6.9a's step 3, unchanged). Nothing else changes: no run is merged, no change is added or removed. A real one-word answer after a sentence end or a pause is on a supported boundary and never moves.

**Variants.** S in {1, 2} times support `sentence` or `clause`: four. Nothing else is tried. The function lives in `diar::assign` from the start (`snap_changes`), and the tool calls it, so what is measured is the code that would ship; production does not call it until the rule passes.

**Data.** Stage 1's data with today's word ends (`edge-words-baseline.json` setup: `scenes`, `ami_mix`, `ami_sdm`), plus `ami_held`: the second and third meetings of each dev series, `Mix-Headset` (ES2003b, ES2003c, ES2011b, ES2011c, IS1008b, IS1008c, TS3004b, TS3004c, TS3006b, TS3006c; set `ami-dev`), with stage 1's recogniser, diarisation and AMI truth rules. Stage 1's numbers were seen before this rule was written; `ami_held` was not measured by T12.88 before it. Every variant is scored on the same run as the baseline, with the baseline's speaker map.

**Pick.** On `ami_mix` and `ami_sdm` pooled: fewest edge errors, then lowest WDER, then S = 1, then `sentence`.

**Decision.** The picked variant ships only if all of these hold:

1. `ami_held`: fewer edge errors and a lower WDER than the baseline.
2. `ami_mix` and `ami_sdm` pooled: fewer edge errors; and each of them: WDER not higher.
3. `scenes`: edge errors and WDER not higher.
4. Every set: short true turns kept down by at most 2% of that set's words in short true turns.
5. Every set: change recall at most 1 percentage point lower.

If all hold, `label_segment_speakers` applies the picked variant to each track's words in time order before splitting segments, with unit tests (the symptom, a real one-word answer kept, a change out of reach kept, runs never emptied), and `cargo xtask accept m3` and `cargo xtask accept m6` must end unchanged or better; if they do not, the change is withdrawn and reported. If any condition fails, nothing ships and the result is written below.

**Tool.** `edge_words --stage2` (one run: baseline and variants, verdict inside), report `edge-words-stage2.json`, counts only.
