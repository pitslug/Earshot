// Sample meeting details for the mock IPC (component tests and the browser preview): a
// vendor call with a summary, two voices that need names and a voiceprint match.

import { tokensOf } from "../detail/dictionaryWord";
import type {
  Citation,
  MeetingDetail,
  MeetingListItem,
  PersonItem,
  SpeakerCard,
  SummaryView,
  TranscriptRow,
} from "./ipc";

/** Speaker ids of the sample detail (the same in every sample meeting, per meeting id). */
export const SAMPLE_SPEAKERS = {
  me: "5a000000-0000-7000-8000-000000000001",
  priya: "5a000000-0000-7000-8000-000000000002",
  second: "5a000000-0000-7000-8000-000000000003",
  third: "5a000000-0000-7000-8000-000000000004",
} as const;

/** Persons the sample starts with. */
export function samplePersons(): PersonItem[] {
  return [
    { id: "9e000000-0000-7000-8000-000000000001", name: "Casey", has_voiceprint: true, is_me: true, meetings: 12 },
    { id: "9e000000-0000-7000-8000-000000000002", name: "Priya Shah", has_voiceprint: true, is_me: false, meetings: 4 },
    { id: "9e000000-0000-7000-8000-000000000003", name: "Tom Reid", has_voiceprint: false, is_me: false, meetings: 2 },
  ];
}

const segmentId = (n: number) => `5e000000-0000-7000-8000-${String(n).padStart(12, "0")}`;

/** Track ids of the sample detail: the mic and, in a call, the computer's audio. */
export const SAMPLE_TRACKS = {
  mic: "7a000000-0000-7000-8000-000000000001",
  far: "7a000000-0000-7000-8000-000000000002",
} as const;

/** A segment id no sample transcript has (a citation of a replaced transcript). */
export const MISSING_SEGMENT = "5e000000-0000-7000-8000-999999999999";

const LINES: [speaker: keyof typeof SAMPLE_SPEAKERS, at: number, text: string][] = [
  ["second", 724_000, "So the lead time on the second pump set is now looking like eleven weeks."],
  ["me", 731_000, "Okay, that pushes commissioning past Christmas, which we can't wear."],
  [
    "second",
    739_000,
    "We could split the order and air freight the seals, so the first set still lands in time for the site acceptance test.",
  ],
  ["priya", 752_000, "If the seals go by air, we would want the vendor to carry that cost."],
  ["me", 760_000, "Agreed. I'll confirm the revised commissioning date with the client by Friday."],
  ["third", 771_000, "I can send the split-order quote this afternoon."],
  ["priya", 779_000, "Then the air freight is at the vendor's cost. That's decided."],
  ["second", 786_000, "Fine by us. I'll send it to Tom Reed in writing."],
];

/** The fixed suggestions of the sample's final transcript (T12.15e): "Reed" sounds like the
 * saved person Tom Reid. The mock does not run the sound matcher. */
const SAMPLE_SUGGESTIONS: Record<string, TranscriptRow["suggestions"]> = {
  [segmentId(8)]: [{ token: 8, decoded: "Reed", listed: "Reid" }],
};

function cite(...rows: number[]): Citation[] {
  return rows.map((row) => ({ segment_id: segmentId(row + 1), row }));
}

/** The id of a mock summary (`n` from 1). */
const summaryId = (n: number) => `5a000000-0000-7000-8000-${String(n).padStart(12, "0")}`;

function sampleSummary(): SummaryView {
  return {
    id: summaryId(1),
    title: "Vendor review: pumps",
    tldr:
      "The second pump set slips to 11 weeks. The team agreed to split the order and air freight the seals to hold commissioning before Christmas.",
    decisions: [
      { text: "Split the pump order into two shipments", citations: cite(2) },
      { text: "Air freight the seals at the vendor's cost", citations: cite(3, 6) },
    ],
    actions: [
      {
        owner: "Casey",
        task: "Confirm the revised commissioning date with the client",
        due: "Friday",
        citations: cite(4),
      },
      { owner: "Speaker 3", task: "Send the split-order quote", due: null, citations: cite(5) },
    ],
    notes: [],
    open_questions: [],
  };
}

const NAMES: Record<keyof typeof SAMPLE_SPEAKERS, string> = {
  me: "Me",
  priya: "Priya Shah",
  second: "Speaker 2",
  third: "Speaker 3",
};

/** The transcript rows of `speakers` (names from the cards). */
export function transcriptOf(lines: typeof LINES, speakers: SpeakerCard[]): TranscriptRow[] {
  return lines.map(([who, at, text], i) => {
    const id = SAMPLE_SPEAKERS[who];
    const card = speakers.find((s) => s.id === id);
    return {
      segment_id: segmentId(i + 1),
      start_ms: at,
      speaker_id: id,
      speaker: card?.name ?? NAMES[who],
      unknown_speaker: false,
      track: who === "me" ? "You" : "Them",
      track_id: who === "me" ? SAMPLE_TRACKS.mic : SAMPLE_TRACKS.far,
      text,
      edited: false,
      splittable: false,
      suggestions: [],
      changed_words: [],
    };
  });
}

function sampleSpeakers(): SpeakerCard[] {
  const persons = samplePersons();
  return [
    {
      id: SAMPLE_SPEAKERS.me,
      name: "Me",
      kind: "Me",
      person: { id: persons[0].id, name: "Casey", has_voiceprint: true },
      source: "Mic",
      share_percent: 41,
      sample_line: "Okay, that pushes commissioning past Christmas, which we can't wear.",
      needs_name: false,
      ignored: false,
      can_ignore: false,
      can_not_this_person: false,
      voice_saved_here: false,
      can_mark_me: false,
      can_unmark_me: false,
      recognised_me: false,
    },
    {
      id: SAMPLE_SPEAKERS.priya,
      name: "Priya Shah",
      kind: "Other",
      person: { id: persons[1].id, name: "Priya Shah", has_voiceprint: true },
      source: "Voiceprint",
      share_percent: 22,
      sample_line: "If the seals go by air, we would want the vendor to carry that cost.",
      needs_name: false,
      ignored: false,
      can_ignore: false,
      can_not_this_person: true,
      voice_saved_here: false,
      can_mark_me: false,
      can_unmark_me: false,
      recognised_me: false,
    },
    {
      id: SAMPLE_SPEAKERS.second,
      name: "Speaker 2",
      kind: "Other",
      person: null,
      source: "FarEnd",
      share_percent: 30,
      sample_line: "We could split the order and air freight the seals, so the first set still...",
      needs_name: true,
      ignored: false,
      can_ignore: true,
      can_not_this_person: false,
      voice_saved_here: false,
      can_mark_me: false,
      can_unmark_me: false,
      recognised_me: false,
    },
    {
      id: SAMPLE_SPEAKERS.third,
      name: "Speaker 3",
      kind: "Other",
      person: null,
      source: "FarEnd",
      share_percent: 7,
      sample_line: "I can send the split-order quote this afternoon.",
      needs_name: true,
      ignored: false,
      can_ignore: true,
      can_not_this_person: false,
      voice_saved_here: false,
      can_mark_me: false,
      can_unmark_me: false,
      recognised_me: false,
    },
  ];
}

/** The name of a final line with no speaker (`earshot_core::diar::UNKNOWN_SPEAKER`, T12.65). */
export const UNKNOWN_SPEAKER = "Unknown speaker";

/** `detail` with line `index` (from 0) left without a speaker by the final pass (T12.65). */
export function withUnknownLine(detail: MeetingDetail, index: number): MeetingDetail {
  return {
    ...detail,
    transcript: detail.transcript.map((r, i) =>
      i === index ? { ...r, speaker_id: null, speaker: UNKNOWN_SPEAKER, unknown_speaker: true } : r,
    ),
  };
}

// T12.90 and T12.90a: the mock's stored detail keeps a card for every live speaker row of the
// meeting, a speaker left with no line included (the runtime keeps its row), and serves only
// the cards with a line, as `detail::meeting_detail` lists them. What a line may move to and
// whether it splits are the runtime's rules over that state, pinned to them by
// `fixtures/ui/line_edit_cases.json` (`lineEdits.test.ts`, and `detail_tests.rs` on the Rust side).

/** A speaker row's role (`SpeakerRole`): the mock keeps none of its own. */
export type MockRole = "Me" | "Local" | "Remote" | "Room";

/** The role of card `s` in `detail`, read from what the card shows: the user's row is Me; in
 * the room every other voice is Room; in a call one heard on the mic is Local and one from the
 * computer's audio Remote, and a voice a saved voiceprint named is Local when one of its lines is
 * on the mic, else Remote. That is the runtime's role for every row a meeting of that kind holds
 * (`diar::two_track`, `diar::in_person`, `edit::new_speaker_role`). */
export function roleOf(detail: MeetingDetail, s: SpeakerCard): MockRole {
  if (s.kind === "Me") return "Me";
  if (detail.header.mode === "InRoom") return "Room";
  if (s.source === "Mic") return "Local";
  if (s.source === "FarEnd") return "Remote";
  return detail.transcript.some((r) => r.speaker_id === s.id && r.track !== "Them") ? "Local" : "Remote";
}

/** Whether track `track` of a row is the mic (`TrackId::Mic`; in the room every line is). */
const onMic = (track: TranscriptRow["track"]) => track !== "Them";

/** `edit::movable_by_track`: for each of the meeting's tracks (a line is on it, or it is one
 * the meeting recorded: the mic, and in a call the far end, `SAMPLE_TRACKS` when no line names
 * one of that kind), the speakers that belong to it (a line there, or no line at all and a role
 * of that track: Remote the far end, Local and Me the mic, Room the mic of an in-room meeting),
 * in the cards' order, with their names. Nothing in a draft. */
export function movableByTrack(detail: MeetingDetail): MeetingDetail["movable_by_track"] {
  if (detail.header.transcript !== "Final") return {};
  const inRoom = detail.header.mode === "InRoom";
  const withLines = new Set<string>();
  const onTrack = new Map<string, Set<string>>();
  const micTrack = new Map<string, boolean>();
  for (const r of detail.transcript) {
    micTrack.set(r.track_id, onMic(r.track));
  }
  // A live track with no line still gets its list (T12.91a), as the runtime lists the
  // meeting's live tracks.
  const kinds = new Set(micTrack.values());
  if (!kinds.has(true)) micTrack.set(SAMPLE_TRACKS.mic, true);
  if (!inRoom && !kinds.has(false)) micTrack.set(SAMPLE_TRACKS.far, false);
  for (const r of detail.transcript) {
    if (r.speaker_id === null) continue;
    withLines.add(r.speaker_id);
    const set = onTrack.get(r.track_id) ?? new Set<string>();
    set.add(r.speaker_id);
    onTrack.set(r.track_id, set);
  }
  const out: MeetingDetail["movable_by_track"] = {};
  for (const [track, mic] of micTrack) {
    out[track] = detail.speakers
      .filter((s) => {
        if (onTrack.get(track)?.has(s.id)) return true;
        if (withLines.has(s.id)) return false;
        const role = roleOf(detail, s);
        return role === "Remote" ? !mic : role === "Room" ? inRoom && mic : mic;
      })
      .map((s) => ({ id: s.id, name: s.name }));
  }
  return out;
}

/** `edit::can_split_line`: the line's words (`words`) are its white-space tokens, one to one,
 * and there are two or more. */
export function canSplitLine(text: string, words: readonly string[]): boolean {
  const tokens = tokensOf(text);
  return words.length >= 2 && tokens.length === words.length && tokens.every((t, i) => t === words[i]);
}

/** The detail as the app serves it (T12.90): only the speakers with a line, each track's move
 * offers and whether each line splits, from the line's words (`wordsOf`). */
export function withLineOffers(detail: MeetingDetail, wordsOf: (row: TranscriptRow) => readonly string[]): MeetingDetail {
  const final = detail.header.transcript === "Final";
  const withLines = new Set(detail.transcript.map((r) => r.speaker_id));
  return {
    ...detail,
    speakers: detail.speakers.filter((s) => withLines.has(s.id)),
    movable_by_track: movableByTrack(detail),
    transcript: detail.transcript.map((r) => ({ ...r, splittable: final && canSplitLine(r.text, wordsOf(r)) })),
  };
}

/** `edit::new_speaker_role`: the line's speaker's role, except that there is one Me per meeting,
 * so a line of Me's (or of no speaker) gets the role of a new voice on its track. */
export function newSpeakerRole(current: MockRole | null, inRoom: boolean, track: TranscriptRow["track"]): MockRole {
  if (current !== null && current !== "Me") return current;
  if (inRoom) return "Room";
  return onMic(track) ? "Local" : "Remote";
}

/** The out-of-date marks a transcript change gives a summary. */
const transcriptChanged = (detail: MeetingDetail): Pick<MeetingDetail, "summary_stale" | "summary_stale_names"> => ({
  summary_stale: detail.summary !== null || detail.summary_stale,
  summary_stale_names: false,
});

/** `moveLine` on the sample: the line takes `speaker`'s name and counts as edited; a new
 * speaker's card ("Speaker N") joins the stored cards. A card left with no line stays stored
 * (its row stays) and is no longer served. */
export function movedLine(detail: MeetingDetail, segment: string, speaker: SpeakerCard): MeetingDetail {
  const listed = detail.speakers.some((s) => s.id === speaker.id);
  return {
    ...detail,
    speakers: listed ? detail.speakers : [...detail.speakers, speaker],
    transcript: detail.transcript.map((r) =>
      r.segment_id === segment
        ? { ...r, speaker_id: speaker.id, speaker: speaker.name, unknown_speaker: false, edited: true }
        : r,
    ),
    ...transcriptChanged(detail),
  };
}

/** A new speaker card of role `role` (`edit::move_line`'s `New`): the next free "Speaker N"
 * among every stored card (`first_free_speaker_name` over every live speaker row), no person,
 * needing a name. */
export function newSpeakerCard(detail: MeetingDetail, id: string, role: MockRole): SpeakerCard {
  const taken = new Set(detail.speakers.map((s) => s.name));
  let n = 1;
  while (taken.has(`Speaker ${n}`)) n += 1;
  return {
    id,
    name: `Speaker ${n}`,
    kind: "Other",
    person: null,
    source: role === "Remote" ? "FarEnd" : "Mic",
    share_percent: 0,
    sample_line: "",
    needs_name: true,
    ignored: false,
    can_ignore: false,
    can_not_this_person: false,
    voice_saved_here: false,
    can_mark_me: false,
    can_unmark_me: false,
    recognised_me: false,
  };
}

/** `text` cut before its white-space token `token` (from 1) as `edit::split_line` cuts it: the
 * first part up to the end of the token before, the second from the token's start, each with
 * the white space at its ends dropped and the white space inside kept. */
export function splitText(text: string, token: number): [string, string] {
  const ranges = [...text.matchAll(/[^\p{White_Space}]+/gu)].map((m) => [m.index, m.index + m[0].length]);
  const trim = (s: string) => s.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, "");
  return [trim(text.slice(0, ranges[token - 1][1])), trim(text.slice(ranges[token][0]))];
}

/** `splitLine` on the sample: line `segment` split before token `token` (both parts edited);
 * the second part, id `second`, starts part way to the next line (the mock keeps no word
 * times), and the word marks after the split go with it, renumbered. */
export function splitRow(detail: MeetingDetail, segment: string, token: number, second: string): MeetingDetail {
  const i = detail.transcript.findIndex((r) => r.segment_id === segment);
  const r = detail.transcript[i];
  const tokens = tokensOf(r.text);
  const next = detail.transcript[i + 1]?.start_ms ?? r.start_ms + 1_000 * tokens.length;
  const [before, after] = splitText(r.text, token);
  const first: TranscriptRow = {
    ...r,
    text: before,
    edited: true,
    suggestions: r.suggestions.filter((w) => w.token < token),
    changed_words: r.changed_words.filter((w) => w.token < token),
  };
  const rest: TranscriptRow = {
    ...r,
    segment_id: second,
    start_ms: r.start_ms + Math.floor(((next - r.start_ms) * token) / tokens.length),
    text: after,
    edited: true,
    suggestions: r.suggestions.filter((w) => w.token >= token).map((w) => ({ ...w, token: w.token - token })),
    changed_words: r.changed_words.filter((w) => w.token >= token).map((w) => ({ ...w, token: w.token - token })),
  };
  return {
    ...detail,
    transcript: [...detail.transcript.slice(0, i), first, rest, ...detail.transcript.slice(i + 1)],
    ...transcriptChanged(detail),
  };
}

/** The sample detail of a meeting in the list (a draft without summary while it is processed
 * or recorded). */
export function sampleDetail(item: MeetingListItem): MeetingDetail {
  const ready = item.state === "Ready";
  const speakers = ready
    ? sampleSpeakers()
    : sampleSpeakers()
        .filter((s) => s.kind === "Me")
        .concat([])
        .map((s) => ({ ...s, person: null }));
  const draft = !ready;
  const transcript = transcriptOf(LINES, speakers).map((r) =>
    draft
      ? { ...r, speaker: r.track === "You" ? "Me" : "Them", speaker_id: r.track === "You" ? SAMPLE_SPEAKERS.me : null }
      : { ...r, suggestions: SAMPLE_SUGGESTIONS[r.segment_id] ?? [] },
  );
  return {
    id: item.id,
    state: item.state,
    header: {
      title: item.title,
      started_at_ms: item.started_at_ms,
      duration_ms: item.duration_ms,
      mode: "Call",
      transcript: draft ? "Draft" : "Final",
      summary_writer: ready ? "Qwen 3.5 9B" : null,
      summary_edited: false,
      transcript_backend: "GPU (CUDA)",
    },
    summary: ready ? sampleSummary() : null,
    transcript,
    // Filled from the speakers and lines when the mock serves the detail.
    movable_by_track: {},
    speakers,
    can_undo_summary: false,
    summary_stale: false,
    summary_stale_names: false,
    audio_kept: true,
    waiting_for: null,
    summary_run: null,
    handovers: [],
    kept_local: null,
    // Filled from the Dictionary and the persons when the mock serves the detail.
    word_list: [],
    dictionary: [],
  };
}

/** A meeting recorded in the room with no saved voice: every voice may be the user. */
export function sampleInRoomDetail(item: MeetingListItem): MeetingDetail {
  const d = sampleDetail(item);
  const speakers = sampleSpeakers().map(
    (s, i): SpeakerCard => ({
      ...s,
      kind: "Other",
      name: `Speaker ${i + 1}`,
      person: null,
      source: "Mic",
      needs_name: true,
      ignored: false,
      can_ignore: true,
      can_not_this_person: false,
      voice_saved_here: false,
      can_mark_me: true,
    }),
  );
  return {
    ...d,
    header: { ...d.header, mode: "InRoom" },
    speakers,
    transcript: transcriptOf(LINES, speakers).map((r) => ({ ...r, track: "Room", track_id: SAMPLE_TRACKS.mic })),
  };
}

/** A meeting recorded in the room where the user's voiceprint named the first voice (T12.44):
 * "This isn't me" is refused on it until the user's voice is forgotten. */
export function sampleRecognisedInRoomDetail(item: MeetingListItem): MeetingDetail {
  const d = sampleInRoomDetail(item);
  const user = samplePersons()[0];
  const speakers = d.speakers.map(
    (s, i): SpeakerCard =>
      i === 0
        ? {
            ...s,
            kind: "Me",
            name: user.name,
            person: { id: user.id, name: user.name, has_voiceprint: true },
            needs_name: false,
            can_ignore: false,
            can_not_this_person: false,
            can_mark_me: false,
            recognised_me: true,
          }
        : { ...s, can_mark_me: false },
  );
  return {
    ...d,
    speakers,
    transcript: transcriptOf(LINES, speakers).map((r) => ({ ...r, track: "Room", track_id: SAMPLE_TRACKS.mic })),
  };
}

/** The summary a mock re-summary writes. */
export function resummarised(previous: SummaryView): SummaryView {
  return {
    ...previous,
    id: summaryId(Number(previous.id.slice(-12)) + 1),
    tldr: `${previous.tldr} The client hears the new date by Friday.`,
  };
}

/** `detail` after renaming speaker `id` (transcript rows follow). */
export function renamed(detail: MeetingDetail, id: string, name: string): MeetingDetail {
  return {
    ...detail,
    speakers: detail.speakers.map((s) => (s.id === id ? { ...s, name } : s)),
    transcript: detail.transcript.map((r) => (r.speaker_id === id ? { ...r, speaker: name } : r)),
  };
}

/** `detail` after merging `merge` into `keep`. */
export function merged(detail: MeetingDetail, keep: string, merge: string): MeetingDetail {
  const kept = detail.speakers.find((s) => s.id === keep);
  const gone = detail.speakers.find((s) => s.id === merge);
  if (!kept || !gone) return detail;
  return {
    ...detail,
    speakers: detail.speakers
      .filter((s) => s.id !== merge)
      .map((s) => (s.id === keep ? { ...s, share_percent: s.share_percent + gone.share_percent } : s)),
    transcript: detail.transcript.map((r) =>
      r.speaker_id === merge ? { ...r, speaker_id: keep, speaker: kept.name } : r,
    ),
  };
}

/** `detail` after linking `id` to `person`. */
export function linked(detail: MeetingDetail, id: string, person: PersonItem): MeetingDetail {
  const next = renamed(detail, id, person.name);
  return {
    ...next,
    speakers: next.speakers.map((s) =>
      s.id === id
        ? {
            ...s,
            person: { id: person.id, name: person.name, has_voiceprint: person.has_voiceprint },
            needs_name: false,
            ignored: false,
            can_ignore: false,
            // `edit::can_not_this_person`: not the user's row, not linked to the user, and a
            // stored voice (the mock reads one from the voice's own offers before the link).
            can_not_this_person:
              s.kind === "Other" && !person.is_me && (s.can_ignore || s.can_not_this_person),
          }
        : s,
    ),
  };
}

/** `detail` after "Ignore" (`ignore` true) or its undo on `id` (T12.19): the label stays. */
export function ignoredVoice(detail: MeetingDetail, id: string, ignore: boolean): MeetingDetail {
  return {
    ...detail,
    speakers: detail.speakers.map((s) =>
      s.id === id ? { ...s, ignored: ignore, needs_name: !ignore && s.kind === "Other" && s.person === null } : s,
    ),
  };
}

/** `detail` after "This is me" on `id`: the meeting's one Me row, the user's name. */
export function markedMe(detail: MeetingDetail, id: string, user: PersonItem): MeetingDetail {
  const next = linked(detail, id, user);
  return {
    ...next,
    speakers: next.speakers.map((s) =>
      s.id === id
        ? { ...s, kind: "Me", can_mark_me: false, can_unmark_me: true, can_not_this_person: false }
        : { ...s, can_mark_me: false },
    ),
  };
}

/** `detail` after "Not <name>" on `id` (T12.77): unlinked, the smallest free "Speaker N",
 * ignored. */
export function notThisPersonVoice(detail: MeetingDetail, id: string): MeetingDetail {
  const taken = detail.speakers.filter((s) => s.id !== id).map((s) => s.name);
  let n = 1;
  while (taken.includes(`Speaker ${n}`)) n += 1;
  const next = renamed(detail, id, `Speaker ${n}`);
  return {
    ...next,
    speakers: next.speakers.map((s) =>
      s.id === id
        ? {
            ...s,
            person: null,
            source: s.source === "Voiceprint" ? "FarEnd" : s.source,
            needs_name: false,
            ignored: true,
            can_ignore: true,
            can_not_this_person: false,
            voice_saved_here: false,
          }
        : s,
    ),
  };
}

/** `detail` after "This isn't me" on `id`: an unnamed voice again, the smallest free number. */
export function unmarkedMe(detail: MeetingDetail, id: string): MeetingDetail {
  const taken = detail.speakers.filter((s) => s.id !== id).map((s) => s.name);
  let n = 1;
  while (taken.includes(`Speaker ${n}`)) n += 1;
  const next = renamed(detail, id, `Speaker ${n}`);
  return {
    ...next,
    speakers: next.speakers.map((s) =>
      s.id === id
        ? {
            ...s,
            kind: "Other",
            person: null,
            needs_name: true,
            can_ignore: true,
            can_not_this_person: false,
            can_mark_me: true,
            can_unmark_me: false,
            recognised_me: false,
          }
        : { ...s, can_mark_me: true },
    ),
  };
}
