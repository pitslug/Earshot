# Saved people's names in summaries (T12.91)

A summary spelled a colleague's name as the recogniser heard it, because the colleague was only talked about, not in the call. Saved people's names now reach summaries as spelling help, like the Dictionary's words (the owner, 2026-11-24: all speakers' names are part of the Dictionary; names go only to the summary writer the user chose, and a meeting kept on this computer runs its summary locally as before). The list can hold an organisation's people, so names are filtered per request rather than sent whole (the owner, 2026-11-24); the Dictionary's own words are still sent whole, within their T12.15a limits.

## What ships

- **The list.** Every live saved person's display name (`earshot_runtime::word_choices::people_of`, over `named_persons`, the transcript suggestions' persons rule): not removed or merged-away persons, not the default "Me" (the user's own name once they set one), never "Speaker N" or a voice without a person. Cleaned by `earshot_core::prompt::People::new`: one line, at most 100 characters, no control or invisible formatting character, and a name that repeats an earlier one or a Dictionary entry in another case is left out (the Dictionary carries it). Read at summary time (`summary::load_input`) and once per live session at its start.
- **The filter** (`earshot_core::vocabulary::names`, beside the shipped matcher): over a request's white-space tokens (case ignored, punctuation and a trailing possessive dropped), a name matches *exactly* when its words appear as consecutive tokens, by a *word* when one of its words is a token (a name word that is a common word, the matcher's own list, only where the token is written with an upper-case first letter: "Mark said" matches, "mark the date" does not, "Will you" does), and by *sound* when the shipped matcher (`vocabulary::suggest`, unchanged, with the transcript suggestions' word list) offers one of its words for a token. A single request and a map request (a live window included) are matched against what they show: the meeting's title and attendees, the speakers and text of their transcript lines and their notes. A merge is matched against the meeting's title, attendees and notes and its partial summaries, and keeps every name its parts carried (`prompt::reduce_prompt_carrying`), so a name chosen in a map step is never lost at the merge.
- **The cap.** A request adds matched names strongest first (exact, then word, then sound; ties in the list's order) while it holds fewer than `prompt::MAX_PEOPLE_PER_REQUEST`; names a merge's parts carried are never dropped for it. Its value is chosen by the rule below. Until that rule has run the constant holds a placeholder (50).
- **The block** (`templates/people.md`): right after the Dictionary's block and before the style, every name a line prefixed with `| `, so no name can close or fake the markers. A request that carries no name is byte-identical to one from before T12.91. `plan` and `reduce_groups` count an upper bound of the block (every carried name plus the costliest `MAX_PEOPLE_PER_REQUEST` others) in every request's room.
- **Live summaries.** The live record (version 3) keeps the list its windows were mapped with; a summary whose list differs maps those windows again.

## The cap rule (written before measuring)

This section is fixed before the measurement runs. The result is added below it; nothing here is changed afterwards.

The upper-case condition on common name words in the filter's word match was decided by the coordinator after a synthetic dry run of the tool and before any AMI measurement; the bounds and candidate caps below were written before it and are unchanged.

The invented list's shape (distinct surnames, each first name used by at most two people) and the split of unspoken names into namesakes and spurious names were also decided by the coordinator after that synthetic dry run and before any AMI measurement; choosing everyone who shares a spoken first name or surname is intended, since the filter cannot know which of them was meant.

The median map growth bound was 200 estimated tokens as first written; the coordinator raised it to 400 after the synthetic dry run showed the block's fixed text alone (its introduction and markers, the Dictionary block's wording) costs 191, so 200 allowed about one name. 400 is about 2.5% of the local model's 16k context. Decided before any AMI measurement; the other bounds and the candidate caps are unchanged.

### Data

- **Meetings:** the AMI dev split of fixture set `ami-sum` (ES2003, ES2011, IS1008, TS3004, TS3006, parts a to d: 20 meetings), the split every summary design choice is made on (docs/summary-eval.md). Never the test split, never the owner's meetings.
- **Transcripts:** the AMI reference transcripts, built by sum_eval's rules (`sum_eval/ami.rs`: manual words, turns cut at 1 s pauses and 30 s), each speaker named by their AMI `global_name` pseudonym. Why reference rather than Earshot's own transcripts: they are reproducible from the pinned annotations without running a model, and the invented names below are never spoken in AMI, so recognition errors on them cannot be observed in Earshot's transcripts anyway; the sound part of the filter is exercised by controlled mishearings instead (Injection).
- **The list:** 400 invented full names plus the meeting's own speakers' pseudonyms (four per meeting). The invented names come from `crates/earshot-core/examples/summary_names/seeds.toml`: 200 first names (every `first_start` + `first_end`, 196 invented, then four that are also common English words: Will, Mark, Grace, Rose, standing for the names a real list holds that collide with ordinary speech) and 400 invented surnames (every `last_start` + `last_end`); name *i* (0 to 399) is `first[i mod 200]` and `last[7i mod 400]`, so every surname belongs to one person and every first name to two, and no word is both a first name and a surname. They are never real people's names. Every seed piece, first name, surname and first and last name pair was checked against the private names list with `public_guard::parse_list` and `hits_in_bytes`, printing counts only (`cargo test -p xtask summary_names_seeds -- --ignored`: 200 first names, 400 surnames, 80,000 pairs, 0 hits, 2026-11-25).

### Injection

Per meeting, eight invented names are mentioned: name `(h + 53j) mod 400` for j = 0 to 7, where h is the FNV-1a hash of the meeting id mod 400. Each is appended once whole to segment `floor((2j + 1) n / 16)` and once as a misheard surname to segment `floor((2j + 2) n / 16)` (n segments, clamped to the last). The misheard surname replaces its first vowel after the first letter by the first other vowel (in the order a, e, i, o, u) that keeps the shipped matcher's sound key, changes the spelling and is not a common word ("Oskarine" becomes "Oskerine"); a surname with no such spelling gets no misheard mention.

### Requests measured

Through the production builder (`prompt::plan_chunks`, `plan`, `reduce_prompt_carrying`) with budgets large enough that every request is built as asked:

- **map:** the local writer's 600 s windows (`prompt::time_chunks`);
- **single:** the whole meeting in one request (an API writer's plan for a meeting that fits);
- **merge:** the last merge over the map parts, carrying their names, with partial summaries that name nobody (so it holds exactly what the parts carried and the meeting's attendees; real partials can only add names they write).

The tool rebuilds each map and single request's texts and checks that the filter's top `MAX_PEOPLE_PER_REQUEST` equals the builder's choice for every request; any difference stops the run.

### Measures

Per request kind: names chosen (median, maximum), names matched with no cap, namesakes (chosen names not spoken in the request, that is neither mentioned in it nor a speaker of it, that share a word with a spoken name or hold the word the shipped matcher offers for a misheard mention in it; median, maximum; reported, not bounded), spurious names (every other chosen name not spoken in the request; median, maximum) and prompt growth (estimated tokens with the names minus without; median, maximum). Recall: the share of injected mentions, whole and misheard apart, whose name the request holding them chooses with no cap (map and single requests). For every candidate cap C in {5, 10, 15, 20, 25, 30, 40, 50, 75, 100}: the injected mentions matched with no cap but dropped at C (map and single requests together).

### Rule

- **The cap** is the smallest candidate C that drops no injected mention matched with no cap. If none does, no cap is chosen and the owner decides.
- **The filter fails** (nothing ships until the owner decides) when any of:
  - whole-name recall with no cap is below 100% (map or single requests);
  - misheard recall with no cap is below 50% (map requests);
  - the median map request grows by more than 400 estimated tokens;
  - any request (map, single or merge) grows by more than 1,500 estimated tokens (the Dictionary block's own documented bound);
  - the median map request carries more than 3 spurious names;
  - no candidate cap is chosen.

The tool applies the rule itself (`rule_cap`, `rule_failures` in its report).

### Running it

```
cargo xtask fetch-fixtures --set ami-sum
cargo run --release -p earshot-core --example summary_names -- --annotations <EARSHOT_FIXTURES>\ami\annotations --out docs/summary-names/ami-dev.json
```

The report holds counts only: no transcript text and no name. `--dry-run` measures one synthetic meeting built in the tool instead, to show it works; its numbers mean nothing.

## Result

Not yet run.
