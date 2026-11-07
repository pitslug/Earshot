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

/** The list's second line: start time, length and where the meeting is in its life:
 * "HH:MM · Recording...", "HH:MM · N min · Processing", "HH:MM · Interrupted",
 * "HH:MM · N min · Draft transcript" or "HH:MM · N min". */
export function secondaryLine(m: MeetingListItem): string {
  const parts = [clockTime(m.started_at_ms)];
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

/** The title shown for a meeting (titles stay empty until the user or a summary gives one). */
export function displayTitle(m: Pick<MeetingListItem, "title">): string {
  return m.title.trim() === "" ? "Untitled meeting" : m.title;
}
