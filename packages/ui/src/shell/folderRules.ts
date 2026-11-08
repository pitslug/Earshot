// Pure helpers of the folders in the side panel (T9.5c, docs/folders-design.md).

import type { FolderDeleted, MeetingListItem } from "../ipc";

/** The meetings a folder shows: all of them for "All meetings" (`null`), else those filed in
 * it. The list carries the folder Rust resolved, so a meeting naming a deleted folder has none. */
export function meetingsInFolder(meetings: MeetingListItem[], folder: string | null): MeetingListItem[] {
  return folder === null ? meetings : meetings.filter((m) => m.folder_id === folder);
}

/** "1 meeting", "3 meetings". */
export function meetingCount(n: number): string {
  return n === 1 ? "1 meeting" : `${n} meetings`;
}

/** What the app says after deleting a folder with its meetings left some behind, or `null`
 * when the folder went. Counts only, never a name. */
export function folderDeletionCopy(d: FolderDeleted): string | null {
  if (d.folder_deleted) return null;
  const deleted =
    d.meetings_deleted === 0 ? "No meetings were deleted." : `Deleted ${meetingCount(d.meetings_deleted)}.`;
  if (d.meetings_left === 0) return `${deleted} The folder stays. Try deleting it again.`;
  const left =
    d.meetings_left === 1
      ? "1 could not be deleted because it is busy"
      : `${d.meetings_left} could not be deleted because they are busy`;
  return `${deleted} ${left}, so the folder stays.`;
}
