// Pure helpers of the meeting list: day groups and the secondary line.

import type { MeetingListItem, PostStep } from "../ipc";
import { ORDER_COPY } from "../ipc/summaryOrder";

/** A group of the meeting list (T12.103). `key` stays the same whatever day it is ("today",
 * "yesterday", "this-week", "last-week", or "YYYY-MM" for a month), so a group the user closed
 * stays closed; `label` is what the list shows. */
export interface DayGroup {
  key: string;
  label: string;
  meetings: MeetingListItem[];
}

const MONTH_NAMES = [
  "January",
  "February",
  "March",
  "April",
  "May",
  "June",
  "July",
  "August",
  "September",
  "October",
  "November",
  "December",
];

function startOfLocalDay(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** The local midnight `days` calendar days after `dayStart` (negative: before). */
function addDays(dayStart: number, days: number): number {
  const d = new Date(dayStart);
  d.setDate(d.getDate() + days);
  return d.getTime();
}

/** Groups meetings (already newest first) relative to `now`, by local calendar day: Today,
 * Yesterday, Earlier this week, Last week (weeks start on Monday), then one group per month
 * ("October 2026"). Yesterday wins over Last week on a Monday. Empty groups are left out and the
 * order inside a group is kept. */
export function groupByDay(meetings: MeetingListItem[], now: number): DayGroup[] {
  const today = startOfLocalDay(now);
  const yesterday = addDays(today, -1);
  // Monday 0 ... Sunday 6.
  const weekday = (new Date(today).getDay() + 6) % 7;
  const thisWeek = addDays(today, -weekday);
  const lastWeek = addDays(thisWeek, -7);
  const groups: DayGroup[] = [];
  const groupFor = (key: string, label: string): DayGroup => {
    let g = groups.find((x) => x.key === key);
    if (g === undefined) {
      g = { key, label, meetings: [] };
      groups.push(g);
    }
    return g;
  };
  for (const m of meetings) {
    const t = m.started_at_ms;
    let group: DayGroup;
    if (t >= today) group = groupFor("today", "Today");
    else if (t >= yesterday) group = groupFor("yesterday", "Yesterday");
    else if (t >= thisWeek) group = groupFor("this-week", "Earlier this week");
    else if (t >= lastWeek) group = groupFor("last-week", "Last week");
    else {
      const d = new Date(t);
      const month = d.getMonth();
      const key = `${d.getFullYear()}-${String(month + 1).padStart(2, "0")}`;
      group = groupFor(key, `${MONTH_NAMES[month]} ${d.getFullYear()}`);
    }
    group.meetings.push(m);
  }
  return groups;
}

/** Local `HH:MM` (24-hour). */
export function clockTime(ms: number): string {
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** Local `D Mon YYYY` ("14 Nov 2026"), fixed English month names whatever the locale. */
export function recordedDate(ms: number): string {
  const d = new Date(ms);
  return `${d.getDate()} ${MONTHS[d.getMonth()]} ${d.getFullYear()}`;
}

/** What the work on a meeting is doing, as the window heard it (T12.76): the step of its work
 * after the meeting (`MeetingEvent::PostProgress`) or the job running on it. */
export type MeetingWork = "Transcribing" | "FindingSpeakers" | "Finishing" | "Summarising";

/** What the work after a meeting does once `step` was heard: reading the recordings back and
 * starting the final pass lead to transcribing; a transcribed track and found speakers say their
 * own step; the echo check and storing are the last steps before the summary ("Finishing...",
 * also after `Stored` when no summary step follows). */
export function workOfStep(step: PostStep): MeetingWork {
  switch (step.kind) {
    case "Loading":
    case "Starting":
    case "Transcribed":
      return "Transcribing";
    case "Diarised":
      return "FindingSpeakers";
    case "EchoChecked":
    case "Stored":
      return "Finishing";
    case "Summarising":
      return "Summarising";
    default: {
      const unknown: never = step;
      return unknown;
    }
  }
}

/** The dot beside a row's second line (T12.76), always with a word in that line saying the same:
 * recording (red), work after the meeting running or waiting (green), a summary this computer has
 * not opened (blue). */
export type RowDot = "recording" | "working" | "newSummary";

/** Whether the meeting's work is running or waiting: its listed state, work the window heard
 * of since the list was read (a job, or a waiting meeting's turn starting), or its summary being
 * written on another computer (T14.66). */
function working(m: MeetingListItem, work: MeetingWork | undefined): boolean {
  return work !== undefined || m.state === "Processing" || m.state === "Waiting" || m.summary_elsewhere;
}

/** The row's dot, `null` for none. */
export function rowDot(m: MeetingListItem, work?: MeetingWork): RowDot | null {
  if (m.state === "Recording") return "recording";
  if (working(m, work)) return "working";
  return m.new_summary ? "newSummary" : null;
}

/** The word for running or waiting work: what runs when the window heard it, else "Waiting..."
 * for a meeting queued behind an older one's work, else "Finishing..." (the window has not heard
 * which step runs, for example after it was reloaded). */
function workWord(m: MeetingListItem, work: MeetingWork | undefined): string {
  switch (work) {
    case "Transcribing":
      return "Transcribing...";
    case "FindingSpeakers":
      return "Finding speakers...";
    case "Finishing":
      return "Finishing...";
    case "Summarising":
      return "Summarising...";
    case undefined:
      // Local work wins: only a meeting with none heard says its summary is elsewhere (T14.66).
      if (m.state === "Waiting") return "Waiting...";
      if (m.state !== "Processing" && m.summary_elsewhere) return ORDER_COPY.list_elsewhere;
      return "Finishing...";
  }
}

/** The list's second line: the date and start time it was recorded, its length and where the
 * meeting is in its life: "D Mon YYYY HH:MM · Recording...", "... · N min · Transcribing..."
 * (or "Finding speakers...", "Summarising...", "Waiting...", "Finishing..."), "... · Interrupted",
 * "... · N min · Draft transcript", "... · N min · New summary" or "D Mon YYYY HH:MM · N min".
 * `work`: what the window heard the meeting's work doing (`MeetingWork`). */
export function secondaryLine(m: MeetingListItem, work?: MeetingWork): string {
  const parts = [`${recordedDate(m.started_at_ms)} ${clockTime(m.started_at_ms)}`];
  if (m.state === "Recording") return `${parts[0]} · Recording...`;
  if (m.duration_ms !== null) {
    parts.push(`${Math.max(1, Math.round(m.duration_ms / 60_000))} min`);
  }
  if (working(m, work)) return [...parts, workWord(m, work)].join(" · ");
  if (m.state === "Interrupted") parts.push("Interrupted");
  if (m.state === "Unprocessed") parts.push("Draft transcript");
  if (m.new_summary) parts.push("New summary");
  return parts.join(" · ");
}

/** What the window says once about a recording Earshot was closed during, whose audio
 * start-up recovery saved (T9.18). Whole minutes, never more than was saved. */
export function recoveredText(savedMs: number): string {
  const minutes = Math.floor(savedMs / 60_000);
  const saved =
    minutes < 1
      ? "Less than a minute was saved."
      : minutes === 1
        ? "1 minute was saved."
        : `${minutes} minutes were saved.`;
  return `Earshot closed during this recording. ${saved}`;
}

/** Whether two rows say the same: the same fields with the same values. The rows the app sends
 * hold only plain values; a field holding an object would read as changed every time, which
 * costs a render and never hides a change. */
function sameFields<T extends object>(a: T, b: T): boolean {
  const keys = Object.keys(a) as (keyof T)[];
  return keys.length === Object.keys(b).length && keys.every((k) => k in b && Object.is(a[k], b[k]));
}

/** `next` as the window keeps it after reading a list again (T14.16): every row that says the
 * same as the row with its id in `prev` is that row itself, and when every row is, in the same
 * order, the result is `prev` itself. The app sends new objects at every read, and the list is
 * read after every sync pass and every meeting's start, end and summary: without this each read
 * would render every row of the list again, and the open meeting with them. */
export function keepUnchanged<T extends { id: string }>(prev: T[], next: T[]): T[] {
  const before = new Map(prev.map((p) => [p.id, p]));
  let same = prev.length === next.length;
  const kept = next.map((n, i) => {
    const was = before.get(n.id);
    const row = was !== undefined && sameFields(was, n) ? was : n;
    if (row !== prev[i]) same = false;
    return row;
  });
  return same ? prev : kept;
}

/** The title shown for a meeting (titles stay empty until the user or a summary gives one). */
export function displayTitle(m: Pick<MeetingListItem, "title">): string {
  return m.title.trim() === "" ? "Untitled meeting" : m.title;
}
