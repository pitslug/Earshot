// Moving and splitting a transcript line (T12.90, T12.90a): the mock against the shared case
// table, which `detail_line_edits_match_the_shared_cases` (apps/desktop/src-tauri, through the
// shipped `edit::move_line`, `split_line`, `edit_segment`, `movable_speakers`, `can_split_line`
// and `meeting_detail`) also runs. The mock is the browser preview's server: an offer or a
// refusal that differs from the app's would show a move the app refuses.

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/line_edit_cases.json";
import type { MeetingDetail, MeetingListItem, SpeakerCard, TranscriptRow } from "./ipc";
import { IpcFailure } from "./ipc";
import { createMockIpc } from "./mock";
import { type MockRole, SAMPLE_TRACKS } from "./mockDetail";

type Case = (typeof table.cases)[number];
/** A step of a case (the JSON's union, read as one shape). */
interface Step {
  op: string;
  line: string;
  to?: string;
  token?: number;
  text?: string;
  result: string;
  error?: string;
  new_speaker?: { as: string; name: string; role: string };
  split?: { as: string; texts: string[] };
  offers?: Record<string, string[]>;
  splittable?: Record<string, boolean>;
  listed?: string[];
  by_track?: Record<string, string[]>;
}

const ITEM: MeetingListItem = {
  id: "line-case",
  title: "Line edits",
  started_at_ms: 0,
  duration_ms: 60_000,
  state: "Ready",
  folder_id: null,
  new_summary: false,
};

/** The card a speaker row of role `role` shows (`detail::meeting_detail`): the user's row is Me,
 * a far-end voice comes from the computer's audio, every other from the mic. */
function cardOf(s: Case["speakers"][number]): SpeakerCard {
  const role = s.role as MockRole;
  return {
    id: s.id,
    name: s.name,
    kind: role === "Me" ? "Me" : "Other",
    person: s.person ? { id: `person-${s.id}`, name: s.name, has_voiceprint: false } : null,
    source: role === "Remote" ? "FarEnd" : "Mic",
    share_percent: 0,
    sample_line: "",
    needs_name: !s.person && role !== "Me",
    ignored: false,
    can_ignore: false,
    can_not_this_person: false,
    voice_saved_here: false,
    can_mark_me: false,
    can_unmark_me: false,
    recognised_me: false,
  };
}

/** Case `c` as the mock's stored detail: every speaker row's card, every line. */
function detailOf(c: Case): MeetingDetail {
  const names = new Map(c.speakers.map((s) => [s.id, s.name]));
  const transcript: TranscriptRow[] = c.lines.map((l, i) => ({
    segment_id: l.id,
    start_ms: 1_000 + 6_000 * i,
    speaker_id: l.speaker,
    speaker: l.speaker === null ? "Unknown speaker" : (names.get(l.speaker) ?? ""),
    unknown_speaker: l.speaker === null,
    track: c.in_room ? "Room" : l.track === "mic" ? "You" : "Them",
    track_id: l.track === "mic" ? SAMPLE_TRACKS.mic : SAMPLE_TRACKS.far,
    text: l.text,
    edited: false,
    splittable: false,
    suggestions: [],
    changed_words: [],
  }));
  return {
    id: ITEM.id,
    state: "Ready",
    header: {
      title: ITEM.title,
      started_at_ms: 0,
      duration_ms: 60_000,
      mode: c.in_room ? "InRoom" : "Call",
      transcript: c.draft ? "Draft" : "Final",
      summary_writer: null,
      summary_edited: false,
      transcript_backend: null,
    },
    summary: null,
    transcript,
    movable_by_track: {},
    speakers: c.speakers.map(cardOf),
    can_undo_summary: false,
    summary_stale: false,
    summary_stale_names: false,
    audio_kept: true,
    exported: false,
    waiting_for: null,
    summary_run: null,
    handovers: [],
    kept_local: null,
    word_list: [],
    dictionary: [],
  };
}

/** What line `segment` offers in its Move to speaker menu: its track's list without its own
 * speaker, as `MeetingDetailView` reads it. */
function offersOf(d: MeetingDetail, segment: string) {
  const r = d.transcript.find((x) => x.segment_id === segment);
  if (r === undefined) throw new Error(`no line ${segment}`);
  return (d.movable_by_track[r.track_id] ?? []).filter((s) => s.id !== r.speaker_id);
}

describe("line edits", () => {
  for (const c of table.cases) {
    it(`line edits match the shared cases: ${c.name}`, async () => {
      const ipc = createMockIpc({
        meetings: [ITEM],
        details: { [ITEM.id]: detailOf(c) },
        lineWords: Object.fromEntries(c.lines.map((l) => [l.id, l.words])),
      });
      // Labels of the rows a step made (new speakers, split parts), by their mock ids.
      const speakerLabel = new Map(c.speakers.map((s) => [s.id, s.id]));
      const lineId = new Map(c.lines.map((l) => [l.id, l.id]));
      const names = new Map(c.speakers.map((s) => [s.id, s.name]));
      const label = (id: string) => speakerLabel.get(id) ?? `unlabelled ${id}`;

      const check = async (expect_: Pick<Step, "offers" | "splittable" | "listed" | "by_track">, at: string) => {
        const d = await ipc.getMeeting(ITEM.id);
        for (const [line, want] of Object.entries(expect_.offers ?? {})) {
          const shown = offersOf(d, lineId.get(line) ?? line);
          expect(shown.map((s) => label(s.id)), `${at}: offers of ${line}`).toEqual(want);
          for (const s of shown) expect(s.name, `${at}: name of ${s.id}`).toBe(names.get(label(s.id)));
        }
        for (const [line, want] of Object.entries(expect_.splittable ?? {})) {
          const r = d.transcript.find((x) => x.segment_id === (lineId.get(line) ?? line));
          expect(r?.splittable, `${at}: splittable ${line}`).toBe(want);
        }
        for (const [track, want] of Object.entries(expect_.by_track ?? {})) {
          const id = track === "mic" ? SAMPLE_TRACKS.mic : SAMPLE_TRACKS.far;
          const listed = d.movable_by_track[id];
          expect(listed, `${at}: a list for the ${track} track`).toBeDefined();
          expect((listed ?? []).map((s) => label(s.id)), `${at}: by_track ${track}`).toEqual(want);
        }
        if (expect_.listed !== undefined) {
          expect(d.speakers.map((s) => label(s.id)), `${at}: speakers listed`).toEqual(expect_.listed);
        }
      };

      // The start is checked as a step is: the JSON infers a different type per case.
      await check(c as unknown as Step, `${c.name}, at the start`);
      for (const [k, step] of (c.steps as Step[]).entries()) {
        const at = `${c.name}, step ${k + 1}`;
        const line = lineId.get(step.line) ?? step.line;
        const before = await ipc.getMeeting(ITEM.id);
        let outcome: Promise<unknown>;
        if (step.op === "move") {
          const to = step.to ?? "";
          const target = [...speakerLabel].find(([, l]) => l === to)?.[0] ?? to;
          outcome = ipc.moveLine(ITEM.id, line, to === "new" ? null : target);
        } else if (step.op === "split") {
          outcome = ipc.splitLine(ITEM.id, line, step.token ?? -1);
        } else if (step.op === "edit") {
          outcome = ipc.editSegment(ITEM.id, line, step.text ?? "");
        } else {
          throw new Error(`${at}: unknown op ${step.op}`);
        }
        if (step.result === "error") {
          const e = await outcome.then(
            () => null,
            (e: unknown) => e,
          );
          expect(e, at).toBeInstanceOf(IpcFailure);
          expect((e as IpcFailure).error.kind, at).toBe(step.error);
          continue;
        }
        const value = await outcome;
        const after = await ipc.getMeeting(ITEM.id);
        const changed = JSON.stringify(after) !== JSON.stringify(before);
        if (step.result === "unchanged") {
          expect(changed, `${at}: nothing written`).toBe(false);
        } else if (step.result === "moved") {
          expect(changed, `${at}: written`).toBe(true);
        } else if (step.result === "new_speaker" && step.new_speaker !== undefined) {
          // The new row's card: it has the moved line, so the detail lists it.
          const known = new Set(before.speakers.map((s) => s.id).concat([...speakerLabel.keys()]));
          const made = after.speakers.filter((s) => !known.has(s.id));
          expect(made, `${at}: one new speaker`).toHaveLength(1);
          const want = step.new_speaker;
          const role = want.role as MockRole;
          expect(made[0].name, at).toBe(want.name);
          expect(made[0].kind, `${at}: never a second Me`).toBe("Other");
          expect(made[0].source, `${at}: role ${role}`).toBe(role === "Remote" ? "FarEnd" : "Mic");
          expect(made[0].person, at).toBeNull();
          speakerLabel.set(made[0].id, want.as);
          names.set(want.as, want.name);
        } else if (step.result === "split" && step.split !== undefined) {
          const second = value as string;
          const text = (id: string) => after.transcript.find((r) => r.segment_id === id)?.text;
          expect([text(line), text(second)], at).toEqual(step.split.texts);
          lineId.set(step.split.as, second);
        } else {
          throw new Error(`${at}: unknown result ${step.result}`);
        }
        await check(step, at);
      }
    });
  }
});
