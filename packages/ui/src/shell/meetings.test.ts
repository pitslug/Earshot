import { describe, expect, it } from "vitest";

import type { MeetingListItem, PostStep } from "../ipc";
import {
  clockTime,
  displayTitle,
  groupByDay,
  keepUnchanged,
  recordedDate,
  recoveredText,
  rowDot,
  secondaryLine,
  workOfStep,
} from "./meetings";

// Local times, so the tests hold in any time zone.
const at = (day: number, hour: number, minute = 0) =>
  new Date(2026, 10, day, hour, minute).getTime();

function meeting(id: string, started: number, duration: number | null = 30 * 60_000): MeetingListItem {
  return { id, title: `Meeting ${id}`, started_at_ms: started, duration_ms: duration, state: "Ready", folder_id: null, new_summary: false, summary_elsewhere: false };
}

describe("groupByDay", () => {
  // Wednesday 18 November 2026, 09:15 local; weeks start on Monday (16 November).
  const now = at(18, 9, 15);
  const keys = (list: MeetingListItem[], when = now) => groupByDay(list, when).map((g) => g.key);
  const labels = (list: MeetingListItem[], when = now) => groupByDay(list, when).map((g) => g.label);

  it("splits meetings into today, yesterday, earlier this week, last week and months", () => {
    const list = [
      meeting("a", at(18, 8, 0)),
      meeting("b", at(18, 0, 0)),
      meeting("c", at(17, 23, 59)),
      meeting("d", at(16, 0, 0)), // Monday: earlier this week
      meeting("e", at(15, 23, 59)), // Sunday: last week
      meeting("f", at(9, 0, 0)), // Monday a week ago: last week
      meeting("g", at(8, 23, 59)), // before last week, this month
      meeting("h", new Date(2026, 9, 31, 23, 59).getTime()),
      meeting("i", new Date(2025, 11, 3, 10).getTime()),
    ];
    const groups = groupByDay(list, now);
    expect(groups.map((g) => g.label)).toEqual([
      "Today",
      "Yesterday",
      "Earlier this week",
      "Last week",
      "November 2026",
      "October 2026",
      "December 2025",
    ]);
    expect(groups.map((g) => g.meetings.map((m) => m.id))).toEqual([
      ["a", "b"],
      ["c"],
      ["d"],
      ["e", "f"],
      ["g"],
      ["h"],
      ["i"],
    ]);
    // Keys stay the same whatever day it is, so a closed group stays closed: months by year and
    // month, the others by name.
    expect(groups.map((g) => g.key)).toEqual([
      "today",
      "yesterday",
      "this-week",
      "last-week",
      "2026-11",
      "2026-10",
      "2025-12",
    ]);
  });

  it("puts last week across a month boundary in Last week, then the months", () => {
    // Tuesday 3 November 2026: yesterday is Monday 2 November, last week 26 Oct to 1 Nov.
    const tue = at(3, 12);
    const list = [
      meeting("a", at(2, 10)),
      meeting("b", at(1, 10)),
      meeting("c", new Date(2026, 9, 26, 0, 0).getTime()),
      meeting("d", new Date(2026, 9, 25, 23, 59).getTime()),
    ];
    expect(labels(list, tue)).toEqual(["Yesterday", "Last week", "October 2026"]);
    expect(groupByDay(list, tue).map((g) => g.meetings.map((m) => m.id))).toEqual([["a"], ["b", "c"], ["d"]]);
  });

  it("on a Monday has no earlier this week, and yesterday is last week's Sunday", () => {
    const mon = at(16, 9);
    const list = [meeting("a", at(15, 10)), meeting("b", at(14, 10)), meeting("c", at(9, 10)), meeting("d", at(8, 10))];
    expect(labels(list, mon)).toEqual(["Yesterday", "Last week", "November 2026"]);
    expect(groupByDay(list, mon).map((g) => g.meetings.map((m) => m.id))).toEqual([["a"], ["b", "c"], ["d"]]);
  });

  it("leaves out empty groups and keeps the given order", () => {
    const groups = groupByDay([meeting("x", at(17, 10)), meeting("y", at(17, 9))], now);
    expect(groups).toHaveLength(1);
    expect(groups[0].label).toBe("Yesterday");
    expect(groups[0].meetings.map((m) => m.id)).toEqual(["x", "y"]);
    expect(groupByDay([], now)).toEqual([]);
  });

  it("depends on the injected clock, not the real one", () => {
    const list = [meeting("a", at(18, 8))];
    expect(keys(list, at(18, 23, 59))).toEqual(["today"]);
    expect(keys(list, at(19, 0, 0))).toEqual(["yesterday"]);
    expect(keys(list, at(20, 0, 0))).toEqual(["this-week"]);
    expect(keys(list, at(23, 0, 0))).toEqual(["last-week"]);
    expect(keys(list, at(30, 0, 0))).toEqual(["2026-11"]);
  });
});

describe("secondaryLine", () => {
  it("shows the local date, start time and the length in minutes", () => {
    expect(secondaryLine(meeting("a", at(2, 9, 5), 47 * 60_000))).toBe("2 Nov 2026 09:05 · 47 min");
    expect(secondaryLine(meeting("a", at(2, 14, 30), 20_000))).toBe("2 Nov 2026 14:30 · 1 min");
    expect(secondaryLine(meeting("a", at(2, 14, 30), 90 * 60_000 + 29_000))).toBe("2 Nov 2026 14:30 · 90 min");
  });

  it("says where the meeting is in its life", () => {
    const m = meeting("a", at(2, 7, 0), 47 * 60_000);
    expect(secondaryLine({ ...m, duration_ms: null, state: "Recording" })).toBe(
      "2 Nov 2026 07:00 · Recording...",
    );
    expect(secondaryLine({ ...m, duration_ms: null, state: "Interrupted" })).toBe(
      "2 Nov 2026 07:00 · Interrupted",
    );
    expect(secondaryLine({ ...m, state: "Unprocessed" })).toBe("2 Nov 2026 07:00 · 47 min · Draft transcript");
    expect(secondaryLine({ ...m, state: "Ready" })).toBe("2 Nov 2026 07:00 · 47 min");
  });

  it("says truthfully what the work after the meeting does (T12.76)", () => {
    const m = meeting("a", at(2, 7, 0), 47 * 60_000);
    const processing = { ...m, state: "Processing" as const };
    const waiting = { ...m, state: "Waiting" as const };
    // Not heard which step runs (a window opened mid-way): one honest word.
    expect(secondaryLine(processing)).toBe("2 Nov 2026 07:00 · 47 min · Finishing...");
    expect(secondaryLine(waiting)).toBe("2 Nov 2026 07:00 · 47 min · Waiting...");
    // "Summarising..." only while the summary step runs.
    expect(secondaryLine(processing, "Transcribing")).toBe("2 Nov 2026 07:00 · 47 min · Transcribing...");
    expect(secondaryLine(processing, "Summarising")).toBe("2 Nov 2026 07:00 · 47 min · Summarising...");
    // A waiting meeting whose turn started, and a job on a ready meeting, say what runs.
    expect(secondaryLine(waiting, "Transcribing")).toBe("2 Nov 2026 07:00 · 47 min · Transcribing...");
    expect(secondaryLine({ ...m, new_summary: true, summary_elsewhere: false }, "Summarising")).toBe(
      "2 Nov 2026 07:00 · 47 min · Summarising...",
    );
    for (const [item, work] of [
      [processing, undefined],
      [waiting, undefined],
      [processing, "Summarising"],
      [m, "Transcribing"],
    ] as const) {
      expect(rowDot(item, work)).toBe("working");
    }
  });

  it("names each step of the work after a meeting truthfully (T12.76)", () => {
    const m = { ...meeting("a", at(2, 7, 0), 47 * 60_000), state: "Processing" as const };
    const steps: [PostStep, string][] = [
      [{ kind: "Loading" }, "Transcribing..."],
      [{ kind: "Starting" }, "Transcribing..."],
      [{ kind: "Transcribed", track: "You" }, "Transcribing..."],
      [{ kind: "Transcribed", track: "Them" }, "Transcribing..."],
      [{ kind: "Diarised", track: "You" }, "Finding speakers..."],
      [{ kind: "Diarised", track: "Them" }, "Finding speakers..."],
      [{ kind: "EchoChecked" }, "Finishing..."],
      // Stored with no summary step after it: still finishing, never "Summarising...".
      [{ kind: "Stored" }, "Finishing..."],
      [{ kind: "Summarising" }, "Summarising..."],
    ];
    for (const [step, word] of steps) {
      expect(secondaryLine(m, workOfStep(step)), step.kind).toBe(`2 Nov 2026 07:00 · 47 min · ${word}`);
      expect(rowDot(m, workOfStep(step))).toBe("working");
    }
  });

  it("marks a summary this computer has not opened (T12.76)", () => {
    const m = { ...meeting("a", at(2, 7, 0), 47 * 60_000), new_summary: true, summary_elsewhere: false };
    expect(secondaryLine(m)).toBe("2 Nov 2026 07:00 · 47 min · New summary");
    expect(rowDot(m)).toBe("newSummary");
    expect(secondaryLine({ ...m, state: "Unprocessed" })).toBe(
      "2 Nov 2026 07:00 · 47 min · Draft transcript · New summary",
    );
    // Recording and running work come first; a read summary has no dot and no word.
    expect(rowDot({ ...m, state: "Recording" })).toBe("recording");
    expect(rowDot({ ...m, state: "Processing" })).toBe("working");
    expect(rowDot({ ...m, new_summary: false, summary_elsewhere: false })).toBeNull();
    expect(secondaryLine({ ...m, new_summary: false, summary_elsewhere: false })).toBe("2 Nov 2026 07:00 · 47 min");
    expect(rowDot({ ...m, duration_ms: null, state: "Recording" })).toBe("recording");
  });

  it("pads the clock", () => {
    expect(clockTime(at(2, 0, 3))).toBe("00:03");
  });

  it("names the local recording date with English month names", () => {
    expect(recordedDate(at(2, 0, 3))).toBe("2 Nov 2026");
    expect(recordedDate(new Date(2025, 0, 31, 23, 59).getTime())).toBe("31 Jan 2025");
  });
});

describe("displayTitle", () => {
  it("names untitled meetings", () => {
    expect(displayTitle({ ...meeting("a", 0), title: "" })).toBe("Untitled meeting");
    expect(displayTitle({ ...meeting("a", 0), title: "  " })).toBe("Untitled meeting");
    expect(displayTitle(meeting("a", 0))).toBe("Meeting a");
  });
});

describe("keepUnchanged (T14.16)", () => {
  const a = meeting("a", at(18, 9));
  const b = meeting("b", at(17, 9));
  const c = meeting("c", at(16, 9));
  /** The same rows as the app sends them again: equal, never the same objects. */
  const again = (list: MeetingListItem[]) => list.map((m) => ({ ...m }));

  it("gives back the list itself when a new read says the same", () => {
    const was = [a, b, c];
    expect(keepUnchanged(was, again(was))).toBe(was);
    expect(keepUnchanged<MeetingListItem>([], [])).toEqual([]);
  });

  it("keeps the rows that did not change and takes the one that did", () => {
    const was = [a, b, c];
    const renamed = { ...b, title: "Renamed" };
    const now = keepUnchanged(was, [{ ...a }, renamed, { ...c }]);
    expect(now).not.toBe(was);
    expect(now[0]).toBe(a);
    expect(now[1]).toBe(renamed);
    expect(now[2]).toBe(c);
  });

  it("takes a row for changed whichever field differs", () => {
    const changes: Partial<MeetingListItem>[] = [
      { title: "Other" },
      { started_at_ms: a.started_at_ms + 1 },
      { duration_ms: null },
      { state: "Processing" },
      { folder_id: "f1" },
      { new_summary: true, summary_elsewhere: false },
    ];
    for (const change of changes) {
      const next = { ...a, ...change };
      expect(keepUnchanged([a], [next])[0]).toBe(next);
    }
  });

  it("keeps rows across a new order, a new row and a row that went", () => {
    const was = [a, b, c];
    const reordered = keepUnchanged(was, again([c, a, b]));
    expect(reordered).not.toBe(was);
    expect(reordered).toEqual([c, a, b]);
    expect(reordered[0]).toBe(c);
    expect(reordered[1]).toBe(a);
    const d = meeting("d", at(18, 10));
    const added = keepUnchanged(was, [d, ...again(was)]);
    expect(added).toHaveLength(4);
    expect(added[0]).toBe(d);
    expect(added[1]).toBe(a);
    const fewer = keepUnchanged(was, again([a, c]));
    expect(fewer).not.toBe(was);
    expect(fewer).toHaveLength(2);
    expect(fewer[0]).toBe(a);
    expect(fewer[1]).toBe(c);
  });

  it("takes a row with a field more or a field fewer as changed", () => {
    const wider = { ...a, extra: 1 } as MeetingListItem;
    expect(keepUnchanged([a], [wider])[0]).toBe(wider);
    expect(keepUnchanged([wider], [{ ...a }])[0]).not.toBe(wider);
  });

  it("works for folders too", () => {
    const was = [
      { id: "f1", name: "Planning" },
      { id: "f2", name: "Supplier calls" },
    ];
    expect(keepUnchanged(was, was.map((f) => ({ ...f })))).toBe(was);
    const now = keepUnchanged(was, [{ ...was[0] }, { id: "f2", name: "Suppliers" }]);
    expect(now[0]).toBe(was[0]);
    expect(now[1]).not.toBe(was[1]);
  });
});

describe("recoveredText", () => {
  it("says how many whole minutes were saved, never more", () => {
    const head = "Earshot closed during this recording.";
    expect(recoveredText(0)).toBe(`${head} Less than a minute was saved.`);
    expect(recoveredText(59_999)).toBe(`${head} Less than a minute was saved.`);
    expect(recoveredText(60_000)).toBe(`${head} 1 minute was saved.`);
    expect(recoveredText(119_999)).toBe(`${head} 1 minute was saved.`);
    expect(recoveredText(12 * 60_000 + 59_000)).toBe(`${head} 12 minutes were saved.`);
  });
});

describe("a summary written on another computer (T14.66)", () => {
  const away = { ...meeting("a", at(18, 8)), summary_elsewhere: true };
  it("shows the working dot and says so", () => {
    expect(rowDot(away)).toBe("working");
    expect(secondaryLine(away)).toContain("Summarising on another computer...");
  });

  it("gives way to local work the window heard (control)", () => {
    expect(secondaryLine(away, "Transcribing")).toContain("Transcribing...");
    expect(secondaryLine(away, "Transcribing")).not.toContain("another computer");
    expect(rowDot({ ...away, summary_elsewhere: false })).toBeNull();
  });
});

