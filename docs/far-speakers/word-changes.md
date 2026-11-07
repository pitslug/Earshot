# Speaker changes on word boundaries (T6.9a)

Written before any measurement. The rule, the data and the success criterion below are not changed after the first run; results are appended in the Result section only. That order rests on the lane's word: git cannot show it, because the commit came after the runs. What can be checked is that `word_changes` computes the verdict from the committed criterion, and that `word-changes.json` holds the counts it used.

Tool: `cargo run --release -p earshot-runtime --example word_changes -- <repo-root> --models <dir> --out docs/far-speakers/word-changes.json`. Counts only. No private recording takes part: dev-clean and the AMI dev series, nothing else.

## The rule

Words are in time order with the speaker `diar::assign_words` gives each (the baseline, no rule). A boundary between word i-1 and word i is **supported** when word i-1 ends a sentence (its text ends in `.`, `?` or `!`, ignoring closing quotes and brackets) or the pause between them is at least 300 ms (T6.9's `NO_PAUSE_MS`).

Applied left to right over runs of one speaker:

1. **Merge(N).** A run of fewer than N words whose start boundary is not supported takes the previous run's speaker, whatever comes after it. (The brief's example: a change needs a sentence end or a pause unless the new run holds at least N words.)
2. **Flip.** A run of fewer than N words with one speaker on both sides, where either boundary is not supported, takes that speaker.
3. **Snap(S)**, for S > 0, after 1 and 2. A change still on an unsupported boundary moves to the nearest supported boundary within S words either way (earlier on a tie), if there is one and it does not make either run empty.

The variants are the six combinations of N in {2, 3, 5} and S in {0, 2}. Nothing else is tried.

## The data

- **Scenes**: LibriSpeech dev-clean, the 40 speakers in path order as 20 consecutive pairs, one two-speaker scene of 240 s per pair, quantised as the FLAC writer stores audio. Turns alternate; a turn is whole utterances (200 ms apart) until it holds at least a target drawn from 5-30 s. With probability 0.2 a turn is interrupted: its last utterance is cut at 30-70% of its length, mid-sentence. The next turn follows a 0.1-0.8 s silence or, with probability 0.2, overlaps the turn by 0.2-1.0 s (at most half that turn). No audio is played twice. Split A is pairs 0-9, split B pairs 10-19.
- Words come from the production final-pass model on the CPU (`parakeet-tdt-0.6b-v2`, `transcribe_in_pieces` with `PieceConfig::default()` and `TranscribeOptions::live()`: greedy, the stretch final pass's options; kept words), turns from the production final-pass diarisation on the CPU (`load_diarisation`, `PostConfig` defaults with the calibrated threshold).
- A word's truth is the speaker whose true turn holds the word's midpoint. A midpoint in an overlap or in no turn has no truth, and that word is left out of every rate (the count left out is reported).
- **AMI dev**: the first meeting of each dev series, ES2003a, ES2011a, IS1008a, TS3004a, TS3006a: `Mix-Headset` audio (new manifest set `ami-dev`, same host and licence as the `ami` set) through the same diarisation. The words are the manual annotations v1.6.2 words files with their own times and their speaker's letter as truth, in start-time order across speakers (the `asr.wer_ami_ihm` word rules: no `<vocalsound>`, `<disfmarker>`, `<gap>`, no `trunc` fragment; a `punc` entry of `.`, `?` or `!` becomes that speaker's previous word's sentence end). These words stand in for a recogniser's, with no recogniser error. This is real conversation with backchannels, so it is the guard against erasing true short turns. The AMI test series and acceptance meetings are never used.

## The measures

- **WDER**: words given the wrong speaker over words with truth, pooled over a data set. Each scene or meeting maps its diarised speakers one to one onto true speakers to maximise the baseline's agreement; the same map scores every variant. A diarised speaker left unmapped is always wrong.
- **Change recall**: true speaker changes between neighbouring words with truth where the labels also change within one word either way.
- **Unsupported changes** and **flips of at most 3 words** in the labels (T6.9's counts).
- **Words in true runs of at most 3 words** labelled right (reported).

## The success criterion

The variant is picked on split A alone: lowest WDER, then the smallest N, then S = 0. It is implemented in `diar::assign` only if all of these hold for that variant:

1. Split B: pooled WDER at least 5% lower than the baseline's (relative), and at least 5 words fewer wrong.
2. AMI dev: pooled WDER not higher than the baseline's.
3. Split B and AMI dev each: unsupported changes and flips of at most 3 words both fall.
4. Split B and AMI dev each: change recall falls by at most 1 percentage point.
5. Split A itself: the picked variant's WDER is lower than the baseline's.

If any fails, `diar::assign` stays as it is and this report is the result. If all hold, `cargo xtask accept m3` and `cargo xtask accept m6` must then end unchanged or better; if they do not, the change is withdrawn and that is reported.

## Result

Everything above was committed unchanged. Raw report: `word-changes.json` (threshold 0.68, the committed constant). `word-changes-at-083.json` is the same run at 0.83, for T6.9b: the verdict is the same.

**The criterion fails: `diar::assign` stays as it is.** Split A picked N = 2, S = 2.

| Set | Measure | Baseline | Rule (2, 2) |
|---|---|---|---|
| Split A (6345 words) | wrong | 6 (0.095%) | 0 |
| Split A | unsupported changes / flips | 9 / 1 | 0 / 0 |
| Split B (6407 words) | wrong | 77 (1.202%) | 77 (1.202%) |
| Split B | unsupported changes / flips | 3 / 0 | 1 / 0 |
| AMI dev (12584 words) | wrong | 1656 (13.160%) | 1634 (12.985%) |
| AMI dev | change recall | 32.015% | 31.279% |
| AMI dev | unsupported changes / flips | 173 / 105 | 47 / 78 |
| AMI dev | words in true runs of at most 3 that are right, of 2136 | 890 | 881 |

| Condition | Result |
|---|---|
| 1. Split B WDER 5% lower and 5 words fewer wrong | **fails** (no change) |
| 2. AMI dev WDER not higher | holds |
| 3. Unsupported changes and flips both fall, split B and AMI dev | **fails** (split B has no flips to fall) |
| 4. Change recall within 1 point | holds |
| 5. Split A WDER lower | holds |

What this does and does not say:

- Split B's 77 wrong words go with the one scene where the diarisation found a third speaker: at 0.83, where every scene finds two, split B has none wrong. A word rule cannot repair that. The dev scenes are too clean to show much: at most 9 unsupported changes in 6345 words.
- Condition 3 could not pass on flips because split B has none. That is a flaw in the criterion as written; it is left as written, and condition 1 fails on its own anyway.
- Snapping does the work. Merging alone (S = 0) is worse than the baseline on AMI dev at every N, and a larger N loses more real changes (recall 27.7% at N = 5, S = 0).
- On AMI dev the picked rule removes 73% of unsupported changes and is slightly better overall, at a cost of 9 right words in short true runs and 0.7 points of recall. That is a reason to try snapping alone under a new rule written first, not grounds to ship this one.
- Change recall is low with or without a rule (32%): most real AMI speaker changes are short overlapping turns the diarisation does not separate.
- Timings were not measured.
