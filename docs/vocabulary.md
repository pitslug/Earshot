# Your words in the transcript (T12.15b, spike)

T12.15a gives the user's own words (the Dictionary) to every summary request. This spike asks
whether the same words, plus the names of saved people, can also make the transcript itself
spell them right, and at what cost. Evaluation only: nothing here changes what the app does.
No shipped change follows without the owner.

Three ways are compared:

- **(a) Whisper prompt.** whisper.cpp takes prompt tokens before each window
  (`TranscribeOptions::prompt`, passed as prompt tokens). Whisper models only: the default
  English model is Parakeet, which takes no prompt.
- **(b) Boosting in Parakeet's search.** While greedy TDT decoding picks a token, tokens that
  continue one of the listed words get a bonus. The listed words are a prefix tree of the
  model's own word pieces with Aho-Corasick failure links, as in k2/icefall's contextual
  biasing (`icefall/context_graph.py`, `ContextGraph`; sherpa-onnx `csrc/context-graph.cc`,
  used by its hotwords in modified beam search). Greedy search commits each token, so the
  bonus a partial match collected is never taken back, unlike in beam search.
- **(c) A correction pass.** After decoding, a decoded word close in sound or spelling to a
  listed word is replaced by it, and the original word is kept beside the replacement.

## The rule (written before measuring, 2026-11-10)

Everything in this section was fixed and committed before any of the measurements below were
run. The tool is the `vocab_eval` example (`crates/earshot-runtime/examples/vocab_eval/`).

### Data

Dev data only: **every utterance of LibriSpeech dev-clean** (2,703 utterances, about 5.4 hours,
40 speakers; set `librispeech` of `fixtures/manifest.toml`), each decoded by one direct call on
the whole utterance (no VAD pieces: the comparison is between decodes of the same audio, and
pieces would add the skip-recovery rule of T2.5d to every method). Test sets (test-clean, the
smoke clips, AMI test meetings) are never read. Whisper runs on every fourth utterance in
(speaker, chapter, utterance) number order (index 0, 4, 8, ...: 676 utterances), because
small.en on the CPU of the build host (RTF 0.4 with 4 threads in a speed check) is too
slow for the whole set beside other lanes; its baseline is decoded on the same 676.

AMI dev meeting speech (names and acronyms in a meeting, the owner's real case) is not part of
this spike: the AMI dev meetings are fetched only as Mix-Headset mixes without UEMs, and the
headset audio would be a new fixture set. A follow-up that wants to ship a method re-measures
it there first (see "What would justify a follow-up").

### Which words are listed

LibriSpeech references have no case, so names cannot be found by their capitals. A listed
spoken word is a **rare word** of the utterance's reference instead (after Le et al. 2021,
"Contextualized streaming end-to-end speech recognition with trie-based deep biasing and
shallow fusion", whose LibriSpeech biasing lists take words outside the 5,000 most common;
their common list comes from the LibriSpeech training text, which is not fetched here):

1. Every text is normalised by `normalize_english` (the WER normaliser) and split on spaces.
2. **Common words**: the 5,000 most frequent normalised tokens of the AMI manual annotations
   v1.6.2 (`ami/annotations/words`), every meeting outside the ten series of the
   scenario-only dev and test partitions (ES2003, ES2011, IS1008, TS3004, TS3006, ES2004,
   ES2014, IS1009, TS3003, TS3007): about 900,000 words of meeting speech, one text per
   words file, read by `refs::parse_ami_words` (the `asr.wer_ami_ihm` reader: punctuation
   and unfinished fragments left out, `&#39;` read as an apostrophe, the underscores of
   spelled letters such as `T_V_` removed), except that a word without a start time still
   counts. Ties at the cut go by text order.
3. **Rare token**: a normalised dev-clean reference token made only of the letters a-z and
   apostrophes, not a common word, and present in at most 2 of dev-clean's 97 chapters (a
   word used across many books, such as "lady", is ordinary English for the model; character
   names and specialist words stay in one book). The pool is every distinct rare token of
   dev-clean (5,114; the lists below hold 6,881 spoken listed words, counted once per
   utterance).
4. **List per utterance**: the utterance's distinct rare tokens, then **distractors** drawn
   without repeats from the pool minus every token of the utterance's reference, until the
   list holds **100 words** (the Dictionary's limit, `MAX_VOCABULARY_WORDS`), then shuffled.
   Draws and shuffle use ChaCha8 seeded with `0x7121_5b00 + utterance index`. A real list is
   one per user and most of it is not said in any one meeting: 100 words with a few spoken is
   that case at its largest.
5. **Written form**: each listed word in title case ("Bozzle"), as a user would type a name.
   Boosting adds its all-lower-case form as a second path; the correction pass compares
   without case.

### Methods and variants

Every variant is run once; nothing is tuned between runs.

- **Parakeet baseline** `parakeet-tdt-0.6b-v2` (the default English model), greedy TDT as
  shipped, on the CPU.
- **(b) Boosting**: each listed form tokenised into the model's word pieces by longest match
  from the left after the word-start mark `▁`; a token gets bonus `beta` (added to its logit,
  so to its log-probability) when it continues the current match or starts a listed word
  (the token's arcs from the current node and from every node on its failure chain). The
  durations are untouched. Variants: `beta` in {1, 2, 3, 4}, each in two forms:
  - `open`: boosted tokens compete with blank (icefall's behaviour);
  - `gated`: when the unboosted best token is blank it stays blank, so a bonus only changes
    which word is written, never whether one is (designed here, not taken from a reference:
    words).
- **(a) Whisper prompt**: `small.en` (the default English whisper model on Vulkan), greedy
  (`TranscribeOptions::live()`, as the final pass decodes), on the CPU, without and with
  `prompt` = the list in its shuffled order joined by ", " and ending with ".". whisper.cpp
  keeps at most the last 224 prompt tokens, so a long list is cut from the front, as it would
  be in the app.
- **(c) Correction pass**, applied to the Parakeet baseline and to the whisper baseline (no
  decoding: it rewrites the decoded words). A decoded word is compared after stripping
  leading and trailing punctuation, which is put back around a replacement. A word equal to a
  listed word (ignoring case) is never changed. Variants:
  - `sound`: replace when the word's **sound key** equals exactly one listed word's sound key
    and the key has at least 2 letters. Sound key (a reduced key after Philips' Metaphone,
    1990): lower case, letters only; `ph`->f, `ck`->k, `sch`->sk, `qu`->kw, `wh`->w, leading
    `kn`->n and `wr`->r, `gh`->g at the start and dropped elsewhere; `c`->s before e, i, y,
    else k; `q`->k, `x`->ks, `z`->s, `v`->f; then the first letter stays (any vowel or y as
    "a"), every later a, e, i, o, u, y, h and w is dropped, and runs of one letter collapse.
    This is the owner's "a saved Kat wins over Cat" taken literally.
  - `sound-rare`: `sound`, but a decoded word whose normalised tokens are all common words
    (step 2) is never changed.
  - `spelling-rare`: replace when exactly one listed word of at least 4 letters is within
    Levenshtein distance max(1, floor(len/4)) of the decoded word (lower case, the listed
    word's length), and the decoded word is not common in the same sense.
  Every replacement records the original word, the listed word and the position.

### Metrics

The hypothesis is the words production keeps (`refs::kept_words`: non-speech annotations
dropped) joined by spaces. Per utterance, reference and hypothesis are normalised and aligned (word Levenshtein, ties
in the order match/substitution, deletion, insertion, as `metrics::wer::align_tokens`). With
the utterance's listed tokens S (spoken listed words P: the reference tokens in S; distractors
D: S minus the reference's tokens):

- **WER**: corpus WER, summed counts.
- **B-WER** (listed words, Le et al.): substitutions and deletions at reference positions
  whose token is in P, plus inserted hypothesis tokens in S, over the number of such positions.
- **U-WER** (unlisted words): the same for every other reference position and every other
  inserted token, over the other positions.
- **Listed recall**: listed reference positions aligned to an equal token, over all listed
  positions.
- **Distractor hits**: hypothesis tokens in D (a listed word written where it was not said),
  absolute and per 1,000 reference words. A meeting hour holds about 9,000 words.
- **Changed words**: reference positions correct in the baseline and wrong in the method
  (lost) and the reverse (gained), counted apart for listed and unlisted words.
- Decode time is recorded with the host load noted; it decides nothing.

### Choosing a variant and what would justify a follow-up

For each method, its variants are judged against that method's own baseline (whisper against
whisper on the same 676 utterances, the others against Parakeet):

- G1: distractor hits per 1,000 reference words at most 0.5 above the baseline's;
- G2: U-WER at most 0.1 points above the baseline's;
- G3: overall WER not above the baseline's.

The chosen variant of a method is the one with the lowest B-WER among those passing G1-G3
(ties: the earlier variant in this order: boosting open with bonus 1, 2, 3, 4, then gated
with 1, 2, 3, 4; correction sound, sound-rare, spelling-rare). The correction pass is judged
twice, on each baseline. A method **justifies a
follow-up task** when its chosen variant lowers B-WER by at least 20% of the baseline's B-WER.
If more than one method does, the recommendation is the one with the larger relative B-WER
reduction, the correction pass winning a tie (no decoder change), then the prompt. The
follow-up re-measures the method on AMI dev headset speech with the AMI named-entity
annotations as the list before anything ships, and is the owner's decision.

Variants that fail a guard are still reported with their numbers, `sound` in particular,
because it is what "a saved Kat wins over Cat" costs.

### Commands

The runs, from the repo root, with the artefacts written to `docs/vocabulary/` (one JSON line
per utterance: id, hypothesis, decode time; the correction files also list every replacement
with the original word):

```powershell
cargo build --release -p earshot-runtime --example vocab_eval
```

```powershell
target\release\examples\vocab_eval.exe --models $env:LOCALAPPDATA\Earshot\models --fixtures $env:LOCALAPPDATA\earshot-dev\fixtures --out docs\vocabulary --decode parakeet --workers 4 --threads 4
```

```powershell
target\release\examples\vocab_eval.exe --models $env:LOCALAPPDATA\Earshot\models --fixtures $env:LOCALAPPDATA\earshot-dev\fixtures --out docs\vocabulary --decode whisper --threads 8
```

```powershell
target\release\examples\vocab_eval.exe --fixtures $env:LOCALAPPDATA\earshot-dev\fixtures --out docs\vocabulary --score
```

`--score` reads the decodes, runs the correction pass and writes `docs/vocabulary/results.json`.
The models come from `cargo xtask fetch-models --set ci` (Parakeet) and the whisper models of
the desktop set; the fixtures from `cargo xtask fetch-fixtures --set librispeech` and
`--set ami` (the annotations).

(These are the commands as planned. What is committed differs; see "Deviations" and
"Reproducing" below.)

## Results (measured 2026-11-11)

Host: the x64 desktop (32 logical cores), CPU only, shared with other lanes compiling and
testing throughout; WER does not depend on the load, decode times do and decide nothing.
Backends from evidence: ONNX Runtime placed every Parakeet node on the CPU; whisper.cpp's log
said CPU on every call (`docs/vocabulary/*-meta.json`). The lists hold 6,881 spoken listed
words (6,977 reference positions, 12.7% of dev-clean's 54,798 reference words); every other
listed entry is a distractor.

### Parakeet 0.6B v2: boosting (b) and the correction pass (c), all 2,703 utterances

| Variant | WER % | B-WER % | U-WER % | Listed recall % | Distractor hits (per 1,000 words) | B-WER change | G1 G2 G3 |
|---|---|---|---|---|---|---|---|
| baseline | 1.64 | 6.16 | 0.98 | 93.87 | 0 (0.00) | | |
| boost open 1 | 1.58 | 5.49 | 1.00 | 94.54 | 0 (0.00) | -10.9% | pass pass pass |
| boost open 2 | 1.51 | 4.99 | 1.00 | 95.06 | 0 (0.00) | -19.1% | pass pass pass |
| boost open 3 | 1.57 | 4.57 | 1.13 | 95.49 | 1 (0.02) | -25.8% | pass **fail** pass |
| boost open 4 | 1.96 | 4.77 | 1.55 | 95.53 | 11 (0.20) | -22.6% | pass **fail** **fail** |
| boost gated 1 | 1.54 | 5.52 | 0.96 | 94.51 | 0 (0.00) | -10.5% | pass pass pass |
| boost gated 2 | 1.52 | 5.09 | 0.99 | 94.94 | 0 (0.00) | -17.4% | pass pass pass |
| boost gated 3 | 1.55 | 4.69 | 1.10 | 95.36 | 1 (0.02) | -24.0% | pass **fail** pass |
| boost gated 4 | 1.79 | 4.69 | 1.37 | 95.36 | 1 (0.02) | -24.0% | pass **fail** **fail** |
| correct sound | 4.53 | 3.00 | 4.75 | 97.18 | 1,747 (31.88) | -51.4% | **fail** **fail** **fail** |
| correct sound-rare | 1.46 | 3.01 | 1.24 | 97.03 | 137 (2.50) | -51.2% | **fail** **fail** pass |
| correct spelling-rare | 1.49 | 3.34 | 1.22 | 96.72 | 99 (1.81) | -45.8% | **fail** **fail** pass |

Changed words against the baseline (listed gained / lost; unlisted gained / lost): boost open 2
107 / 24 and 55 / 45; boost gated 2 97 / 22 and 49 / 44; boost open 3 151 / 38 and 65 / 92;
sound-rare 222 / 1 and 0 / 132 (a correction only ever writes a listed word, so it never gains
an unlisted one). The correction pass made 2,124 (sound), 433 (sound-rare) and 391
(spelling-rare) replacements, of which 377, 296 and 292 wrote a word said in the utterance.

**Chosen: boost open 2, B-WER -19.1%, under the 20% bar: boosting does not justify a
follow-up.** No correction variant passes G1 and G2.

### small.en: the prompt (a) and the correction pass (c), 676 utterances

| Variant | WER % | B-WER % | U-WER % | Listed recall % | Distractor hits (per 1,000 words) | B-WER change | G1 G2 G3 |
|---|---|---|---|---|---|---|---|
| baseline | 2.83 | 8.57 | 2.01 | 91.48 | 0 (0.00) | | |
| prompt | 2.52 | 6.85 | 1.90 | 93.21 | 0 (0.00) | -20.1% | pass pass pass |
| correct sound | 5.47 | 4.55 | 5.60 | 95.57 | 433 (31.24) | -47.0% | **fail** **fail** **fail** |
| correct sound-rare | 2.60 | 4.95 | 2.26 | 95.11 | 36 (2.60) | -42.3% | **fail** **fail** pass |
| correct spelling-rare | 2.68 | 5.52 | 2.27 | 94.53 | 30 (2.17) | -35.6% | **fail** **fail** pass |

13,860 reference words, 1,738 listed positions. The prompt changed listed words 57 gained / 27
lost and unlisted words 28 / 30.

**Chosen: the prompt, B-WER -20.1%: by the rule the whisper prompt justifies a follow-up, with
0.1 points to spare** (30 fewer listed-word errors out of 1,738 positions).

### What the numbers say

- **The whisper prompt** is the only method over the bar, and only just. It is also the only
  one with no cost seen here: no distractor written, unlisted words slightly better. It reaches
  only transcripts written by whisper (small.en on Vulkan machines, large-v3-turbo for other
  languages); the default English model on the CPU, NVIDIA cards and the NPU is Parakeet. A
  100-word list is longer than the 224 prompt tokens whisper.cpp keeps, so part of every list
  was cut; that was in the measurement, as it would be in the app.
- **Boosting** buys 17-19% fewer listed-word errors with no visible cost at a bonus of 2, and
  24-26% at 3, where unlisted words start to suffer (92 lost against 65 gained). Greedy search
  cannot take a bonus back, which is the likely limit; beam search with icefall's retraction is
  the known way past it and is a much larger change (no beam TDT search exists here).
- **The correction pass** halves listed-word errors but writes a listed word where it was not
  said 1.8-2.6 times per 1,000 words in its careful forms: about 16-23 times in a meeting hour
  with a 100-word list. The literal "a saved Kat wins over Cat" (`sound`) does it about 32
  times per 1,000 words, about 290 an hour. Most false corrections replace ordinary words that
  share a key or a close spelling with a listed word; the common words (from meeting speech)
  protect only what meetings say most. As a suggestion shown beside the word it could still
  help; as a replacement it does not.

### Recommendation

1. Leave the Parakeet path alone: neither boosting nor a correction pass passes the rule.
2. The whisper prompt is the one candidate, with a thin margin and a small reach. Before any
   shipped change a follow-up would re-measure it on AMI dev headset speech with the AMI
   named-entity annotations as the list (the dev headset audio is a new fixture set), check
   what a prompt does on non-speech (model_eval's `silence` corpus with a prompt: a prompted
   whisper can write the prompt back), and decide how a list longer than the prompt is cut.
3. If the owner wants "Kat over Cat" on Parakeet, the safe form is a suggestion: mark a
   decoded word that sounds like a saved name and offer the name, keeping the decoded word as
   written. It needs no decoder change and writes no false text; it is a UI task.

## Deviations from the rule

Each decided before any number of the affected decode was read:

- **Artefacts.** The rule planned to commit each decode's hypotheses. LibriSpeech text holds
  names of people and places, and one of them matches the private names list that
  `public-guard` checks, so the committed record is word-free: per decode
  `docs/vocabulary/counts-<decode>.jsonl` (per utterance: id, SHA-256 of the hypothesis text,
  and the counts in the order of `results.json`'s `count_fields`), with `results.json` and the
  two meta files. The hypotheses and the correction pass's replacement records (original word
  kept beside each) stay in `target/vocab-eval/`, regenerated by the commands below.
- **One encoding for every boosting variant.** The first Parakeet run decoded each variant
  with its own encoder call and was on course for over 3 hours beside the other lanes; it was
  stopped after about 100 utterances and never scored, and `transcribe_biased` now takes every
  variant's bias and encodes each piece once. Decoding is unchanged; the baseline still goes
  through `Transcriber::transcribe`.
- **Whisper ran alone.** The first whisper run shared the CPU with the Parakeet run and made
  almost no progress in 45 minutes (whisper.cpp's threads spin while they wait, which an
  oversubscribed CPU turns into a crawl); it was stopped and nothing from it was read. The
  measured run started after Parakeet had finished and took 42 minutes.

## Reproducing

From the repo root; `target` stands for the cargo target folder (this lane built into
`C:\eb\lane-vocabasr`). Run the two decodes one after the other, not together.

```powershell
cargo build --release -p earshot-runtime --example vocab_eval
```

```powershell
target\release\examples\vocab_eval.exe --models $env:LOCALAPPDATA\Earshot\models --fixtures $env:LOCALAPPDATA\earshot-dev\fixtures --out target\vocab-eval --decode parakeet --workers 4 --threads 4
```

```powershell
target\release\examples\vocab_eval.exe --models $env:LOCALAPPDATA\Earshot\models --fixtures $env:LOCALAPPDATA\earshot-dev\fixtures --out target\vocab-eval --decode whisper --threads 8
```

```powershell
target\release\examples\vocab_eval.exe --fixtures $env:LOCALAPPDATA\earshot-dev\fixtures --out target\vocab-eval --report docs\vocabulary --score
```

`--score` takes seconds and rewrites `docs/vocabulary/`. A rerun's hypotheses can be checked
against the committed `hyp_sha256` values; multithreaded whisper.cpp CPU decodes are not
bit-reproducible, so a whisper rerun may differ in a few utterances.

The evaluation-only hook in the shipped decoder, `ParakeetTdt::transcribe_biased`, is pinned to
decode exactly as `transcribe` when its bias changes nothing: `cargo test -p earshot-onnx
parakeet` (a stand-in model) and `cargo test --release -p earshot-onnx --test parakeet --
--include-ignored` (the real model on the smoke clips).

# The nudge on meeting speech (T12.15c)

The owner (2026-11-11) chose to ship boosting "open, bonus 2" (above: B-WER -19.1% within G1-G3)
for 0.3, waiving the 20% bar, once it has been re-measured on meeting speech. This section is
that measurement. Evaluation only; T12.15d ships the change only if the verdict below passes.

## The rule (written before measuring, 2026-11-11)

Everything in this section was fixed and committed before any decode below was run. The
annotations were read beforehand only to count how many named-entity words each meeting has
(about 40 distinct non-common ones over the five meetings; that count chose nothing below). The
tool is the same `vocab_eval` example (`--decode ami`, `--score-ami`).

### Data

AMI dev meeting speech only: the individual headsets (IHM) of the five meetings of fixture set
`ami-dev` (ES2003a, ES2011a, IS1008a, TS3004a, TS3006a: the first meeting of each series of the
scenario-only dev partition), pinned as the new fixture set `ami-dev-ihm` (20 headset
recordings and the five meetings' UEMs from AMI-diarization-setup at the commit the `ami` set
uses, `uems/train/` for ES2003a and TS3006a, `uems/dev/` for the others), with the meetings'
oracle segments and named-entity annotations as newly pinned members of the v1.6.2 annotations
archive. Decoded exactly as `asr.wer_ami_ihm` decodes the test meetings (`refs::load_ami` with
`AmiMic::Ihm`: each speaker's segments clipped to the UEM, one direct call per segment on the
speaker's own headset, reference the speaker's words starting inside it, the speaker's UEM
words outside every segment counted as deletions). No test meeting is read.

### The model and the one variant

`parakeet-tdt-0.6b-v2`, greedy TDT (`TranscribeOptions::live()`), on the CPU. Baseline through
`Transcriber::transcribe`; the boost through `transcribe_biased` with boosting **open, bonus 2**
exactly as T12.15b ran it (same `ContextGraph`, tokenising, title-case and lower-case paths).
Nothing is tuned; no other bonus or gate is run.

### Lists

One list per meeting, used for every segment of that meeting (a user has one list, not one per
sentence). Tokens are `normalize_english` tokens split on spaces; "common words" are T12.15b's
5,000 (AMI meetings outside the ten dev and test series); "the meeting's words" are every word
of its four speakers' words files, all times.

- **`ne` (the verdict list).** The meeting's named entities (`namedEntities/<meeting>.<X>.ne.xml`,
  speakers A-D in order, entities in file order), every type except those under TIMEX and NUMEX
  (times, dates, durations, money, measures, percentages and numbers: not what a user types
  into a Dictionary, and the normaliser turns them into digits). Each entity's words (its child
  range in the speaker's words file, punctuation and unfinished fragments left out, as
  `refs::parse_ami_words` reads them) give their tokens; a token is listed when it is made only
  of a-z and apostrophes and is not a common word: a name, place, product or uncommon term, what
  a user would add. Distinct, in that order, at most 100.
- **Distractors** fill each list to 100 words: drawn without repeats from T12.15b's pool (the
  rare tokens of LibriSpeech dev-clean) minus every token of the meeting's words, then the list
  shuffled, with ChaCha8 seeded `0x7121_5b00 + 100_000 + k` for meeting k (0-4 in the order
  above), the draws and shuffle of T12.15b's step 4.
- **Informational lists**, run in the same decode and judged by the same guards, deciding
  nothing: **`ne-all`**, `ne` without the common-word filter (role names, colours and objects
  that the meeting says often); and **`rare`**, T12.15b's rare-word rule adapted per meeting:
  the meeting's tokens (its speakers' UEM words in start order) made only of a-z and
  apostrophes, not common, and in no other of the five meetings' words, distinct, at most 100.

### Metrics and guards

As T12.15b, per segment, summed: WER, B-WER, U-WER, listed recall, distractor hits per 1,000
reference words and changed words against the baseline, with one change forced by the meeting
list: a **distractor** is a listed token not among the meeting's words (T12.15b took the
utterance's reference), so a listed word said elsewhere in the meeting and written in a segment
where it was not said counts as a B-WER insertion, not as a distractor hit. The outside words of
each speaker are one more segment with an empty hypothesis in every decode.

- G1, G2, G3: exactly T12.15b's (distractor hits per 1,000 words at most 0.5 above the
  baseline's, U-WER at most 0.1 points above, WER not above), against the baseline on the same
  segments.
- **G4 (non-speech)**: model_eval's `silence` clips (30 s each: dither at -90 dBFS, white noise at
  -60 and -40 dBFS; same generator and seed `0x6e0_15e`), each decoded directly, without VAD,
  once unbiased and once boosted with each meeting's `ne` list (15 boosted decodes). G4 passes
  when no boosted decode writes a token of its list. The informational lists run on the same
  clips and are reported.

### Verdict

**Passes when G1, G2, G3 and G4 all pass for `ne` and its B-WER is below the baseline's**
(strictly). It is judged on whatever number of listed positions the five meetings hold; that
number, the listed words gained and lost, and the informational lists are reported beside it.
Decode time is recorded with the host load and decides nothing (every list is decoded from one
encoding per segment, so a variant's time is the call's shared out).

### Commands

```powershell
cargo xtask fetch-fixtures --set ami-dev-ihm
```

```powershell
cargo build --release -p earshot-runtime --example vocab_eval
```

```powershell
target\release\examples\vocab_eval.exe --models $env:LOCALAPPDATA\Earshot\models --fixtures $env:LOCALAPPDATA\earshot-dev\fixtures --out target\vocab-eval-ami --decode ami --workers 4 --threads 4
```

```powershell
target\release\examples\vocab_eval.exe --fixtures $env:LOCALAPPDATA\earshot-dev\fixtures --out target\vocab-eval-ami --report docs\vocabulary\ami --score-ami
```

The committed record is word-free, as for T12.15b: `docs/vocabulary/ami/results.json`, the meta
file and per decode `counts-<name>.jsonl` (per segment its id, the SHA-256 of the hypothesis and
the counts). AMI text holds names, so hypotheses and lists stay in `target/`.
