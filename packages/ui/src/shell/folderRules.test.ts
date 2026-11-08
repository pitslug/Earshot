import { describe, expect, it } from "vitest";

import type { MeetingListItem } from "../ipc";
import { folderDeletionCopy, meetingCount, meetingsInFolder } from "./folderRules";

const row = (id: string, folder: string | null): MeetingListItem => ({
  id,
  title: id,
  started_at_ms: 0,
  duration_ms: 60_000,
  state: "Ready",
  folder_id: folder,
});

describe("folders", () => {
  it("All meetings shows every meeting and a folder only its own", () => {
    const all = [row("a", "f1"), row("b", null), row("c", "f2"), row("d", "f1")];
    expect(meetingsInFolder(all, null)).toBe(all);
    expect(meetingsInFolder(all, "f1").map((m) => m.id)).toEqual(["a", "d"]);
    expect(meetingsInFolder(all, "f3")).toEqual([]);
  });

  it("counts meetings in words", () => {
    expect(meetingCount(1)).toBe("1 meeting");
    expect(meetingCount(0)).toBe("0 meetings");
    expect(meetingCount(4)).toBe("4 meetings");
  });

  it("says nothing when the folder went, and the counts when it stayed", () => {
    expect(
      folderDeletionCopy({ folder_deleted: true, meetings_deleted: 3, meetings_left: 0, meetings_unfiled: 0 }),
    ).toBeNull();
    expect(
      folderDeletionCopy({ folder_deleted: false, meetings_deleted: 2, meetings_left: 1, meetings_unfiled: 0 }),
    ).toBe("Deleted 2 meetings. 1 could not be deleted because it is busy, so the folder stays.");
    expect(
      folderDeletionCopy({ folder_deleted: false, meetings_deleted: 1, meetings_left: 3, meetings_unfiled: 0 }),
    ).toBe("Deleted 1 meeting. 3 could not be deleted because they are busy, so the folder stays.");
    expect(
      folderDeletionCopy({ folder_deleted: false, meetings_deleted: 0, meetings_left: 2, meetings_unfiled: 0 }),
    ).toBe("No meetings were deleted. 2 could not be deleted because they are busy, so the folder stays.");
    expect(
      folderDeletionCopy({ folder_deleted: false, meetings_deleted: 1, meetings_left: 0, meetings_unfiled: 0 }),
    ).toBe("Deleted 1 meeting. The folder stays. Try deleting it again.");
  });
});
