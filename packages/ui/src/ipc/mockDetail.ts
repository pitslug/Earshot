// Sample meeting details for the mock IPC (component tests and the browser preview): a
// vendor call with a summary, two voices that need names and a voiceprint match.

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
    { id: "9e000000-0000-7000-8000-000000000001", name: "Casey", has_voiceprint: true, is_me: true },
    { id: "9e000000-0000-7000-8000-000000000002", name: "Priya Shah", has_voiceprint: true, is_me: false },
    { id: "9e000000-0000-7000-8000-000000000003", name: "Tom Reid", has_voiceprint: false, is_me: false },
  ];
}

const segmentId = (n: number) => `5e000000-0000-7000-8000-${String(n).padStart(12, "0")}`;

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
  ["second", 786_000, "Fine by us. I'll put it in writing."],
];

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
      track: who === "me" ? "You" : "Them",
      text,
      edited: false,
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
      can_mark_me: false,
      can_unmark_me: false,
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
      can_mark_me: false,
      can_unmark_me: false,
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
      can_mark_me: false,
      can_unmark_me: false,
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
      can_mark_me: false,
      can_unmark_me: false,
    },
  ];
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
      : r,
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
      summary_model: ready ? "Qwen 3.5 9B" : null,
      transcript_backend: "GPU (CUDA)",
    },
    summary: ready ? sampleSummary() : null,
    transcript,
    speakers,
    can_undo_summary: false,
    summary_stale: false,
    audio_kept: true,
    handovers: [],
    kept_local: null,
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
      can_mark_me: true,
    }),
  );
  return {
    ...d,
    header: { ...d.header, mode: "InRoom" },
    speakers,
    transcript: transcriptOf(LINES, speakers).map((r) => ({ ...r, track: "Room" })),
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
          }
        : s,
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
        ? { ...s, kind: "Me", can_mark_me: false, can_unmark_me: true }
        : { ...s, can_mark_me: false },
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
        ? { ...s, kind: "Other", person: null, needs_name: true, can_mark_me: true, can_unmark_me: false }
        : { ...s, can_mark_me: true },
    ),
  };
}
