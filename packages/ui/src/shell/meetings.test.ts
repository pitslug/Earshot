import { describe, expect, it } from "vitest";

import type { MeetingListItem } from "../ipc";
import { clockTime, displayTitle, groupByDay, recoveredText, secondaryLine } from "./meetings";

// Local times, so the tests hold in any time zone.
const at = (day: number, hour: number, minute = 0) =>
  new Date(2026, 10, day, hour, minute).getTime();

function meeting(id: string, started: number, duration: number | null = 30 * 60_000): MeetingListItem {
  return { id, title: `Meeting ${id}`, started_at_ms: started, duration_ms: duration, state: "Ready", folder_id: null };
}

describe("groupByDay", () => {
  const now = at(2, 9, 15);

  it("splits meetings into today, yesterday and earlier by local day", () => {
    const list = [
      meeting("a", at(2, 8, 0)),
      meeting("b", at(2, 0, 0)),
      meeting("c", at(1, 23, 59)),
      meeting("d", at(1, 0, 0)),
      // 31 October 23:59, the evening before yesterday.
      meeting("e", new Date(2026, 9, 31, 23, 59).getTime()),
    ];
    const groups = groupByDay(list, now);
    expect(groups.map((g) => g.label)).toEqual(["Today", "Yesterday", "Earlier"]);
    expect(groups.map((g) => g.meetings.map((m) => m.id))).toEqual([["a", "b"], ["c", "d"], ["e"]]);
  });

  it("leaves out empty groups and keeps the given order", () => {
    const groups = groupByDay([meeting("x", at(1, 10)), meeting("y", at(1, 9))], now);
    expect(groups).toHaveLength(1);
    expect(groups[0].label).toBe("Yesterday");
    expect(groups[0].meetings.map((m) => m.id)).toEqual(["x", "y"]);
    expect(groupByDay([], now)).toEqual([]);
  });

  it("depends on the injected clock, not the real one", () => {
    const list = [meeting("a", at(2, 8))];
    expect(groupByDay(list, at(2, 23, 59))[0].label).toBe("Today");
    expect(groupByDay(list, at(3, 0, 0))[0].label).toBe("Yesterday");
    expect(groupByDay(list, at(4, 0, 0))[0].label).toBe("Earlier");
  });
});

describe("secondaryLine", () => {
  it("shows the local start time and the length in minutes", () => {
    expect(secondaryLine(meeting("a", at(2, 9, 5), 47 * 60_000))).toBe("09:05 · 47 min");
    expect(secondaryLine(meeting("a", at(2, 14, 30), 20_000))).toBe("14:30 · 1 min");
    expect(secondaryLine(meeting("a", at(2, 14, 30), 90 * 60_000 + 29_000))).toBe("14:30 · 90 min");
  });

  it("says where the meeting is in its life", () => {
    const m = meeting("a", at(2, 7, 0), 47 * 60_000);
    expect(secondaryLine({ ...m, duration_ms: null, state: "Recording" })).toBe(
      "07:00 · Recording...",
    );
    expect(secondaryLine({ ...m, state: "Processing" })).toBe("07:00 · 47 min · Processing");
    expect(secondaryLine({ ...m, duration_ms: null, state: "Interrupted" })).toBe(
      "07:00 · Interrupted",
    );
    expect(secondaryLine({ ...m, state: "Unprocessed" })).toBe("07:00 · 47 min · Draft transcript");
    expect(secondaryLine({ ...m, state: "Ready" })).toBe("07:00 · 47 min");
  });

  it("pads the clock", () => {
    expect(clockTime(at(2, 0, 3))).toBe("00:03");
  });
});

describe("displayTitle", () => {
  it("names untitled meetings", () => {
    expect(displayTitle({ ...meeting("a", 0), title: "" })).toBe("Untitled meeting");
    expect(displayTitle({ ...meeting("a", 0), title: "  " })).toBe("Untitled meeting");
    expect(displayTitle(meeting("a", 0))).toBe("Meeting a");
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
