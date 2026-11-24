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

(Appended after the runs.)
