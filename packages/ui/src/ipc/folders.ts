// The folder commands (T9.5c), kept apart from ipc.ts so `IpcCommands` gains them in one line.
// Every folder read and edit is `earshot_runtime::folders`; refusals are fixed copy.

import type { FolderDeleted, FolderListItem } from "./bindings";

export type { FolderDeleted, FolderListItem };

/** The folder part of `IpcCommands`. */
export interface FolderCommands {
  /** Every live folder, by name (case ignored). */
  listFolders(): Promise<FolderListItem[]>;
  /** Creates a folder and resolves with its id. Rejects with `invalid_name` or
   * `folder_name_taken` (fixed copy that never quotes the name). */
  createFolder(name: string): Promise<string>;
  /** Renames a folder. Rejects with `invalid_name`, `folder_name_taken`, `folder_not_found`. */
  renameFolder(folder: string, name: string): Promise<void>;
  /** Moves a meeting into a folder, or out of any (`null`). Rejects with `meeting_recording`
   * while it records, `folder_not_found`, `meeting_not_found`. */
  moveToFolder(meeting: string, folder: string | null): Promise<void>;
  /** Deletes a folder; its meetings stay and show in "All meetings". */
  deleteFolderKeepingMeetings(folder: string): Promise<FolderDeleted>;
  /** Deletes a folder and its meetings. A busy meeting stays and so does the folder: the
   * result says how many were deleted and how many are left. */
  deleteFolderWithMeetings(folder: string): Promise<FolderDeleted>;
}
