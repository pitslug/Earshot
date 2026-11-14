// Pure helpers of the meeting list: day groups and the secondary line.

import type { MeetingListItem } from "../ipc";

export type DayGroupLabel = "Today" | "Yesterday" | "Earlier";

export interface DayGroup {
  label: DayGroupLabel;
  meetings: MeetingListItem[];
}

function startOfLocalDay(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** Groups meetings (already newest first) by local calendar day relative to `now`; empty
 * groups are left out and the order inside a group is kept. */
export function groupByDay(meetings: MeetingListItem[], now: number): DayGroup[] {
  const today = startOfLocalDay(now);
  const yesterdayDate = new Date(today);
  yesterdayDate.setDate(yesterdayDate.getDate() - 1);
  const yesterday = yesterdayDate.getTime();
  const groups: DayGroup[] = [
    { label: "Today", meetings: [] },
    { label: "Yesterday", meetings: [] },
    { label: "Earlier", meetings: [] },
  ];
  for (const m of meetings) {
    const index = m.started_at_ms >= today ? 0 : m.started_at_ms >= yesterday ? 1 : 2;
    groups[index].meetings.push(m);
  }
  return groups.filter((g) => g.meetings.length > 0);
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

/** The list's second line: the date and start time it was recorded, its length and where the
 * meeting is in its life: "D Mon YYYY HH:MM · Recording...", "... · N min · Processing",
 * "... · Interrupted", "... · N min · Draft transcript" or "D Mon YYYY HH:MM · N min". */
export function secondaryLine(m: MeetingListItem): string {
  const parts = [`${recordedDate(m.started_at_ms)} ${clockTime(m.started_at_ms)}`];
  if (m.state === "Recording") return `${parts[0]} · Recording...`;
  if (m.duration_ms !== null) {
    parts.push(`${Math.max(1, Math.round(m.duration_ms / 60_000))} min`);
  }
  if (m.state === "Processing") parts.push("Processing");
  if (m.state === "Waiting") parts.push("Waiting to finish");
  if (m.state === "Interrupted") parts.push("Interrupted");
  if (m.state === "Unprocessed") parts.push("Draft transcript");
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

/** The title shown for a meeting (titles stay empty until the user or a summary gives one). */
export function displayTitle(m: Pick<MeetingListItem, "title">): string {
  return m.title.trim() === "" ? "Untitled meeting" : m.title;
}
