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
