# Quiet voices in the room: gain and compression (T12.89)

Question (the owner, 2026-11-24): in an in-room meeting, someone not in front of the laptop is quieter. Would more microphone gain, or a compressor, help Earshot hear them (fewer missed and mis-heard words)?

Expectation before measuring (a judgement): gain alone should change little, because Parakeet's front end normalises each mel band over the decoded audio and the final pass decodes in speech pieces; a compressor raises a distant voice's level relative to a near one, but raises room echo and noise with it. The one place level could matter is voice detection (Silero), which decides what is decoded at all.

## Rule (written before any run, 2026-11-24)

**Data.** AMI dev meetings ES2003a, ES2011a, IS1008a, TS3004a and TS3006a (fixture sets `ami-dev-sdm` and `ami-dev-ihm`), the far-field recording `Array1-01` (the first microphone of the array in the middle of the table), each meeting's UEM, the pinned oracle segments and the AMI manual words. Dev data only; the acceptance meetings IS1009a and ES2004a are not used.

**Conditions**, each applied to a meeting's whole `Array1-01` recording, then quantised to 16-bit PCM (`audio::quantise_pcm16`, as the stored FLAC):

- `off`: as recorded.
- `gain12`: +12 dB (x 3.98), clamped to [-1, 1]. The control: a change of level alone.
- `comp4` and `comp10`: a feed-forward compressor with ratio 4 and 10. Envelope: one-pole smoothing of x squared, attack 5 ms, release 100 ms. Reference level L: the 95th percentile of 10 ms frame RMS (dBFS) inside the UEM. Threshold T = L - 25 dB. Above T the level becomes T + (level - T) / ratio; below T it is unchanged; make-up gain (L - T)(1 - 1/ratio), so a signal at L leaves at L and everything below is raised (noise included); output clamped to [-1, 1].

**Recogniser.** The production default English model on CUDA with the production final-pass constructors (`backends::final_pass_asr`, Silero through `load_silero`, `PieceConfig::default()`, the stretch track's final-pass options), as `m2_prod`. The hypothesis is the words production keeps (`refs::kept_words`).

**Measures**, per condition, summed over the five meetings:

1. `segments` WER (mis-hearing alone): the `ami_sdm` protocol of `m2_prod` on these meetings, one batch call per oracle segment, words outside every segment counted as deletions.
2. `whole` WER (the production in-room path: missed plus mis-heard): `asr::transcribe_in_pieces` over each meeting's UEM range, scored against every speaker's words in the UEM ordered by start time (`refs::ami_reference`), WerCounts summed.
3. `heard_quiet` (the quiet person): for each speaker, the share of their UEM reference words matched one to one by a word of the `whole` pass with the same normalised text (Whisper's English normaliser, per word) starting within 1.0 s of the reference word's start, matched in time order. A speaker's level is the RMS (dBFS) of the `off` recording over their segments minus every other speaker's segments; each meeting's quietest speaker is the one with the lowest level. `heard_quiet` pools the five quietest speakers' matched and total words. All speakers pooled are reported beside it.

**Decision.** A condition helps only if, against `off`, all three hold:

- `whole` WER at least 1.0 point lower;
- `heard_quiet` at least 2.0 points higher;
- `segments` WER at most 0.5 point higher.

If `gain12` passes, the effect is level alone (a gain change would do, no compressor needed). If a compressor passes and `gain12` does not, a follow-up task designs a live compressor for in-room meetings (mic only, before the recording and voice detection). If none passes, the idea is parked and the advice is microphone placement. Nothing ships from this task.

**Limits.** In AMI everyone sits at a similar distance from the array in the middle of the table, so the level spread between speakers is smaller than with a laptop at one end; five meetings; one model; the compressor's settings are one reasonable choice, not tuned.
