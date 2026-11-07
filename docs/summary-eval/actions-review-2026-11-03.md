# Action items by hand: Qwen 3.5 4B against 2B (T8.3l, 2026-11-03)

The owner asked for the time from hang-up to summary for the 4B and the 2B, and for a check by hand of which real action items each caught. A reworded item, or one that combines several references, counts as caught when the intent is the same.

## Setup

- **Host.** Surface (Snapdragon X2 Elite), CPU, 6 threads. One model at a time, nothing else running, so the times are clean.
- **Command.**

  ```
  cargo xtask sum-eval --backend cpu --split dev --source reference --model qwen3.5-4b-q4_k_m,qwen3.5-2b-q4_k_m --chunk-seconds 600 --threads 6 --meeting IS1008b,TS3006a,ES2011c --out target/sum-eval/t83l-solo
  ```

  This is the production setup: 600 s chunks, 16K context, the shipped prompt and grammar.
- **Meetings.** The dev meeting with the most AMI reference actions in three different series (the counts were read before any run): IS1008b (4), TS3006a (4), ES2011c (3).
- **How items were judged.**
  - **Caught**: same task and a right owner. One summary item may catch several references. Reworded is fine.
  - **Partly**: the right task, but the wrong owner.
  - **Missed**: no item with that intent.
  - Judged by Claude (this session), with every item listed below so the calls can be checked.

## Time from hang-up to summary

"Today" is the production flow: everything runs after the meeting. "Chunks during the meeting" is the last chunk plus the merge, the wait if the earlier chunks were summarised while recording (T8.3j).

| Meeting (audio) | 4B today | 4B, chunks during the meeting | 2B today | 2B, chunks during the meeting |
|---|---|---|---|---|
| ES2011c (27 min) | 661 s (11.0 min) | 366 s (6.1 min) | 263 s (4.4 min) | 130 s (2.2 min) |
| IS1008b (29 min) | 495 s (8.3 min) | 251 s (4.2 min) | **failed** after 173 s | n/a |
| TS3006a (21 min) | 521 s (8.7 min) | 159 s (2.7 min) | 219 s (3.7 min) | 82 s (1.4 min) |

The 2B is about 2.4 times faster. Its IS1008b run failed on the first chunk: the reply repeated an earlier item and was still repeating after two repair rounds. The run stopped with no summary. The 4B had no repairs on any meeting.

## Actions caught

| Meeting | References | 4B caught | 4B partly | 2B caught | 2B partly |
|---|---|---|---|---|---|
| ES2011c | 3 | 0 | 0 | 2 | 1 |
| IS1008b | 4 | 2 | 1 | 0 (failed) | 0 |
| TS3006a | 4 | 3 | 0 | 1 | 0 |
| **Total** | **11** | **5** | **1** | **3** | **1** |
| Total on the 7 actions of the two meetings both completed | 7 | 3 | 0 | 3 | 1 |

Summary items in total: 4B 17 (on 3 meetings), 2B 8 (on 2). Many items not in the AMI reference are plausible actions from the meeting that the annotator did not list. They are not counted against a model.

## Every item

### ES2011c

References:
1. The Project Manager instructed the User Interface Designer and the Industrial Designer to construct the prototype.
2. The User Interface Designer will work on the interface and the Industrial Designer will work on the look and feel of the device.
3. The Marketing Expert will prepare a prototype evaluation.

**4B: none caught.**
- [Project Manager] Look into the programming of how menus are shown on TVs. No reference.
- [Industrial Designer] Research how menus are displayed inside TVs versus remotes. No reference.
- [Marketing Expert] Contact relevant department to see the cost of covering the squashy bit. No reference.

**2B: 2 caught, 1 partly.**
- [User Interface Designer] Play with play-dough. **Caught 1.** The prototype was built from play-dough and the owner is one of the two. This is the most generous call.
- [User Interface Designer] Work on the look and feel of the design and user interface. **Caught 2** (combines both designers' parts, owner one of them).
- [User Interface Designer] Perform product evaluation. **Partly 3**: right task, but the reference owner is the Marketing Expert.
- [Project Manager] Finalize the design and user interface based on play-dough play and instructions from the coach. Overlaps 1 and 2 with the wrong owner; not counted again.
- [Project Manager] Coordinate with relevant departments to determine costs for covering the fruit and vegetable sections. No reference.

### IS1008b

References:
1. The Industrial Designer will make a presentation on the components concept for the product.
2. The User Interface Designer will make a presentation on the user interface concept.
3. The Marketing Expert will present the marketing concept.
4. The Project Manager will research companies that can provide the Industrial Designer with help on integrating voice recognition into her design.

**4B: 2 caught, 1 partly.**
- [Industrial Designer] Work on the components concept and user interface concept. **Caught 1; partly 2** (the UI concept is given to the wrong person).
- [Project Manager] Coordinate with the Marketing Expert to get technical details required to add the speech recognition feature. **Caught 4**: same owner, same intent of getting help to add speech recognition.
- [Industrial Designer] Find out the timeframe needed to develop the speech recognition feature. Related to 4, but not the same action.
- [Industrial Designer] Add unbreakable feature to outline design. No reference.
- [Project Manager] Send an email to the Marketing Expert regarding the project aim and details. No reference.
- [Marketing Expert] Provide feedback on the design and functional ideas. Not 3.
- [Marketing Expert] Go back to management and be more precise about what they want regarding risk and market potential. No reference.
- [Project Manager] Send minutes of the meeting to attendees. No reference.
- [All Attendees] Put all information in the sharing folder. No reference.
- Missed: 3 (the marketing concept).

**2B: failed, no summary.**

### TS3006a

References:
1. The interface designer should specify technical functions.
2. The marketing expert will come up with user requirements by doing internet research.
3. The industrial designer should produce a working design.
4. They will receive more information by email.

**4B: 3 caught.**
- [Industrial Designer] Produce a working design. **Caught 3.**
- [User Interface Designer] Specify the technical functions. **Caught 1.**
- [Marketing Expert] Come up with user requirements. **Caught 2**, together with the next item.
- [Marketing Expert] Conduct research on existing products, interview users, and search the internet for new functions. Part of 2.
- [Project Manager] Summarize the discussion and put it in the project folder. No reference.
- Missed: 4 (information by email).

**2B: 1 caught.**
- [Industrial Designer] Start the conceptual design phase. **Caught 3**, generously: same owner, design work.
- [User Interface Designer] Complete the animal drawing exercises on the smartboard. No reference (the kickoff's tool training).
- [Project Manager] Guide the team through the tool training and practice exercises. No reference.
- Missed: 1 and 2 (the two clearest assignments of the meeting) and 4.

## Reading

- **The 4B** caught 5 of 11 actions (plus 1 partly) and completed every meeting. Its wait at hang-up was 8 to 11 minutes today, or 3 to 6 minutes with chunks summarised during the meeting.
- **The 2B** is about 2.4 times faster: 4 minutes today, about 2 with chunks during the meeting. But it failed one meeting in three, and caught 3 of 11 (plus 1 partly). On the two meetings both completed they tie, 3 of 7 each. The 2B's ES2011c wins rest partly on generous calls, and on TS3006a it missed the two clearest assignments, which the 4B caught.
- **Three meetings is a small sample.** The failure is the strongest signal: a model that loses a whole meeting's summary one time in three is not usable as it stands. A larger run, or the repeat guard tuned for the 2B, would be needed before the 2B could be considered.
