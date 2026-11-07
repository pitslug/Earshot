# Summary prompt rounds for actions and decisions (T6.20e, 2026-11-04)

tune the shipped summary rules to catch more real action items and decisions, round after round, each round judged by hand. Everything up to "Round 0" was written before any round's summary was read.

## Setup

- **Host.** Desktop (x86_64-pc-windows-msvc, RTX 5080), llama.cpp on CUDA.
- **Meetings.** The five judged dev meetings: ES2003c, ES2011c, IS1008c, TS3004c, TS3006c. AMI reference transcripts (`--source reference`, no audio). The AMI test series is never run during the rounds.
- **Production setup.** 600 s chunks, 16K context, 8K reply ceiling, the shipped templates, grammar, repairs and loop guard.
- **Models.** Tuned on Qwen 3.5 4B (`qwen3.5-4b-q4_k_m`, the CPU and live-summary model). Qwen 3.5 9B (`qwen3.5-9b-q4_k_m`, the default on larger machines) runs on round 0 and on the final prompt as a check only.
- **Command per round** (one `--out` per round):

  ```
  cargo xtask sum-eval --model qwen3.5-4b-q4_k_m --split dev --source reference --chunk-seconds 600 --backend cuda --target-dir C:\eb\t620e --meeting ES2003c,ES2011c,IS1008c,TS3004c,TS3006c --prompt-variant <variant> --out target/sum-eval/t620e-r<N>-<variant>
  ```

- **Variants.** Round 0 is today's prompt (`current`, `templates/system.md`). Each later round is one new `PromptVariant`: a complete rules file in `crates/earshot-core/src/prompt/templates/variants/` with its snapshot. Its reason is written here before it runs. `system.md` is not edited during the rounds.

## Real items (the fixed lists)

Before round 1, each meeting's real actions and decisions are written from the reference transcript (not only from the AMI annotation), with segment numbers, and kept fixed for every round. They are in "Real items, per meeting" below.

- **Real action.** A task that a named person or role (or "everyone") takes on, or is given, to do after this meeting, including preparation for the next meeting. Owner as the transcript gives it. Routine meeting mechanics ("let's take a break", "I'll close the meeting") are not actions.
- **Real decision.** Something the group settled in this meeting: a choice agreed or accepted, including one stated by the person leading that nobody opposed. A proposal, an option argued for, or a question left open is not a decision. When a choice changed later in the meeting, the final one is the real decision.

### Borderline items (decided by the coordinator before round 0 was read)

The list writers (one Claude Code agent per meeting, reading the whole transcript, with the AMI list as a hint only) marked doubtful items "(borderline)". One rule for all of them: a borderline **decision** that the room accepted without objection is real (ES2003c D7, D8; ES2011c D2, D3, D4; TS3004c D3, D11, D14, D17; TS3006c D6, D7, D8). A borderline **action** (no owner given, a standing reminder, an in-meeting workaround or a passing "I'll check") counts neither way: catching it is "real, not listed" and missing it is not a miss (ES2003c A5; ES2011c A4, A5; TS3004c A4, A5). A joint task for two people is one real item: one summary item per person catches it once, and the owner is right when it names either person.

| Meeting | Real decisions | Real actions |
|---|---|---|
| ES2003c | 8 | 4 |
| ES2011c | 4 | 3 |
| IS1008c | 4 | 5 |
| TS3004c | 17 | 3 |
| TS3006c | 9 | 3 |
| **Total** | **42** | **18** |

### Real items, per meeting

### ES2003c
Decisions:
- D1: Power from conventional off-the-shelf batteries (the PM's summary names a lithium-ion battery as in small hand-held devices), not a hand dynamo, kinetic or solar cell [43, 45, 112, 117, 204, 205]
- D2: The case has a double-curved shape (so it suits left- and right-handed users) [139, 141, 206]
- D3: The case is made of soft, spongy rubber (stress-ball type), possibly over a firmer inner frame [120, 122, 176, 206, 207, 213, 329]
- D4: The remote is yellow, the colour of the company logo [217, 218, 219, 220, 223, 226, 332]
- D5: Use standard off-the-shelf components where possible (e.g. the infra-red transmitter) [20, 193, 204]
- D6: No number keypad: channels are reached through the LCD menu (sub-grouped by name) with the joystick [134, 193, 194, 197, 198]
- D7 (borderline): The menu system is customisable and offers several languages, as the product is international; PM agrees ("International would make sense") and nobody objects, but it is stated in passing at the time warning [305, 307, 308, 311, 312, 315]
- D8 (borderline): Target audience is broadly anyone under about sixty, with the core in the younger, single, free-cash bracket; stated by the PM in answer to a question and accepted by Marketing without discussion [279, 280, 283, 287]
Actions:
- A1 (Industrial Designer and User Interface Designer): work closely together on the detailed design and build a prototype in clay for the next meeting [261, 262, 315]
- A2 (Marketing Expert): work out how the product evaluation will be done once there is a model to test (user tests, field placement, street feedback) [271, 274, 276, 315]
- A3 (Industrial Designer): look further into the physical make-up of the remote [315]
- A4 (Project Manager): write up the minutes of this meeting [315]
- A5 (borderline) (design team): find a place for the company slogan on the remote, perhaps around the joystick; PM says "we always need to remember", a standing reminder more than an assigned task [337, 338]
AMI hints:
AMI A1 -> A1
AMI A2 -> A2
AMI D1 -> D1 (the meeting settled conventional batteries; lithium-ion appears only in the PM's closing summary as an example)
AMI D2 -> D2
AMI D3 -> D3
AMI D4 -> D4
Not decisions (left open): articulated joints (PM worried about weakness, UI Designer defended, unresolved [208, 213]); fruit-and-veg styling (split [321, 327]); USB/PC programming link (depends on cost [286, 299]); monkey-noise locator sound (proposal [227, 230]); "RR" logo on the top function button (proposal [340]).

### ES2011c
Decisions:
- D1: No screen on the remote: the TV acts as the screen, with a central button on the wheel bringing up the menu on the TV [98] [103] [104] [107] [146] [150]
- D2 (borderline): The handheld part is a spongy stress-ball rubber with a hard plastic face holding the wheel. Reason: proposed by ID [190] and PM [220], welcomed by UI/ID [193] [201] and then used as the working design [278] [293] [297], but never explicitly confirmed and ME said "I hate spongy" [196]
- D3 (borderline): Part of the remote is interchangeable so users can swap the fruit/vegetable look. Reason: everyone builds on it [251] [253] [297], but which part changes (plastic face or squishy part) is left open, with two prototypes suggested [253] [278]
- D4 (borderline): The fruit and vegetable trend shows up through colours or fruit designs on the changeable part, next to the company colours (silver and yellow). Reason: [213] [218] [220] [278] [284] point this way, but ME says "we don't know" where the fruit goes [277]
Actions:
- A1 (Industrial Designer and User Interface Designer): build a play-dough model of the design for the next meeting [342]
- A2 (Industrial Designer and User Interface Designer): work on the look and feel of the design and the user interface design [343]
- A3 (Marketing Expert): do the product evaluation [343]
- A4 (borderline, no owner): look into how the menu-on-TV programming is done. Reason: PM says "we have to look into" it [152] but gives it to nobody
- A5 (borderline, no owner): find costs for the changeable cover options and for the power options (battery vs dynamo). Reason: raised as needed [253] [317] [324] with no owner assigned
AMI hints:
- AMI D1 -> D2 (borderline: spongy material treated as the working design, not explicitly agreed)
- AMI D2 -> D4 (borderline: fruit colours with company colours discussed, placement left open at [277] [278])
- AMI D3 -> D3 (borderline: interchangeable part agreed in principle, which part left open)
- AMI A1 -> A1 (the transcript says "you two" will be "playing with play-dough" [342], i.e. building the model)
- AMI A2 -> A2 (the transcript does not split the roles; it gives "look and feel of the design and user interface design" to the two designers together [343])
- AMI A3 -> A3
Not included: batteries as the power source (PM proposed [316], ME and UI preferred a dynamo [317] [318], cost unknown: left open); the power button as a long press of the menu button [115] (proposal only); no DVD [56] (an earlier meeting's decision, restated here).

### IS1008c
Decisions:
- D1: The advanced chip on print is the chosen chip, carrying the infrared sender plus the integrated sensor/microphone and speaker (speech recognition and the beep locator) [32-33, 229-230]
- D2: The case is wood, customisable by the buyer (type and colour of wood, sized to the user's hand) [99-100, 230-233]
- D3: The buttons are rubber, not wood [234-237]
- D4: The prototype keeps the User Interface Designer's simple design without a display for now; a display is reconsidered only once the Marketing Expert has its cost [209-210, 238]
Actions:
- A1 (Industrial Designer and User Interface Designer): build the prototype model of the remote and bring it to the next meeting [238-241, 243, 251-252]
- A2 (Marketing Expert): find out how much adding a display would cost [222-224, 238, 244]
- A3 (Marketing Expert): prepare the marketing strategy, including how much extra could be sold [244-247]
- A4 (Industrial Designer): find out more about the technology behind the advanced chip on print before the next meeting [39-46]
- A5 (Project Manager): submit the prototype design to management once it is ready [243]
AMI hints:
AMI D1 -> generic (features decided); covered by D1-D4
AMI D2 -> partly real: customised wood case and ergonomic sizing = D2, advanced chip = D1 (its microphone and speaker give voice recognition and the locator), rubber buttons = D3, simple arrangement = D4; the battery life indicator is only part of the simple design adopted in D4, never settled on its own
AMI D3 -> D4
AMI D4 -> not real: the Project Manager deferred production ("we can talk about the production later") and manufacturing location was left to manufacturing to explore [196, 200-205]
AMI A1 -> A1
AMI A2 -> A2 (display cost) and A3 (marketing strategy)

### TS3004c
Decisions:
- D1: No LCD display on the remote (it would need the most expensive chip) [375, 377, 379, 380]
- D2: Speech recognition is included, final choice after the Project Manager first said to leave it out [669, 671, 704, 706, 708, 709, 710]
- D3: The remote uses the advanced chip (replacing the regular chip agreed earlier at [378, 412, 413]), needed for speech recognition [672, 674, 675] (borderline: stated as "probably we need" and accepted with "Fine", cost unknown)
- D4: Power comes from rechargeable batteries, not kinetic or solar power [346, 347, 348, 356, 407, 408]
- D5: The remote recharges in a docking station it slides into bottom first [342, 343, 524, 525, 526, 527, 538]
- D6: The case is hard plastic, not a soft/spongy rubber case and not titanium [365, 367, 391, 392, 393, 396, 397, 399, 411, 416]
- D7: Buttons are hard plastic and flush with the case (one level, mobile phone/iPod style), not rubber [271, 273, 276, 280, 284, 285, 598]
- D8: The case is curved in two dimensions only, not 3D curved (3D would force rubber buttons) [360, 361, 362, 363, 401, 403, 404]
- D9: The overall shape follows the iPod/MP3 player look with rounded corners, kept a bit square rather than a radical new form [548, 550, 553, 614, 649, 651, 652, 653]
- D10: Volume is controlled by a scroll wheel on the side; the scroll wheel is for volume only, not channels [161, 162, 164, 368, 371, 372, 373, 416, 449]
- D11: The scroll wheel is small and in yellow (Real Reaction colour) [628, 632, 635, 638, 639, 640] (borderline: Project Manager proposal accepted with "we could do that", no explicit close)
- D12: Button layout: frequently used channel switch buttons in the upper/middle area, number buttons at the bottom, little-used buttons such as teletext lower down, on-off stays on top [171, 172, 174, 178, 195, 468, 470]
- D13: Five changeable fronts are supplied with the remote, each carrying the logo [240, 241, 242, 247, 260, 518, 519]
- D14: The fronts/case are transparent so the backlight shows through [255, 262, 263, 296, 304, 306] (borderline: degree of transparency left open, "they can choose" [305])
- D15: Backlight LEDs in several colours (blue, red, green) [307, 310, 316, 323, 325]
- D16: The Real Reaction logo goes on the remote [226, 516, 517]
- D17: The remote opens up to hold the speech recognition functions [670, 671, 672] (borderline: brief question answered "Yeah" in the closing minute)
Actions:
- A1 (User Interface Designer and Industrial Designer): draw the prototype design together on the smart board in the next meeting [425, 426, 430, 431, 432]
- A2 (User Interface Designer): decide the arrangement of the channel up/down buttons (above and below vs side by side) [489, 494, 499, 500]
- A3 (User Interface Designer): look for more rounded design examples (iPod, MP3 players, mobile phones) for the shape [435, 436, 438, 439]
- A4 (User Interface Designer): prepare the drawing by opening images and painting them, since the smart board pen does not draw properly [641, 643, 644, 645, 646] (borderline: arose as a workaround to "ask her" about the board; unclear if it is for after the meeting)
- A5 (Industrial Designer): check which chip type is needed [412, 415] (borderline: a passing "I'll just check it", and the chip choice changed later in the meeting)
AMI hints:
AMI A1 -> A1
AMI A2 -> A2 (placement of the channel buttons is what was left to the User Interface Designer)
AMI D1 -> D10
AMI D2 -> D11 (it is the scroll wheel that is small and yellow, not the remote)
AMI D3 -> D4 and D5
AMI D4 -> D6 and D7
AMI D5 -> D1
AMI D6 -> D8 and D9 (the transcript says curved in 2D only with rounded corners, "a bit square"; "double-curved" matches 2D curved, not 3D)
AMI D7 -> D13
AMI D8 -> D2 and D3

### TS3006c
Decisions:
- D1: The energy source is kinetic (shake to power), not batteries or solar [100, 109-116, 170, 182]
- D2: The case is double curved (the single-curved compromise the Project Manager raised was dropped) [170, 182, 270-288]
- D3: The case material is rubber [170-173]
- D4: The remote uses the advanced chip (needed for the screen and audio functions) [175-179]
- D5: Speech commands are matched to the existing functions, for both the simple and the advanced options, with no separate design [137, 142-144]
- D6: The buttons are red by default (borderline: settled by "so red buttons are okay?" / "I think so" / "you can make them red" after the colour-blindness point, not formally confirmed) [544-560]
- D7: Buttons are see-through plastic with one coloured LED behind each, so the whole button lights in its own colour (borderline: proposed by the Industrial Designer and accepted with "yeah" by all, no explicit confirmation) [571-581]
- D8: No flip-up screen: the LCD is integrated into the body, set a bit lower with rubber around it so a drop does not hit the screen (borderline: the flip was first kept at [737-741], then reversed by majority at the end with "most votes count") [756-768]
- D9: The product is named "the Real Remote", with a copyright sign after "Real" [782-790]
Actions:
- A1 (Industrial Designer and User Interface Designer): design the prototype look-and-feel together on the smartboard in the thirty minutes after the meeting, to show to management [589, 606, 613-621, 777]
- A2 (Marketing Expert): do the product evaluation, for now preparing an evaluation procedure to follow [593-608]
- A3 (Industrial Designer and User Interface Designer): include the Real Remote name (with the copyright sign) in the design [787, 794]
AMI hints:
AMI A1 -> A1
AMI A2 -> A2
AMI D1 -> D1
AMI D2 -> D2 and D3 (shape and material are separate decisions)
AMI D3 -> D4
Not included (considered): blue or white display backlight [475-487] left open ("you should test it"); body base colour (dark blue with fruit print [416-453] vs bright background [507-515]) never settled; flashing standby light [455-463] only dismissed by the Project Manager as too much; one ergonomic shape rather than left- and right-handed versions [371-394] only a passing "we should better choose one"; pushbuttons for arrows [94-97] was decided in an earlier meeting; picture of the double-curved case to the project folder [203-206] and saving the board [631-634] are in-meeting mechanics.

## Judging rules

Every decision and action item in every summary is judged against the transcript and the fixed list.

- **Caught.** The summary item states a real item with the same intent. Reworded counts; one summary item may catch several real items. A real item is caught at most once; a second summary item for it is a duplicate (neither caught nor made up).
- **Owner right** (actions only). The summary owner is the person the transcript gives the task to. A caught action with the wrong owner still counts as caught; its owner is counted wrong.
- **Made up.** The summary item is not supported by the transcript: invented, the opposite of what was said, a proposal or open question written as settled, or a task nobody took on.
- **Real, not listed.** Supported by the transcript but not on the fixed list (a minor point, a restatement, a mechanic). Neither caught nor made up; listed so the reader can check the list.
- **Missed.** A real item no summary item catches.
- A decision written as an action item (or the other way round) catches the real item if the intent matches; the list it appeared in is noted.
- An item that catches a real item but also states a false part (for example "rubber with a single-curved design" when the case is double curved) is caught for the true part and also counts one made up.
- Judges are Claude Code agents, one per meeting, kept for every round (written before round 0 was judged): each reads its transcript and fixed list once and judges every round's summary of its meeting, so verdicts stay consistent; an item with the same meaning as one judged in an earlier round keeps its earlier verdict. The judge sees no prompt or model name, only a run label. The coordinator checks every verdict listed below.

## Rule for a better round

A round is better than the best round so far only if real actions plus real decisions caught rises, made-up items do not rise, and every summary is valid. Stop after two rounds in a row with no gain, or after round 6. The winner is the best round; its prompt runs once on the AMI test series (automatic scores only), and the owner decides whether it replaces `system.md`.

## Results per round

Caught counts real items (42 decisions, 18 actions). "Owner right" counts caught actions. "Not listed" and duplicates are neither caught nor made up. Judge labels: X1 = round 0 on the 4B, X2 = round 0 on the 9B, X3 = round 1, X4 = round 2, X5 = round 3, X6 = round 4, X7 = the winner on the 9B (all later runs on the 4B unless marked).

| Round | Prompt | Model | Valid | Decisions caught | Actions caught (owner right) | Caught total | Made up | Not listed | Duplicates |
|---|---|---|---|---|---|---|---|---|---|
| 0 | `current` | 4B | 5/5 | 25/42 | 10/18 (7) | 35 | 24 | 19 | 3 |
| 0 | `current` | 9B (check) | 5/5 | 27/42 | 10/18 (8) | 37 | 19 | 16 | 5 |
| 1 | `assignments` | 4B | 5/5 | 20/42 | 14/18 (11) | 34 | 14 | 7 | 3 |
| 2 | `settled` | 4B | 5/5 | 25/42 | 12/18 (9) | 37 | 10 | 7 | 4 |
| 3 | `handed` | 4B | 5/5 | 24/42 | 11/18 (9) | 35 | 16 | 13 | 2 |
| 4 | `checked` | 4B | 5/5 | 22/42 | 10/18 (8) | 32 | 16 | 6 | 5 |
| final | `settled` | 9B (check) | 5/5 | 27/42 | 13/18 (11) | 40 | 15 | 9 | 7 |

### Round 0: today's prompt

Out directory `target/sum-eval/t620e-r0-current` (both models in one run).

What goes wrong on the 4B, read from the verdicts below:

- **Made-up decisions** are mostly proposals or open points written as settled (articulation, base colour, backlight, two prototypes) and choices the meeting later reversed (the regular chip, keeping the flip screen, a single curve).
- **Made-up actions** are suggestions nobody took on, or questions raised in passing.
- **Not-listed actions** (19 items on the 4B) are mostly things done during the meeting: presenting slides, showing a drawing.
- **Missed actions** are the closing assignments: all three of ES2011c ("you two" build the model, the designers do the look and feel, the Marketing Expert the evaluation), and ES2003c's prototype and minutes.
- **Owners** go to the speaker of the line: ES2003c's prototype and evaluation are given to the Project Manager.

### Round 1: `assignments` (reason written before the run)

Rule 5 only. An action item is a task taken on or given during the meeting to do after it, including preparation for the next meeting; look where people say who does what next, often near the end ("you two will ...", "for the next meeting"); not something done during the meeting, a suggestion nobody took on, or an open question. The owner is the person addressed or named, not the speaker of the line; a task for several people is one item naming them all (not one item per person, which T6.20b's `lean` linked to a reduce-step repetition failure). Why: round 0's action failures are the closing assignments missed, in-meeting activity written as actions and owners taken from the speaker. Expected risk: fewer action items written, and a longer system message for a 4B model.

### Round 1 result

Out directory `target/sum-eval/t620e-r1-assignments`. **Not better than round 0** (34 caught against 35). Actions rose from 10 to 14 (owners right 7 -> 11): every closing assignment of ES2011c and IS1008c was caught, and IS1008c had every owner right. Made-up items fell from 24 to 14, mostly fewer invented actions. Decisions fell from 25 to 20, nearly all on TS3004c (12 -> 6): the summary wrote 6 decisions, several bundling choices, against 11 in round 0. The rule did what it was aimed at; the loss is on a list it did not touch.

Coordinator ruling while judging round 1 (applies to every later round): a real action is credited to the summary item that catches it with an owner. A decision-list item that restates an action without an owner is then "not listed", not a catch with a wrong owner (TS3006c S-D9).

### Round 2: `settled` (reason written before the run)

Round 1's rules plus a new rule 6 on decisions: list every choice the group settled, one decision per choice (material, shape, colour, power source, chip, each feature kept or dropped), each a short sentence naming what was chosen; a choice stated by the person leading that nobody objected to counts; proposals, options argued for and questions left open are not decisions; when a choice changed later in the meeting or in a later partial summary, keep only the final one. Why: round 1 kept the action gains but lost decisions on TS3004c by bundling; round 0's made-up decisions are proposals written as settled and choices reversed later (the regular chip, keeping the flip screen, a single curve), which the reduce step carried from an earlier chunk. Expected risk: T6.20b's `decisions` rule cost the 9B decisions ("settled" read too strictly); this one asks for every choice first, then excludes proposals.

### Round 2 result

Out directory `target/sum-eval/t620e-r2-settled`. **Better than round 0, the new best** (37 caught against 35; made up 10 against 24; 5/5 valid). Decisions are back to 25 (ES2003c 6, TS3004c 9 against round 1's 6) and the made-up decisions fell (ES2003c's articulation and USB link gone). Actions 12 (round 1: 14): TS3006c lost the Marketing Expert's evaluation procedure. Still wrong:

- **Owners given to the meeting lead.** ES2003c's product evaluation (and the physical make-up) go to the Project Manager in every round so far, though the Project Manager hands them to others at [315].
- **Choices reversed later are still written as settled**: TS3004c's regular chip, TS3006c's single curve and the kept flip screen. The rule to keep the final choice did not reach them.
- **Decisions missed on TS3004c** (9 of 17): mostly what the remote will not have (no LCD) and add-ons (changeable fronts, transparent case, coloured LEDs, logo).

### Round 3: `handed` (reason written before the run)

Round 2's rules plus two sentences. In rule 5: when the person leading the meeting hands out tasks, each task belongs to the person it is handed to; the leader gets only what they say they will do themselves. In rule 6: what the product or plan will not have (for example "no screen") is a decision too. Why: the two remaining failure kinds that a rule can name without naming a meeting. Expected risk: more items written, so more chances of a made-up one.

### Round 3 result

Out directory `target/sum-eval/t620e-r3-handed`. **Not better than round 2** (35 caught against 37; made up 16 against 10). First round in a row without a gain. The lead-owner sentence did not move ES2003c's product evaluation (still the Project Manager's); TS3004c caught "no LCD" (the "will not have" sentence) but lost its prototype drawing action, and in-meeting presentations came back as actions (13 not listed against 7). Made-up decisions rose on TS3006c (flashing standby lights, base colour, backlight) and IS1008c (a production tie-up). Two sentences more did not help a 4B model; the variation between rounds on the same meeting (TS3004c actions 1-2, TS3006c actions 1-3) is as large as the effect of a sentence.

### Round 4: `checked` (reason written before the run)

Round 2's content, without round 3's sentences, restated more tightly (about 90 tokens shorter than `settled`): each of rules 5 and 6 says what to include, then what to leave out ("Leave out things done during this meeting (presenting, showing, discussing), suggestions nobody took on, and open questions"; "Leave out proposals, options someone argued for, and questions still open"), with one example of an owner ("you two build the model" belongs to the two people addressed) and an explicit check: "Before writing a decision, check the rest of the transcript (or the later partial summaries): when the choice was changed or reversed later, write only the final choice." "Each feature kept, added or left out" replaces round 3's separate sentence. Why: round 2's gains came from the include/exclude definitions, and round 3 showed that added sentences dilute them; a shorter, list-shaped rule is easier for a 4B model to follow. Aimed at the made-up decisions that remain (reversed choices, proposals) and at in-meeting actions.

### Round 4 result

Out directory `target/sum-eval/t620e-r4-checked`. **Not better than round 2** (32 caught against 37; made up 16 against 10). Second round in a row without a gain, so the rounds stop here (the stop rule, before round 6). The tighter wording lost decisions on ES2003c (4) and ES2011c (0) and did not remove the reversed choices (TS3004c's regular chip, TS3006c's kept flip screen and single curve) or the proposals written as settled (base colour, backlight).

## Winner: round 2, `settled`

Best round under the rule: 37 real items caught on the 4B against 35 for today's prompt, with 10 made up against 24, every summary valid.

**What changed in the prompt** (`templates/variants/settled.md` against `templates/system.md`; everything else is identical):

- **Rule 5, action items**, rewritten. Before: "`owner` is the speaker's name as it appears in the transcript, or "Unassigned"". After: an action item is a task someone took on or was given in this meeting to do after it (including preparation for the next meeting); look where people say who does what next, often near the end; not what was done during the meeting, a suggestion nobody took on, or an open question; the owner is the person addressed or named, not the speaker of the line; a task for several people is one item naming them all; "Unassigned" when nobody is named.
- **Rule 6, decisions**, new: every choice the group settled, one per choice (material, shape, colour, power source, chip, each feature kept or dropped), each a short sentence naming what was chosen; a choice the leader stated that nobody opposed counts; proposals, options argued for and open questions are not decisions (notes or `open_questions`); when a choice changed later, only the final one.
- Rules 6 and 7 of today's prompt become 7 and 8, unchanged. The system message grows by about 460 estimated tokens (1576 -> 2040 in the general snapshot).

**9B check** (out directory `target/sum-eval/t620e-final-settled-9b`): 40 caught against 37 for today's prompt on the 9B, made up 15 against 19, 5/5 valid. The gain on the 9B is all in actions (10 -> 13, owners right 8 -> 11); decisions are level at 27.

**Where it still fails, on both models:** choices reversed later in the meeting are still written as settled (TS3004c's regular chip; on the 4B also TS3006c's flip screen and single curve), proposals still become decisions (TS3006c's base colour and backlight, ES2003c's articulation), and ES2003c's product evaluation goes to the Project Manager in every run of every round. Two rounds aimed at these (3 and 4) did not move them.

**Reading the margin.** Two items out of 60 on the 4B is within the variation between rounds on the same meeting (TS3004c actions 1-2, TS3006c actions 1-3 across rounds with the same action rule). The fall in made-up items (24 -> 10 on the 4B, 19 -> 15 on the 9B) is the larger and steadier effect: every round with the new action rule wrote far fewer in-meeting or invented actions.

## Test series, once, for the winner (automatic scores only)

Reference source, the 20 AMI test meetings, 600 s chunks, CUDA, `--prompt-variant settled`, both models in one run (`target/sum-eval/t620e-settled-test`). Nothing was tuned on it. The current rows are T6.20's stored chunked test run (`chunk-test`, 2026-10-25, same format key without the variant), not a re-run on this build. Items are caught / reference items under the strict, reference-recall and coverage rules where stored; T6.20 stored only coverage for the 4B.

| Model | Prompt | Valid | ROUGE-L | ROUGE-Lsum | Decisions s / rr / cov (of 94) | Actions s / rr / cov (of 30) | Repairs | Loop-guard stops | At hang-up, s per meeting |
|---|---|---|---|---|---|---|---|---|---|
| 4B | current (`chunk-test`) | 20/20 | 0.151 | 0.274 | - / - / 17 | - / - / 11 | - | - | 18.2 |
| 4B | settled | 20/20 | 0.150 | 0.277 | 7 / 8 / 16 | 6 / 6 / 7 | 0 | 0 | 16.3 |
| 9B | current (`chunk-test`) | 20/20 | 0.166 | 0.300 | 6 / 10 / 21 | 10 / 6 / 10 | 0 | 0 | 22.3 |
| 9B | settled | **19/20** | 0.161 | 0.293 | 13 / 21 / 34 | 10 / 7 / 9 | 2 | 3 | 19.7 |

- **One invalid summary on the 9B.** TS3007b failed in a map step after 3 requests: "repeats an earlier item of the same list; write each item once" (the loop guard stopped it; the repairs did not remove the repeat). It is the second test-series failure of this kind after T6.20b's `lean` (ES2014a, a reduce step), and both prompts carry an action rule of this shape: the rule seems to make the 9B write long lists that repeat.
- **The automatic action scores do not confirm the dev gain.** On the 4B, action coverage falls from 11 to 7 of 30 (AMI's own action list, token overlap); on the 9B it is level (10 -> 9). These rules count token overlap with the annotator's wording, so a reworded or more precise item can score lower; the hand judging on dev is the measure this task set, and it cannot be repeated on test without a judged list.
- **Decisions rise on the 9B** (coverage 21 -> 34 of 94, reference recall 10 -> 21) and are level on the 4B (17 -> 16).
- ROUGE-L is level on both (-0.001, -0.005).

## Reading for the owner

- On dev, judged by hand, `settled` catches more real actions and decisions than today's prompt on both models (4B 35 -> 37, 9B 37 -> 40) and writes far fewer made-up items (4B 24 -> 10, 9B 19 -> 15). The catch margin is small (2-3 of 60); the drop in made-up items is not.
- On test, automatic only: no gain on actions, a gain on decisions for the 9B, and one failed 9B summary from repetition, the same failure that kept T6.20b's `lean` out.
- `system.md` and the default are unchanged. Adopting `settled` as it stands would trade fewer invented items for a repetition risk on the 9B; a follow-up could look at why the action rule makes the 9B repeat (dev data only) before any switch.

## Decision

The owner adopted `settled` as the production prompt (T6.20f): `system.md` now holds its rules, the `settled` variant is gone, and the earlier production rules are `--prompt-variant previous`. The sum-eval format key is `indented-numbered-r5`, so runs from before the switch are never reused. The 9B repetition risk on the test series stands as measured.
## Every item

Each run's summary items (S-D decisions, S-A actions, in summary order) with the judge's verdict, then the real items missed and the counts. Segment numbers are the reference transcript's.

### X1 (round 0, current, 4B)

#### ES2003c
- S-D1: caught D1. ID proposes conventional batteries over kinetic/solar [43], PM agrees [117, 204-205].
- S-D2: caught D2, D3. Rubber (UI and ME [120, 122], PM summary [206]) and double-curved shape [139, 141, 206] both settled.
- S-D3: made up. Articulation was proposed by the UI Designer and questioned by the PM as a weak point [208, 213]; it was left open, not confirmed. (The left/right-handed point is already caught by S-D2's D2.)
- S-D4: not listed. Moving to the detailed design phase is the meeting schedule [258, 260], an in-meeting mechanic.
- S-D5: caught D4. Bright yellow [217-226, 332]; the spongy rubber part repeats D3 already caught by S-D2, colour options are a hedged UI suggestion [332].
- S-D6: not listed. ME and UI say it will need on-board/flash memory [302, 305]; supported but not on the list.
- S-D7: caught D7 (borderline decision). Customisable menu, languages and skins, international [305-315].
- S-A1: not listed. Presenting the components slides is an in-meeting mechanic [6].
- S-A2: made up. The PM asked the ID in the meeting how feasible an LCD is [110]; nobody took on an evaluation task.
- S-A3: not listed. PM says "we have to look into the costs" of bulk buying [99]; supported but minor and not on the list.
- S-A4: not listed. Working between slides and whiteboard is the UI Designer's in-meeting presentation [129-132], not a follow-up task.
- S-A5: not listed. "I can add pretty pictures to this" is an in-meeting presentation remark [150].
- S-A6: not listed. Presenting the trend-watching slides is an in-meeting mechanic [170].
- S-A7: caught A2, A1. Product evaluation plan matches A2 and the clay prototype matches A1 [261-262, 274-276, 315]; owner wrong for both (A2 is the Marketing Expert's, A1 the Industrial and UI Designers'; the PM's own task was the minutes).
- S-A8: made up. Fruit-and-veg styling was split and left open [321-327]; the UI Designer's preference is written as an assigned task.
- S-A9: made up. The PM says the USB link depends on cost and may be prohibitive [286, 299]; an open question, no one took on an evaluation.
Missed: D5, D6, D8, A3, A4
Counts: decisions caught 5/8, actions caught 2/4 (owner right 0), made up 4, not listed 7, duplicates 0

#### ES2011c
- S-D1: made up. ME's tentative "for now we can do two prototypes maybe" [253] is a suggestion written as a decision. Nobody commits to it, and the meeting ends by giving the designers a play-dough model [342]. It does not state D3's intent (an interchangeable part for the fruit look).
- S-A1: made up. ME's presentation says the Industrial Designer "should look into" the spongy feel [38], but ID never takes it on as a task. ID answers it in the meeting with the stress-ball rubber [190], and the closing assignments [342] [343] do not include it.
- S-A2: made up. Same meaning as S-A1 (ME's "come up with a solution that's better than mine" [38]). It is a suggestion nobody took on, answered in the meeting [190].
- S-A3: not listed. This catches borderline A4 (look into the menu-on-TV programming [152]). PM says "we have to look into" it and gives it to nobody, so the Project Manager owner is not in the transcript.
- S-A4: not listed. This catches borderline A5 (costs for the changeable cover [253]). ME says "we can just contact our relevant department" with no owner assigned.
Missed: D1, D2, D3, D4, A1, A2, A3
Counts: decisions caught 0/4, actions caught 0/3 (owner right 0), made up 3, not listed 2, duplicates 0

#### IS1008c
- S-D1: caught D2. Wood is agreed for the case [29-35, 230-233]; customisation not stated but the intent (wooden case) matches.
- S-D2: caught D4. PM: stick with the simple design until Ed finds out what the display costs [238].
- S-D3: caught D3. Buttons rubber, not wood [234-237].
- S-A1: caught A4, owner right. Industrial Designer offers to find out more about the advanced chip on print before the next meeting [42-46].
- S-A2: not listed. The consumer-trends presentation is given in this meeting (in-meeting mechanic), not a task for later [133-163].
- S-A3: not listed. The trend watch slide is shown in this meeting (in-meeting mechanic) [139-147].
- S-A4: made up. The Industrial Designer raised labour laws as a consideration, but said manufacturing would explore it and the PM deferred production; nobody took on the task [189-205].
- S-A5: caught A2, owner wrong. The display cost task goes to the Marketing Expert (Ed); the Industrial Designer only asked him to take it [222-224, 238, 244].
- S-A6: caught A1, owner right. PM asks both designers to come up with the prototype; the User Interface Designer is one of them [238-241, 251-252].
- S-A7: caught A3, owner right. Marketing Expert to prepare the marketing strategy [244-247]; its display price part restates A2, already caught by S-A5.
Missed: D1, A5
Counts: decisions caught 3/4, actions caught 4/5 (owner right 3), made up 1, not listed 2, duplicates 0

#### TS3004c
- S-D1: caught D14 + made up (soft primary colours as settled). Transparent case matches [255, 262]; "soft, primary colours" is the ID's web finding [212], soft material was rejected [391-397] and colours were never settled (black/yellow debated [226-239]).
- S-D2: caught D7. Buttons on one level [273, 276, 280]; top/side placement is a loose restatement.
- S-D3: caught D16, D13. Real Reaction logo on the remote in its black/yellow colours [226]; changeable fronts [240-242, 260], hedged as "an option" but same intent.
- S-D4: caught D6, D8, D4. Hard plastic [396-397], 2D curved [361-363, 401], rechargeable batteries [346-348, 407].
- S-D5: made up. The regular chip was agreed at [378, 412-413] but replaced by the advanced chip for speech recognition [672-675]; stated as the final choice it is the opposite of the outcome. Does not state the no-LCD decision.
- S-D6: caught D12. Numbers at the bottom, channel switch in the middle [468, 470].
- S-D7: caught D9. Rounded corners, iPod-like look [548-553, 652].
- S-D8: caught D11 + made up (an LED display). Yellow scroll wheel [628, 632]; there is no display (LCD rejected [375-377]), only backlight LEDs [508].
- S-D9: caught D5. Docking station recharger [524-527, 538].
- S-D10: made up. Nobody took on investigating speech recognition cost; the meeting decided to include it [708-709] with cost left unknown.
- S-D11: caught D2. Speech recognition included [708-710].
- S-A1: not listed. In-meeting mechanic: ID presenting the material options he received [18, 83].
- S-A2: not listed. UID said he would check what the technology department meant [104] and reported it in the meeting [688-693].
- S-A3: not listed. In-meeting mechanic: watching the next presentation [103].
- S-A4: not listed. In-meeting mechanic: UID's design presentation already given [133-198].
- S-A5: not listed. In-meeting mechanic: ID's component design presentation [210-250].
- S-A6: made up. [208] says the team must decide the look within this meeting; no task to finalise a presentation.
- S-A7: not listed. Matches borderline A5 (ID checks the chip [415]).
- S-A8: made up. [439] is UID agreeing to look for rounded examples; nobody took on reviewing scroll wheel placement on the smart board.
- S-A9: caught A2, owner right. Channel button arrangement left to UID [499-500].
- S-A10: made up. "I'll see it" [650] is a passing reply; no task to confirm originality against the iPod.
- S-A11: made up. [641-643] is about asking "her" whether the smart board pen can work better, and the PM told UID to ask; nothing about the scroll wheel or an engineering team.
- S-A12: made up. Nobody took on investigating the advanced chip's cost; ID only said he lacked the information [684-687].
Missed: D1, D3, D10, D15, D17, A1, A3
Counts: decisions caught 12/17, actions caught 1/3 (owner right 1), made up 9, not listed 6, duplicates 0

#### TS3006c
- S-D1: caught D1. Kinetic energy chosen [109-116, 170, 182].
- S-D2: caught D3 + made up (single-curved). Rubber is right, but the single-curved compromise [270-282] was dropped for double curved [284-288].
- S-D3: caught D5. Speech commands for both the simple and advanced options [142-144].
- S-D4: made up. Hiding okay/back/menu under a flip screen was a PM proposal [338], and the flip itself was dropped at the end [756-768].
- S-D5: not listed. "We should better choose one shape" [390-394] is supported but only a passing remark; not on the list.
- S-D6: made up. The base colour was never settled: dark blue [416-453] was challenged by a bright background [507-515].
- S-D7: made up. The backlight was left open ("you should test it" [487]); white was only suggested [483-486].
- S-D8: caught D6. Red buttons by default [557-560]; matching the LED colour to the button [571] is consistent. Does not state the see-through button with an LED behind it (D7).
- S-D9: made up. Keeping the flip [737-741] was reversed later ("no flipping", "most votes count" [756-768]).
- S-D10: caught D8. LCD integrated into the body instead of flipping [727-730, 756].
- S-D11: duplicate. Same as S-D10 (D8, no flip [756-768]).
- S-D12: made up. Side volume/scroll buttons were floated after the meeting ended [806-817] and left uncertain ("we kind of wanted to stick with the...").
- S-A1: not listed. Putting a picture in the project folder [203-206] is an in-meeting mechanic.
- S-A2: caught A1, owner right. ID and UID design the look-and-feel on the smartboard in the next thirty minutes [589, 606, 614].
- S-A3: duplicate. Per-person copy of A1 for the UID.
- S-A4: caught A2, owner right. Marketing Expert prepares an evaluation procedure [593-601].
- S-A5: caught A3 + made up (side buttons), owner right. Include the copyright sign/name in the design [787, 794]; the "new side buttons" were never settled [806-817].
- S-A6: duplicate. Per-person copy of A3 for the UID.
Missed: D2, D4, D7, D9
Counts: decisions caught 5/9, actions caught 3/3 (owner right 3), made up 7, not listed 2, duplicates 3

### X2 (round 0, current, 9B)

#### ES2003c
- S-D1: caught D1. Conventional batteries over kinetic/solar [43, 112, 117, 204-205]; the advanced chip is the stated consequence of the LCD [116], supported.
- S-D2: caught D3. Rubber case, UI and ME [120, 122], PM summary [206].
- S-D3: caught D2 + made up (articulated). Double curve for left and right hands is settled [139, 141, 206]; articulation was questioned by the PM and left open [208, 213].
- S-D4: caught D6. No number keypad, channels through the LCD menu [134, 193, 197-198].
- S-D5: made up. The PC programming link was the UI Designer's proposal; the PM says it depends on cost and may be prohibitive [286, 299], left open.
- S-D6: not listed. A joke about the drawing [213-215]; supported but trivial.
- S-D7: caught D4. Yellow to match the logo [217-226].
- S-D8: duplicate (D3, D4). Spongy rubber and bright yellow restate D3 (S-D2) and D4 (S-D7); colour options are a hedged UI suggestion [332].
- S-D9: caught D7 (borderline decision). Customisable menu with languages and skins, international [305-315].
- S-A1: not listed. Working between slides and whiteboard is the UI Designer's in-meeting presentation [129-132].
- S-A2: not listed. "I can add pretty pictures to this" is an in-meeting presentation remark [150].
- S-A3: caught A2. Working out the product evaluation [271-276, 315]; owner wrong (the Marketing Expert's task, not the PM's; the PM's own write-up was the minutes).
- S-A4: made up. The UI Designer asked whether clay will be supplied [317]; nobody, the PM included, took on finding it.
Missed: D5, D8, A1, A3, A4
Counts: decisions caught 6/8, actions caught 1/4 (owner right 0), made up 3, not listed 3, duplicates 1

#### ES2011c
- S-D1: caught D3 + made up (two prototypes as a settled plan). The squishy part as changeable matches D3's interchangeable part [251] [253] [297]. ME's "for now we can do two prototypes maybe" [253] is a tentative suggestion written as a decision (same verdict as the X1 two-prototype item).
- S-D2: made up. PM proposes batteries [316], but ME and UI prefer a dynamo [317] [318] and costs are unknown [322] [324]. It is left open, not decided.
- S-A1: not listed. This catches borderline A4 (look into the menu-on-TV programming [152]). PM says "we have to look into" it and gives it to nobody (same as X1 S-A3).
- S-A2: not listed. This catches borderline A5 (cover costs [253]). ME says "we can just contact our relevant department" with no owner (same as X1 S-A4).
- S-A3: caught A2, owner right. PM gives the look and feel and the user interface design to the two designers [342] [343].
- S-A4: duplicate. A2 was already caught by S-A3 (joint task for ID and UI).
- S-A5: caught A3, owner right. "you're gonna be doing product evaluation" [343] goes to the Marketing Expert, the one person not in "you two".
- S-A6: made up. The User Interface Designer is one of "you two" on the model and design [342] [343]. Product evaluation goes to the Marketing Expert, so this owner is false (and A3 is already caught by S-A5).
Missed: D1, D2, D4, A1
Counts: decisions caught 1/4, actions caught 2/3 (owner right 2), made up 3, not listed 2, duplicates 1

#### IS1008c
- S-D1: caught D4. PM: stick with the simple design until Ed finds out what the display costs [238].
- S-D2: caught D3. Buttons rubber like now for the tactile experience [234-237].
- S-A1: caught A4, owner right. Industrial Designer offers to find out more about the advanced chip on print before the next meeting [42-46].
- S-A2: made up. The Industrial Designer raised labour laws as a consideration, but said manufacturing would explore it and the PM deferred production; nobody took on the task [189-205].
- S-A3: not listed. Finding the trend watch slide happens in this meeting (in-meeting mechanic) [139-147].
- S-A4: caught A2, owner wrong. The display cost task goes to the Marketing Expert (Ed); the Industrial Designer only asked him to take it [222-224, 238, 244].
- S-A5: caught A1, owner right. PM asks both designers to come up with the prototype; the User Interface Designer is one of them [238-241, 251-252]. "Wooden case" is incidental here and does not state the case decision D2.
- S-A6: caught A3, owner right. Marketing Expert to prepare the marketing strategy [244-247].
Missed: D1, D2, A5
Counts: decisions caught 2/4, actions caught 4/5 (owner right 3), made up 1, not listed 1, duplicates 0

#### TS3004c
- S-D1: caught D14, D13. Transparent case [255, 262] and changeable coloured fronts [240-242, 260].
- S-D2: caught D7. Hard plastic buttons flush with the surface, not rubber [267, 271, 273, 284].
- S-D3: caught D12. Frequently used buttons on top, little-used ones such as text TV lower down [171-174].
- S-D4: caught D6, D8. Hard plastic case [396-397, 411], curved in 2D only [401].
- S-D5: caught D10. Volume on a scroll wheel on the side [368, 371, 449].
- S-D6: caught D4. Rechargeable batteries [346-348, 407].
- S-D7: made up. The regular chip [378, 412-413] was replaced by the advanced chip for speech recognition [672-675]; stated as final it is the opposite of the outcome (same as X1 S-D5).
- S-D8: caught D15. Backlight LEDs in several colours [316, 323, 325]; "changeable" is loosely supported by multi-colour LEDs [316, 320].
- S-D9: made up. Channel up above and down below the numbers was only the ID's proposal [475], disputed [476-494] and left to the UID [499-500]; there is no number display.
- S-D10: caught D9 + made up (avoid resembling the iPod). Rounded corners, kept a bit square [548-553, 614]; the team agreed "more like iPod" [649-652], not to avoid it.
- S-D11: caught D11. Small yellow scroll wheel [628, 638-640]; lights on the bottom are the PM's remark [628].
- S-D12: caught D2, D3. Speech recognition included [708-710] with the advanced chip [672-675]; microphone mentioned [659].
- S-A1: not listed. In-meeting mechanic: ID presenting the material options he received [18] (same as X1 S-A1).
- S-A2: not listed. UID checked what the technology department meant [104] and reported it in the meeting [688-693] (same as X1 S-A2).
- S-A3: not listed. UID's "I'll get back on it" [58, 91] was answered in the meeting [99, 688-693].
- S-A4: not listed. In-meeting mechanic: ID giving material information in his presentation [83].
- S-A5: made up. Button top/bottom placement was settled by the group [171-195]; nobody gave the UID a task to finalise the layout ([191] is a general "we have to look").
- S-A6: made up. The soft case was rejected [391-397]; nobody took on researching materials.
- S-A7: not listed. [205-208] is the team deciding the speech recognition question within this meeting, which it did [708].
- S-A8: not listed. Matches borderline A5 (ID checks the chip [415]) (same as X1 S-A7).
- S-A9: caught A1, owner right. ID and UID draw the prototype on the smart board in the next meeting [425-431].
- S-A10: caught A2, owner right. The UID takes a look at what was left to him at [499-500] (same as X1 S-A9).
- S-A11: not listed. Matches borderline A4 (UID opens images and paints them [644-646]).
- S-A12: made up. [641-643] is about whether the smart board pen can work better, not the scroll wheel (same as X1 S-A11).
Missed: D1, D5, D16, D17, A3
Counts: decisions caught 13/17, actions caught 2/3 (owner right 2), made up 6, not listed 7, duplicates 0

#### TS3006c
- S-D1: caught D1. Kinetic energy chosen [109-116, 170, 182].
- S-D2: made up. Opposite of what was settled: the single-curved compromise [270-282] was dropped for double curved [284-288].
- S-D3: caught D4. Advanced chip for the screen and audio [175-179].
- S-D4: made up. The flip-up screen [313-359, 737-741] was reversed at the end ("no flipping" [756-768]).
- S-D5: made up. The base colour was never settled: dark blue [416-453] was challenged by a bright background [507-515].
- S-D6: made up. The backlight was left open ("you should test it" [487]); white was only suggested [483-486].
- S-D7: caught D7. See-through plastic buttons with a coloured LED behind each [576-581].
- S-D8: not listed. Thicker ends and a narrow middle for grip came up while sketching [680-689]; supported detail, not on the list.
- S-D9: caught D8. No flipping mechanism, for robustness when dropped or shaken [756-773].
- S-D10: caught D9. Named the Real Remote [782-790] (copyright sign not mentioned, not false).
- S-A1: not listed. Putting a picture in the project folder [203-206] is an in-meeting mechanic.
- S-A2: made up. The fruit picture was already in the folder [209]; no one took on a task to share pictures.
- S-A3: made up. Two handed versions were floated [371] and rejected as too expensive in favour of one shape [373, 390-394].
- S-A4: not listed. The UID asked which display colour to use [472]; the backlight was left open to be tested [487], not on the list.
- S-A5: caught A1, owner right. Outline sketches of the shape in the smartboard design session [589, 614, 644-678].
- S-A6: duplicate. Per-person part of A1 for the UID [645, 647].
- S-A7: duplicate. Restates A1 (go and design after the meeting [777, 792-793]).
- S-A8: duplicate. Restates A1 for the UID.
Missed: D2, D3, D5, D6, A2, A3
Counts: decisions caught 5/9, actions caught 1/3 (owner right 1), made up 6, not listed 3, duplicates 3

### X3 (round 1, assignments, 4B)

#### ES2003c
- S-D1: caught D1. ID proposes conventional batteries over kinetic/solar [43], PM agrees [117, 204-205].
- S-D2: caught D2, D3. Rubber [120, 122, 206] and double-curved shape [139, 141, 206] settled; the battery part repeats D1 from S-D1.
- S-D3: made up. Articulation was proposed by the UI Designer and questioned by the PM as a weak point [208, 213]; left open, not confirmed.
- S-D4: not listed. Moving to the detailed design phase is the meeting schedule [258, 260], an in-meeting mechanic.
- S-D5: caught A1 (action written as a decision). Clay prototype for the next meeting [261-262, 315]; owner wrong (no owner named; the task is the Industrial and UI Designers').
- S-D6: caught D8 (borderline decision). PM puts the target audience at under about sixty with free cash [280, 283], Marketing accepts.
- S-A1: made up. The UI Designer asked whether clay will be supplied [317]; nobody, the Industrial Designer included, took on providing it.
- S-A2: caught A2. Working out the product evaluation [271-276, 315]; owner wrong (the Marketing Expert's task, not the PM's; the PM's own write-up was the minutes).
Missed: D5, D6, D7, A3, A4
Counts: decisions caught 4/8, actions caught 2/4 (owner right 0), made up 2, not listed 1, duplicates 0

#### ES2011c
- S-D1: made up. ME's tentative "for now we can do two prototypes maybe" [253] is a suggestion written as a decision, and it does not say which part is changeable (same as X1 S-D1).
- S-A1: caught A1, A2, owner right. PM gives "you two" the play-dough and the look and feel and user interface design [342] [343]. This item names both designers.
- S-A2: caught A3, owner wrong. Product evaluation [343] goes to the Marketing Expert. The Industrial Designer is one of "you two" on the model and design.
- S-A3: not listed. This catches borderline A4 (look into the menu-on-TV programming [152]). PM says "we have to look into" it and gives it to nobody (same as X1 S-A3).
Missed: D1, D2, D3, D4
Counts: decisions caught 0/4, actions caught 3/3 (owner right 2), made up 1, not listed 1, duplicates 0

#### IS1008c
- S-D1: caught D2. Wood is agreed for the case [29-35, 230-233]; customisation not stated but the intent (wooden case) matches.
- S-D2: caught D1. The advanced chip on print is the determined chip [32-34, 229-230].
- S-D3: caught D4. PM: stick with the simple design until Ed finds out what the display costs [238].
- S-D4: caught D3. Buttons rubber, not wood [234-237].
- S-A1: caught A4, owner right. Industrial Designer offers to find out more about the advanced chip on print before the next meeting [42-46].
- S-A2: caught A1, owner right. PM asks both designers to come up with the prototype by the next meeting [238-241, 251-252].
- S-A3: caught A2 and A3, owner right (both). Marketing Expert to find the display price and prepare the marketing strategy [238, 244-247].
- S-A4: duplicate. Display cost (A2) already caught by S-A3.
- S-A5: duplicate. Marketing strategy (A3) already caught by S-A3.
- S-A6: caught A5, owner right. PM: "I will submit to the management" [243].
Missed: none
Counts: decisions caught 4/4, actions caught 5/5 (owner right 5), made up 0, not listed 0, duplicates 2

#### TS3004c
- S-D1: caught D6, D4, D1 + made up (regular chip). Hard plastic case [396-397, 411], rechargeable batteries [346-348, 407], no LCD [375-380]; the regular chip was replaced by the advanced chip [672-675] (same as X1 S-D5).
- S-D2: caught D8 + made up (scroll wheel for channel control). 2D curved [361-363, 401]; the scroll wheel is for volume only, channels were ruled out [371-373], so it does not catch D10.
- S-D3: caught D9. Rounded corners only, not as round as the iPod [548-558].
- S-D4: not listed. Restatement of scroll wheel and lights as what sets it apart [504-508, 598]; does not state the volume role or the LED colours.
- S-D5: made up. Speech recognition was not left for consideration; the meeting decided to include it [708-710] (same as X1 S-D10).
- S-D6: caught D2. Speech recognition included [708-710].
- S-A1: not listed. Matches borderline A5 (ID checks the chip [415]) (same as X1 S-A7).
- S-A2: caught A1, owner right. ID and UID draw the prototype on the smart board in the next meeting [425-431].
- S-A3: caught A2, owner right. Channel button arrangement left to UID [499-500] (same as X1 S-A9).
- S-A4: made up. "I'll see it" [650] is a passing reply; no task to confirm originality against the iPod (same as X1 S-A10).
- S-A5: made up. [641-643] is about whether the smart board pen can work better, not the scroll wheel (same as X1 S-A11).
- S-A6: made up. Nobody took on finding the advanced chip's cost; ID said he did not know it [684-687] (same as X1 S-A12).
Missed: D3, D5, D7, D10, D11, D12, D13, D14, D15, D16, D17, A3
Counts: decisions caught 6/17, actions caught 2/3 (owner right 2), made up 6, not listed 2, duplicates 0

#### TS3006c
- S-D1: caught D1. Kinetic energy chosen [109-116, 170, 182].
- S-D2: caught D3 + made up (single curved). Rubber is right, but the single-curved compromise [270-282] was dropped for double curved [284-288].
- S-D3: caught D5. Speech commands for both the simple and advanced options [142-144].
- S-D4: caught D4. Advanced chip for the screen and audio [175-179].
- S-D5: not listed. "We should better choose one shape" [390-394] after two versions were called too expensive [373]; supported but not on the list.
- S-D6: made up. The backlight was left open ("you should test it" [487]); blue [475] was followed by a white suggestion [483-486], nothing decided.
- S-D7: caught D6. Red buttons by default [557-560]; matching the LED colour to the button [571] is consistent.
- S-D8: made up. Keeping the flip [737-741] was reversed later ("no flipping", "most votes count" [756-768]).
- S-D9: not listed (coordinator ruling: the sketching work of A1, written as a decision with no owner; A1 is credited to S-A2, which names the owners) [589, 644-678].
- S-D10: caught D8 + made up (poor aim). No flip, for robustness [756-773]; "it doesn't aim so well" [755] was about the smartboard pen, not the product.
- S-A1: not listed. Putting a picture in the project folder [203-206] is an in-meeting mechanic.
- S-A2: caught A1, owner right. ID and UID on the smartboard for thirty minutes [589, 606, 614].
- S-A3: caught A2, owner right. Marketing Expert prepares an evaluation procedure [593-601].
- S-A4: duplicate + made up (side buttons). Restates the A1 design work; the rubber layer is supported [756], but side buttons were never settled [806-817]. Does not mention the name, so it does not catch A3.
Missed: D2, D7, D9, A3
Counts: decisions caught 6/9, actions caught 2/3 (owner right 2), made up 5, not listed 3, duplicates 1

### X4 (round 2, settled, 4B)

#### ES2003c
- S-D1: caught D1. Conventional batteries over kinetic/solar [43, 112, 117, 204-205].
- S-D2: caught D3. Rubber case, UI and ME [120, 122], PM summary [206].
- S-D3: caught D2. Double curve for left and right hands [139, 141, 206].
- S-D4: caught D6. Joystick, two function buttons and LCD with no number keypad [133-134, 193, 197-198].
- S-D5: duplicate (D3). Spongy rubber restates S-D2 [176, 328-329].
- S-D6: caught D4. Bright yellow [217-226, 332].
- S-D7: not listed. ME and UI say it will need on-board memory [302, 305]; supported but not on the list.
- S-D8: caught D7 (borderline decision). Customisable menu with languages and skins, international [305-315].
- S-A1: not listed. Moving on to the detailed design for the next meeting is the meeting schedule [258-260], not an assigned task.
- S-A2: caught A2, A3. Product evaluation [271-276, 315] and the physical make-up [315]; owner wrong for both (A2 is the Marketing Expert's, A3 the Industrial Designer's; the PM's own write-up was the minutes).
- S-A3: made up. The UI Designer asked about the clay in the meeting [317]; nobody took on finding it as a follow-up.
Missed: D5, D8, A1, A4
Counts: decisions caught 6/8, actions caught 2/4 (owner right 0), made up 1, not listed 2, duplicates 1

#### ES2011c
- S-D1: caught D1. The team settles on no screen on the remote: "we don't want the screen I guess" [107] and "the TV is the screen" [103]. The cited [56] [59] are near it (no DVD, the screen on the telly), and the item leaves out the TV-menu part but has the same intent.
- S-D2: made up. ME's tentative "for now we can do two prototypes maybe" [253] is a suggestion written as a decision, with no changeable part named (same as X1 S-D1 and X3 S-D1).
- S-D3: made up. PM proposes batteries [316], but ME and UI prefer a dynamo [317] [318] and costs are unknown [322] [324]. It is left open (same as X2 S-D2).
- S-A1: not listed. This catches borderline A4 (look into the menu-on-TV programming [152]). PM says "we have to look into" it and gives it to nobody (same as X1 S-A3).
- S-A2: caught A1, A2, owner right. PM gives "you two" the play-dough and the look and feel and user interface design [342] [343]. This item names both designers (same as X3 S-A1).
- S-A3: caught A3, owner wrong. Product evaluation [343] goes to the Marketing Expert. The two designers named here are "you two" on the model and design.
Missed: D2, D3, D4
Counts: decisions caught 1/4, actions caught 3/3 (owner right 2), made up 2, not listed 1, duplicates 0

#### IS1008c
- S-D1: caught D2. Wood is agreed for the case [29-35, 230-233]; customisation not stated but the intent (wooden case) matches.
- S-D2: caught D1. Advanced chip on print with the integrated sensor and speaker [32-34, 229-230].
- S-D3: caught D4. PM: stick with the simple design until Ed finds out what the display costs [238].
- S-D4: caught D3. Buttons rubber, not wood [234-237].
- S-A1: caught A4, owner right. Industrial Designer offers to find out more about the advanced chip on print before the next meeting [42-46].
- S-A2: caught A1, owner right. PM asks both designers to come up with the prototype by the next meeting [238-241, 251-252].
- S-A3: caught A2 and A3, owner right (both). Marketing Expert to find the display cost and prepare the marketing strategy [238, 244-247].
Missed: A5
Counts: decisions caught 4/4, actions caught 4/5 (owner right 4), made up 0, not listed 0, duplicates 0

#### TS3004c
- S-D1: caught D6. Hard plastic case [396-397, 411].
- S-D2: caught D4. Rechargeable batteries, not kinetic or solar [346-348, 356, 407].
- S-D3: made up. The regular chip [378, 412-413] was replaced by the advanced chip for speech recognition [672-675] (same as X1 S-D5).
- S-D4: caught D8. Curved in 2D only, not 3D [360-363, 401].
- S-D5: caught D12. Numbers at the bottom [468, 470].
- S-D6: duplicate (D12). Channel switch in the middle, part of the same layout decision [468, 470].
- S-D7: caught D10. Volume on a side scroll wheel [368, 371, 449].
- S-D8: caught D9. Rounded corners, kept a bit square [548-558, 614].
- S-D9: caught D11. Small yellow scroll wheel [628, 638-640].
- S-D10: caught D2, D3. Speech recognition included [708-710] with the advanced chip [672-675].
- S-D11: duplicate (D2). Speech recognition included [708].
- S-A1: not listed. Matches borderline A5 (ID checks the chip [415]) (same as X1 S-A7).
- S-A2: caught A3 + made up (rounded scroll wheels). UID looks for iPod, MP3 player and phone examples [435-439], owner right; the examples are for the remote's rounded shape, not scroll wheels.
- S-A3: caught A2, owner right. The UID takes a look at the layout left to him at [499-500] (same as X2 S-A10).
- S-A4: not listed. Matches borderline A4 (UID opens images and paints them [644-646]) (same as X2 S-A11).
- S-A5: made up. "I'll see it" [650] is a passing reply; no task to confirm originality (same as X1 S-A10).
Missed: D1, D5, D7, D13, D14, D15, D16, D17, A1
Counts: decisions caught 9/17, actions caught 2/3 (owner right 2), made up 3, not listed 2, duplicates 2

#### TS3006c
- S-D1: caught D1. Kinetic energy chosen [109-116, 170, 182].
- S-D2: caught D3. Rubber case material [170-173].
- S-D3: made up. Opposite of what was settled: the single-curved compromise [270-282] was dropped for double curved [284-288].
- S-D4: made up. The base colour was never settled: dark blue [416-453] was challenged by a bright background [507-515].
- S-D5: made up. The backlight was left open ("you should test it" [487]); blue [475] was followed by a white suggestion [483-486], nothing decided.
- S-D6: caught D6. Red buttons by default [557-560]; matching the LED colour to the button [571] is consistent.
- S-D7: made up. Keeping the flip [737-741] was reversed later ("no flipping", "most votes count" [756-768]).
- S-D8: caught D8. No flipping mechanism; screen integrated with rubber around it [756-768].
- S-D9: caught D9. Named the Real Remote [782-790] (copyright sign not mentioned, not false).
- S-A1: not listed. Putting a presentation in the shared folder [15-21] is an in-meeting mechanic.
- S-A2: not listed. Putting a picture in the project folder [203-206] is an in-meeting mechanic.
- S-A3: caught A1, owner right. ID and UID design the look-and-feel and sketch the shape on the smartboard for thirty minutes [589, 606, 614, 644].
- S-A4: duplicate. Restates A1 (go and design after the meeting [777, 800]); does not mention the name, so it does not catch A3.
Missed: D2, D4, D5, D7, A2, A3
Counts: decisions caught 5/9, actions caught 1/3 (owner right 1), made up 4, not listed 2, duplicates 1

### X5 (round 3, handed, 4B)

#### ES2003c
- S-D1: caught D3. Spongy rubber skin, possibly over a firmer inner frame [176, 206, 213, 329].
- S-D2: caught D2 + made up (articulation). Double curve is settled [139, 141, 206]; articulation was questioned by the PM and left open [208, 213].
- S-D3: caught D1. Conventional batteries over kinetic/solar [43, 112, 117, 204-205].
- S-D4: caught D6. No number keypad, channels through the LCD menu [134, 193, 197-198].
- S-D5: made up. The PC programming link (and its offline fallback) was the UI Designer's proposal; the PM says it depends on cost and may be prohibitive [286, 299], left open.
- S-D6: made up. The on-remote code lookup was a PM suggestion in discussion [245]; never settled.
- S-D7: not listed. A joke about the drawing [213-215]; supported but trivial.
- S-D8: caught D7 (borderline decision). Customisable menu with languages, international [305-315].
- S-D9: not listed. Finding a place for the slogan is borderline action A5 [337-338]; catching it counts neither way.
- S-A1: not listed. Presenting the components slides is an in-meeting mechanic [6].
- S-A2: not listed. The ID drew the shapes on the whiteboard during the meeting [54-68], an in-meeting mechanic.
- S-A3: not listed. PM says "we have to look into the costs" of bulk buying [99]; supported but minor and not on the list.
- S-A4: caught A1. Clay prototype for the next meeting [261-262, 315]; owner right (names the Industrial Designer, one of the two owners).
- S-A5: caught A2. Working out the product evaluation [271-276, 315]; owner wrong (the Marketing Expert's task, not the PM's; the PM's own write-up was the minutes).
Missed: D4, D5, D8, A3, A4
Counts: decisions caught 5/8, actions caught 2/4 (owner right 1), made up 3, not listed 5, duplicates 0

#### ES2011c
- S-D1: caught D3. A changeable cover on the squishy part matches the interchangeable part: ME's cover for the squashy bit [232] [253], UI's detachable squishy part [251] and ID's "switching out the squishy part" [297] (same reading as X2 S-D1, without the prototypes).
- S-D2: made up. PM proposes batteries [316], but ME and UI prefer a dynamo [317] [318] and costs are unknown [322] [324]. It is left open (same as X2 S-D2 and X4 S-D3).
- S-A1: caught A1, A2, owner right. PM gives "you two" the play-dough and the look and feel and user interface design [342] [343]. This item names both designers (same as X3 S-A1 and X4 S-A2).
- S-A2: caught A3, owner wrong. Product evaluation [343] goes to the Marketing Expert, not the two designers (same as X4 S-A3).
Missed: D1, D2, D4
Counts: decisions caught 1/4, actions caught 3/3 (owner right 2), made up 1, not listed 0, duplicates 0

#### IS1008c
- S-D1: caught D2. Wood is agreed for the case [29-35, 230-233]; customisation not stated but the intent (wooden case) matches.
- S-D2: caught D1. Advanced chip on print with the integrated sensor and speaker [32-34, 229-230].
- S-D3: caught D4. PM: stick with the simple design for the time being [238]; the display-cost condition is left out but the intent matches.
- S-D4: made up. The PM floated a business tie-up but deferred production ("we can talk about the production later") and the team left manufacturing to explore it; a proposal written as settled [196-205].
- S-A1: caught A4, owner right. Industrial Designer offers to find out more about the advanced chip on print before the next meeting [42-46].
- S-A2: caught A1, owner right. PM asks both designers to come up with the prototype by the next meeting [238-241, 251-252].
- S-A3: caught A2 and A3, owner right (both). Marketing Expert to prepare the marketing strategy and find the display cost [238, 244-247].
Missed: D3, A5
Counts: decisions caught 3/4, actions caught 4/5 (owner right 4), made up 1, not listed 0, duplicates 0

#### TS3004c
- S-D1: caught D9. iPod/MP3 player look with rounded shape [133, 548-553, 652].
- S-D2: caught D14 + made up (soft materials). Transparent case [255, 262]; a soft case was rejected for hard plastic [391-397] (same as X1 S-D1).
- S-D3: made up. Black and yellow for the remote was the PM's proposal [226, 231, 236], disputed [234-237] and answered with changeable coloured fronts [240-242]; never settled. Does not state the logo going on the remote.
- S-D4: caught D10. Volume on a side scroll wheel [162-164, 368, 449].
- S-D5: caught D12. Frequently used channel buttons on top, text TV lower down [171-174].
- S-D6: caught D7. Buttons flush, on one level [273, 276, 280].
- S-D7: caught D6, D8. Hard plastic case [396-397, 411], 2D curved [361-363, 401].
- S-D8: caught D4. Rechargeable batteries [346-348, 356, 407].
- S-D9: made up. The regular chip [378, 412-413] was replaced by the advanced chip for speech recognition [672-675] (same as X1 S-D5).
- S-D10: caught D1. No LCD [375-380].
- S-D11: duplicate (D10). Scroll wheel for volume [368, 371].
- S-D12: made up. The light comes on when any button is used [36] and backlight LEDs light the case [296]; nothing ties a flashing LED to channel switching.
- S-A1: not listed. In-meeting mechanic: ID presenting the material options he received [18] (same as X1 S-A1).
- S-A2: not listed. UID checked what the technology department meant [104] and reported it in the meeting [688-693] (same as X1 S-A2).
- S-A3: not listed. In-meeting mechanic: ME's market research presentation [49-60].
- S-A4: not listed. In-meeting mechanic: ID's materials and chip presentation [120-130, 349-378].
- S-A5: not listed. Matches borderline A5 (ID checks the chip [415]) (same as X1 S-A7).
- S-A6: caught A2, owner right. The UID takes a look at what was left to him at [499-500] (same as X2 S-A10).
- S-A7: not listed. ME's "I'll see it" [650] is an in-meeting reply to the shape being settled; unlike X1 S-A10 it adds no invented originality check.
- S-A8: made up. [641-643] is about whether the smart board pen can work better, not the scroll wheel (same as X1 S-A11).
- S-A9: made up. Nobody took on finding the advanced chip's cost; ID said he did not know it [684-687] (same as X1 S-A12).
Missed: D2, D3, D5, D11, D13, D15, D16, D17, A1, A3
Counts: decisions caught 9/17, actions caught 1/3 (owner right 1), made up 6, not listed 6, duplicates 1

#### TS3006c
- S-D1: caught D1. Kinetic energy chosen [109-116, 170, 182].
- S-D2: caught D3 + made up (single curved). Rubber is right, but the single-curved compromise [270-282] was dropped for double curved [284-288].
- S-D3: caught D5. Speech commands for both the simple and advanced options [142-144].
- S-D4: not listed. Each button having its own LED is supported [164, 548-550] but does not state the see-through button lit whole by its LED (D7).
- S-D5: made up. The base colour was never settled: dark blue [416-453] was challenged by a bright background [507-515].
- S-D6: made up. The backlight was left open ("you should test it" [487]); white was only suggested [483-486].
- S-D7: made up. Flashing standby lights were proposed [455-462] and dismissed by the PM as "a bit too much" [463].
- S-D8: caught D6. Red buttons by default [557-560]; matching the LED colour to the button [571] is consistent.
- S-D9: made up. Keeping the flip [737-741] was reversed later ("no flipping", "most votes count" [756-768]).
- S-D10: caught D8. No flipping mechanism; screen integrated with rubber around it [756-768].
- S-D11: caught D9. Named the Real Remote [782-790] (copyright sign not mentioned, not false).
- S-A1: not listed. Putting a picture in the project folder [203-206] is an in-meeting mechanic.
- S-A2: caught A1, owner right. ID and UID design the look-and-feel on the smartboard for thirty minutes [589, 606, 614].
- S-A3: duplicate. Restates A1 (go and design after the meeting [777, 800]); does not mention the name, so it does not catch A3.
Missed: D2, D4, D7, A2, A3
Counts: decisions caught 6/9, actions caught 1/3 (owner right 1), made up 5, not listed 2, duplicates 1

### X6 (round 4, checked, 4B)

#### ES2003c
- S-D1: caught D1. Conventional batteries over kinetic/solar [43, 112, 117, 204-205].
- S-D2: not listed. The LCD was carried over from the last meeting [1-2] and the advanced chip is its stated consequence [76, 116]; supported but not on the list.
- S-D3: caught D3. Rubber case, UI and ME [120, 122], PM summary [206]; the UI Designer's reason was ergonomics [138].
- S-D4: caught D6. Joystick, two function buttons and LCD with no number keypad [133-134, 193, 197-198].
- S-D5: caught D2 + made up (articulated). Double curve for left and right hands is settled [139, 141, 206]; articulation was questioned by the PM and left open [208, 213].
- S-D6: not listed. The UI Designer's description of the control layout and modes [137-138, 158]; supported but not on the list.
- S-D7: made up. The PC programming link (with a fallback mode) was the UI Designer's proposal; the PM says it depends on cost and may be prohibitive [286, 299], left open.
- S-D8: duplicate (D3). Flexible rubber restates S-D3 [206].
- S-D9: not listed. A joke about the drawing [213-215]; supported but trivial.
- S-D10: made up. "RR" on the top function button was the UI Designer's proposal at the close [340]; nobody agreed.
- S-A1: caught A2, A3. Product evaluation [271-276, 315] and the physical make-up [315]; owner wrong for both (A2 is the Marketing Expert's, A3 the Industrial Designer's; the PM's own write-up was the minutes).
Missed: D4, D5, D7, D8, A1, A4
Counts: decisions caught 4/8, actions caught 2/4 (owner right 0), made up 3, not listed 3, duplicates 1

#### ES2011c
- S-D1: made up. ME's tentative "for now we can do two prototypes maybe" [253] is a suggestion written as a decision (same as X1 S-D1). It was about which part takes the cover (the squashy bit or the hard face [232] [237]), not which fruit or vegetable shape is best, and it names no changeable part.
- S-A1: caught A1, A2, owner right. PM gives "you two" the play-dough and the look and feel and user interface design [342] [343]. This item names both designers (same as X3 S-A1).
- S-A2: made up. At [344] PM tells the others they "will get more instructions from your personal coach". It is a closing remark to the team, not a task the Project Manager took on.
Missed: D1, D2, D3, D4, A3
Counts: decisions caught 0/4, actions caught 2/3 (owner right 2), made up 2, not listed 0, duplicates 0

#### IS1008c
- S-D1: caught D2. Wood is agreed for the case [29-35, 230-233]; customisation not stated but the intent (wooden case) matches.
- S-D2: caught D1. Advanced chip on print with the integrated sensor and speaker [32-34, 229-230].
- S-D3: caught D4. PM: stick with the simple design for the time being [238]; the display-cost condition is left out but the intent matches.
- S-D4: caught D3 + made up (mahogany red as the chosen wood). Rubber buttons are agreed [234-237]; the customisable wooden case restates D2, already caught by S-D1; "mahogany red" was the Marketing Expert's aside, while the buyer chooses type and colour of wood [71, 233].
- S-A1: caught A4, owner right. Industrial Designer offers to find out more about the advanced chip on print before the next meeting [42-46].
- S-A2: caught A1, owner right. PM asks both designers to come up with the prototype by the next meeting [238-241, 251-252].
- S-A3: caught A2 and A3, owner right (both). Marketing Expert to prepare the marketing strategy and find the display cost [238, 244-247].
Missed: A5
Counts: decisions caught 4/4, actions caught 4/5 (owner right 4), made up 1, not listed 0, duplicates 0

#### TS3004c
- S-D1: caught D9. Modern look with rounded corners [213-220, 548-553].
- S-D2: caught D14 + made up (soft case). Transparent case [255, 262]; a soft case was rejected for hard plastic [391-397] (same as X1 S-D1).
- S-D3: caught D7. Buttons on one level [273, 276, 280].
- S-D4: caught D10. Volume on the side [162-164, 368, 449].
- S-D5: caught D12. Frequently used channel buttons on top [171-172, 185].
- S-D6: duplicate (D12). Text TV lower down, part of the same layout decision [174].
- S-D7: caught D6, D8. Hard plastic case [396-397, 411], 2D curved [361-363, 401].
- S-D8: caught D4. Rechargeable batteries [346-348, 356].
- S-D9: caught D1 + made up (regular chip). No LCD [375-380]; the regular chip was replaced by the advanced chip [672-675] (same as X3 S-D1).
- S-D10: duplicate (D10). Volume scroll wheel on the side [449].
- S-D11: made up. Channel up above and down below the numbers was only the ID's proposal [475], disputed [476-494] and left to the UID [499-500] (same as X2 S-D9).
- S-D12: duplicate (D9). Rounded corners [548-558].
- S-A1: not listed. Matches borderline A5 (ID checks the chip [415]) (same as X1 S-A7).
- S-A2: caught A2, owner right. The UID takes a look at the layout left to him at [499-500] (same as X4 S-A3).
- S-A3: not listed. ME's "I'll see it" [650] is an in-meeting reply (same as X5 S-A7).
- S-A4: made up. [641-643] is about whether the smart board pen can work better; nothing about a hardware team or the advanced chip (same as X1 S-A11).
Missed: D2, D3, D5, D11, D13, D15, D16, D17, A1, A3
Counts: decisions caught 9/17, actions caught 1/3 (owner right 1), made up 4, not listed 2, duplicates 3

#### TS3006c
- S-D1: caught D1. Kinetic energy chosen [109-116, 170, 182].
- S-D2: made up. Opposite of what was settled: the single-curved compromise [270-282] was dropped for double curved [284-288].
- S-D3: caught D4. Advanced chip for the screen and audio [175-179].
- S-D4: made up. The base colour was never settled: dark blue [416-453] was challenged by a bright background [507-515].
- S-D5: made up. The backlight was left open ("you should test it" [487]); white was only suggested [483-486].
- S-D6: caught D6 + made up (green background). Red buttons by default [557-560]; the green body colour was only tried on the board and called cheap [513-544], never settled.
- S-D7: made up. Keeping the flip [737-741] was reversed later ("no flipping", "most votes count" [756-768]).
- S-D8: caught D8. No flipping mechanism; screen integrated with rubber around it [756-768].
- S-D9: caught D9. Named the Real Remote [782-790] (copyright sign not mentioned, not false).
- S-A1: not listed. Putting a picture in the project folder [203-206] is an in-meeting mechanic.
- S-A2: caught A1, owner right. ID and UID design the look-and-feel and sketch the shape on the smartboard for thirty minutes [589, 606, 614, 644].
- S-A3: duplicate. Restates A1 (go and design after the meeting [777]); does not mention the name, so it does not catch A3.
Missed: D2, D3, D5, D7, A2, A3
Counts: decisions caught 5/9, actions caught 1/3 (owner right 1), made up 6, not listed 1, duplicates 1

### X7 (final check, settled, 9B)

#### ES2003c
- S-D1: caught D1. Conventional batteries over kinetic/solar [43, 112, 117, 204-205].
- S-D2: caught D3. Rubber case, UI and ME [120, 122], PM summary [206]; the UI Designer's reason was ergonomics [138].
- S-D3: caught D2 + made up (articulated at two points). Double curve for left and right hands is settled [139, 141, 206]; articulation was questioned by the PM and left open [208, 213].
- S-D4: duplicate (D3). Spongy, stress-ball rubber restates S-D2 [176].
- S-D5: duplicate (D2, D3). Double curve and rubber skin over a firm inner frame restate S-D3 and S-D2 [206-207, 213].
- S-D6: made up. Two points of articulation were the UI Designer's proposal, questioned by the PM and left open [207-213].
- S-D7: not listed. A joke about the drawing [213-215]; supported but trivial.
- S-D8: caught D4. Yellow to match the logo [217-226].
- S-D9: duplicate (D3). Spongy rubber restates S-D2 [329].
- S-D10: duplicate (D4). Bright yellow restates S-D8 [332].
- S-D11: made up. "RR" on the top function button was the UI Designer's proposal at the close [340]; nobody agreed.
- S-A1: made up. In his summary the PM says he does not know the cost implications of the double curve [206]; nobody took on looking into costs.
- S-A2: not listed. Moving on to the detailed design for the next meeting is the meeting schedule [258-260], not an assigned task.
- S-A3: caught A2. Working out the product evaluation [271-276, 315]; owner wrong (the Marketing Expert's task, not the PM's; the PM's own write-up was the minutes).
- S-A4: caught A3. Looking further at the physical make-up [315]; owner wrong (the Industrial Designer's task, not the PM's).
- S-A5: made up. The UI Designer asked about the clay in the meeting [317]; nobody took on finding it as a follow-up.
Missed: D5, D6, D7, D8, A1, A4
Counts: decisions caught 4/8, actions caught 2/4 (owner right 0), made up 5, not listed 2, duplicates 4

#### ES2011c
- S-D1: caught D1. The team settles on no screen on the remote [103] [107]. The cost reason is supported: ID says a screen needs the advanced chip [93], and UI calls it "incredibly expensive" [107].
- S-D2: caught D2. ID proposes the stress-ball rubber [190], UI and ID welcome it [193] [201], and PM puts it around the grip [200] [220].
- S-D3: caught D3 + made up (two prototypes as a settled plan). Covering the squishy part as the changeable option matches D3 [232] [251] [253]. ME's "for now we can do two prototypes maybe" [253] is a tentative suggestion written as a decision (same as X2 S-D1).
- S-D4: made up. PM proposes batteries [316], but ME and UI prefer a dynamo [317] [318] and costs are unknown [322] [324]. It is left open (same as X2 S-D2).
- S-A1: not listed. This catches borderline A4 (look into the menu-on-TV programming [152]). PM says "we have to look into" it and gives it to nobody, so the User Interface Designer owner is not in the transcript (same treatment as X1 S-A3).
- S-A2: not listed. This catches borderline A5 (which cover is easiest to change and what it costs [253]). It was raised with no owner, and the Project Manager never took it on.
- S-A3: caught A1, A2, owner right. PM gives "you two" the play-dough and the look and feel and user interface design [342] [343], and the UI is one of the two designers.
- S-A4: duplicate. A1 and A2 were already caught by S-A3 (joint task for ID and UI).
- S-A5: caught A3, owner right. Product evaluation [343] goes to the Marketing Expert.
Missed: D4
Counts: decisions caught 3/4, actions caught 3/3 (owner right 3), made up 2, not listed 2, duplicates 1

#### IS1008c
- S-D1: caught D4. PM: stick with the simple design for the time being [238]; the display-cost condition is left out but the intent matches.
- S-D2: caught D2 + made up (mahogany red as the chosen wood). A customisable wooden case is agreed [99-100, 230-233]; "mahogany red" was the Marketing Expert's aside, while the buyer chooses type and colour of wood [71, 233].
- S-D3: caught D3. Buttons rubber, not wood [234-237].
- S-A1: caught A4, owner right. Industrial Designer offers to find out more about the advanced chip on print, including whether it is microprocessor based, before the next meeting [42-46].
- S-A2: made up. The Industrial Designer raised labour laws as a consideration, but said manufacturing would explore it and the PM deferred production; nobody took on the task [189-205].
- S-A3: caught A2, owner right. Marketing Expert to find out what the display costs [222-224, 238, 244].
- S-A4: caught A1, owner right. PM asks both designers to come up with the prototype by the next meeting [238-241, 251-252].
- S-A5: caught A3, owner right. Marketing Expert to prepare the marketing strategy [244-247].
Missed: D1, A5
Counts: decisions caught 3/4, actions caught 4/5 (owner right 4), made up 2, not listed 0, duplicates 0

#### TS3004c
- S-D1: caught D9 + made up (soft materials). Curved with rounded corners [213-220, 548-553]; a soft case was rejected for hard plastic [391-397] (same as X6 S-D2).
- S-D2: caught D16 + made up (black and yellow colour scheme). Logo on the remote [226, 516-517]; black and yellow for the remote was a disputed proposal [231-237] never settled (same as X5 S-D3).
- S-D3: caught D14, D13. Transparent fronts [245, 255, 262] and changeable coloured fronts [240-242, 260] (same as X2 S-D1).
- S-D4: caught D7. Buttons flush, on one level [273, 276, 280].
- S-D5: caught D12. Frequently used channel buttons on top, little-used ones lower down [171-174].
- S-D6: caught D4. Rechargeable batteries [346-348, 356].
- S-D7: made up. The regular chip [378, 412-413] was replaced by the advanced chip for speech recognition [672-675] (same as X1 S-D5).
- S-D8: caught D6, D8. Hard plastic case [396-397, 411], 2D curved [361-363, 401].
- S-D9: caught D10. Volume on a side scroll wheel [368, 371, 449].
- S-D10: made up. Channel up above and down below the numbers was only the ID's proposal [475], disputed [476-494] and left to the UID [499-500]; there is no number display (same as X2 S-D9).
- S-D11: duplicate (D9). Kept a bit square with rounded corners [614].
- S-D12: caught D11. Small yellow scroll wheel [628, 638-640].
- S-A1: not listed. In-meeting mechanic: ID presenting the material options [18, 83] (same as X1 S-A1).
- S-A2: not listed. UID checked what the technology department meant [104] and reported it in the meeting [688-693] (same as X1 S-A2).
- S-A3: not listed. UID's "I'll get back on it" [58, 91] was answered in the meeting (same as X2 S-A3).
- S-A4: not listed. Matches borderline A5 (ID checks the chip [415]) (same as X1 S-A7).
- S-A5: caught A1, owner right. ID works with the UID on the prototype drawing on the smart board in the next meeting [425-431]; joint task, names one of the pair.
- S-A6: caught A2, owner right. Channel button arrangement left to UID [489-500] (same as X1 S-A9).
- S-A7: not listed. Matches borderline A4 (UID opens images and paints them [644-646]) (same as X2 S-A11).
Missed: D1, D2, D3, D5, D15, D17, A3
Counts: decisions caught 11/17, actions caught 2/3 (owner right 2), made up 4, not listed 5, duplicates 1

#### TS3006c
- S-D1: caught D1. Kinetic energy chosen [109-116, 170, 182].
- S-D2: caught D3. Rubber case material [170-173].
- S-D3: made up. The body colour was never settled: the green was only tried on the board and called cheap [513-544], after dark blue [416-453].
- S-D4: caught D6. Red buttons by default [557-560].
- S-D5: caught D7. A coloured LED behind each button in the button's own colour [571, 576-581]; unlike a bare "each button has an LED", it states the light behind the button.
- S-D6: caught D8. No flipping mechanism; screen integrated with rubber around it [756-768].
- S-D7: caught D9. Named the Real Remote with a copyright sign after "Real" [782-790].
- S-A1: caught A1 + made up (side buttons), owner right. ID and UID go and design the device after the meeting [589, 777]; side volume/scroll buttons were never settled [806-817].
- S-A2: duplicate. Per-person copy of A1 for the UID (same side-buttons claim).
- S-A3: caught A2, owner right. Marketing Expert prepares an evaluation procedure [593-601].
Missed: D2, D4, D5, A3
Counts: decisions caught 6/9, actions caught 2/3 (owner right 2), made up 2, not listed 0, duplicates 1
