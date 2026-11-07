// The Storage page commands (T7.4k), kept apart from ipc.ts so `IpcCommands` gains them in
// one line.

import type { RetentionCount, RetentionOption, StorageSaved, StorageView } from "./bindings";

export type { RetentionCount, RetentionOption, StorageSaved, StorageView };

/** The Storage page part of `IpcCommands`. */
export interface StorageCommands {
  /** How long recordings are kept, with every choice. */
  storageSettings(): Promise<StorageView>;
  /** How many meetings' recordings `retention` would delete now, and how many more once
   * they are not in use (deletes nothing). Rejects with `invalid_setting` for a value that
   * is not a choice. */
  previewRetention(retention: string): Promise<RetentionCount>;
  /** Stores `retention` and deletes the recordings it makes due now. Rejects with
   * `invalid_setting` (nothing stored); once stored it resolves, with `some_left` set when
   * some recordings could not be deleted now. */
  setStorageSettings(retention: string): Promise<StorageSaved>;
}
