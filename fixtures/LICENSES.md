# Fixture licences and attribution

Every fixture artefact is listed in `fixtures/manifest.toml` with its source URL, SHA-256, size, licence and attribution. Large sets are downloaded into the fixture cache (`EARSHOT_FIXTURES`, default `%LOCALAPPDATA%\earshot-dev\fixtures`) by `cargo xtask fetch-fixtures`; only the smoke set and the LLM replay cases are committed.

## LibriSpeech (sets `smoke`, `librispeech`)

- Source: LibriSpeech ASR corpus, OpenSLR resource 12, https://www.openslr.org/12
- Authors: Vassil Panayotov, Guoguo Chen, Daniel Povey, Sanjeev Khudanpur. "LibriSpeech: an ASR corpus based on public domain audio books", ICASSP 2015.
- Audio derived from LibriVox public-domain audiobooks.
- Licence: Creative Commons Attribution 4.0 International (CC BY 4.0), https://creativecommons.org/licenses/by/4.0/
- Files used: `test-clean.tar.gz` and `dev-clean.tar.gz` (pinned subsets in `fixtures/librispeech/subset.toml`).
- Changes: none. The committed files in `fixtures/smoke/` are unmodified byte copies of 20 test-clean FLAC files; `fixtures/smoke/transcripts.txt` holds their reference transcripts copied from the corpus `.trans.txt` files.

## AMI Meeting Corpus (set `ami`)

- Source: AMI Meeting Corpus, https://groups.inf.ed.ac.uk/ami/corpus/ (audio from the official download host, `groups.inf.ed.ac.uk/ami/AMICorpusMirror`).
- Authors: J. Carletta et al., "The AMI Meeting Corpus: A Pre-announcement", MLMI 2005; University of Edinburgh and the AMI Consortium.
- Licence: Creative Commons Attribution 4.0 International (CC BY 4.0) for the corpus and its annotations, per https://groups.inf.ed.ac.uk/ami/corpus/license.shtml
- Files used: meetings IS1009a and ES2004a, channels `Mix-Headset` (IHM-mix), `Array1-01` (SDM) and the individual headsets `Headset-0` to `Headset-3` (IHM, scored per speaker by `asr.wer_ami_ihm`, T2.5b). Unmodified.
- Annotations: AMI manual annotations v1.6.2 (`https://groups.inf.ed.ac.uk/ami/AMICorpusAnnotations/ami_public_manual_1.6.2.zip`, CC BY 4.0), extracted unmodified into the fixture cache; the word-level files `words/<meeting>.<A-D>.words.xml` of the two meetings above are the references of `asr.wer_ami_ihm` (T2.5), `corpusResources/meetings.xml` maps each speaker to its headset, and the eight oracle segments files `segments/<meeting>.<A-D>.segments.xml` give the spans each headset is decoded in (T2.5b).
- `fixtures/ami/parse_cases.toml` (committed, T2.5b) quotes a few speaker and segment lines from `corpusResources/meetings.xml` and `segments/IS1009a.B.segments.xml` of the same annotations (CC BY 4.0) as parser test cases.

## Benchmark meeting (committed, shipped with the app, T7.4c)

- Source and licence: as above (AMI Meeting Corpus, CC BY 4.0): meeting ES2004a, 570 s to 720 s, headsets `Headset-0` to `Headset-3`.
- Files: `apps/desktop/src-tauri/resources/benchmark/mic.flac` (the Project Manager's headset) and `reference.flac` (the sum of the other three headsets), each peak-normalised to -3 dBFS, 16 kHz 16-bit mono FLAC, made by `cargo xtask bench-meeting` (checked with `--check`, SHA-256 pinned in `xtask/src/bench_meeting.rs`); the attribution is in `NOTICE.md` beside them and is shown in the app's Benchmark section.

## AMI summary evaluation (set `ami-sum`, T6.20)

- Source and licence: as above (AMI Meeting Corpus and AMI manual annotations v1.6.2, CC BY 4.0, same archive and download host).
- Files used: the human abstractive summaries `abstractive/<meeting>.abssumm.xml` (abstract, actions, decisions, problems) and the `words` files of the 40 scenario meetings of the AMI scenario-only partition (dev ES2003, ES2011, IS1008, TS3004, TS3006; test ES2004, ES2014, IS1009, TS3003, TS3007; series a-d), and the headsets `Headset-0` to `Headset-3` of the 20 test meetings, which `cargo xtask sum-eval` replays through Earshot. Unmodified; summaries generated from them stay under `target/` and are not committed.

- `fixtures/ami/atomic_facts.json` (committed, T6.20a) quotes the decision and action sentences of those summaries and holds atomic facts split from them by a local model, as adapted material under the same licence: AMI Meeting Corpus, CC BY 4.0, changed by splitting sentences.

## AMI dev-series mixes (set `ami-dev`, T6.9a)

- Source and licence: as above (AMI Meeting Corpus, CC BY 4.0, same download host).
- Files used: the `Mix-Headset` channel of ES2003a, ES2011a, IS1008a, TS3004a and TS3006a, for the word-boundary report in `docs/far-speakers/word-changes.md`. Unmodified. Never committed and never an acceptance set.

## AMI-diarization-setup references (set `ami`)

- Source: https://github.com/pyannote/AMI-diarization-setup (fork of https://github.com/BUTSpeechFIT/AMI-diarization-setup), pinned to commit `67c2d539286e89f68952d5dcf83912bd9f01dfae`.
- Authors: F. Landini, J. Profant, M. Diez, L. Burget (BUT Speech@FIT), "Bayesian HMM clustering of x-vector sequences (VBx) in speaker diarization: theory, implementation and analysis on standard tasks", 2020; pyannote fork by Herve Bredin.
- Licence: repository licence Apache License 2.0; the references are derived from the AMI manual annotations v1.6.2 (CC BY 4.0).
- Files used: `only_words/rttms/test/<meeting>.rttm` and `uems/test/<meeting>.uem` for the two meetings above, chosen as the first two entries of `lists/test.mini.txt`. Unmodified.

## LLM replay cases (set `llm`, committed)

- Source: own work for this project, written by hand in the OpenAI chat completions response format (no output of any real model or service).
- Files: `fixtures/llm/meeting.json` (an invented meeting transcript) and `fixtures/llm/cases/*.json` (recorded-format HTTP exchanges replayed by a loopback mock in `earshot-llm` tests and `cargo xtask accept m4`).
- Licence: own work, follows the project licence (not chosen yet, PLAN.md section 7). No third-party content.
