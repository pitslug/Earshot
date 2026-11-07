// The Storage page part of the mock `Ipc` (T7.4k): the retention choices exactly as the app
// sends them (`retentionChoices.ts`, generated from Rust) and sample meetings whose clock
// started some time ago, so a shorter choice has something to delete.

import { IpcFailure } from "./ipc";
import { RETENTION_CHOICES } from "./retentionChoices";
import type { RetentionCount, StorageCommands, StorageSaved, StorageView } from "./storage";

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

/** The app's text when some due recordings could not be deleted now
 * (`storage_settings::SOME_LEFT_MESSAGE`). */
export const SOME_LEFT_MESSAGE = "Some recordings could not be deleted now. Earshot tries again later.";

/** A sample meeting with kept recordings. */
export interface MockKept {
  /** How long ago its clock started (summary stored), in ms. */
  age: number;
  /** A job is using it: counted apart, never deleted. */
  inUse?: boolean;
  /** Deleting its recordings fails (a file in use). */
  stuck?: boolean;
}

/** What the Storage mock starts with. */
export interface StorageMockData {
  /** The stored choice (default `7d`). */
  retention?: string;
  /** The meetings with kept recordings (default: clocks started 2 hours, 2 days and 5 days
   * ago, so 1 day deletes two and 1 hour deletes three). */
  kept?: MockKept[];
  /** The last automatic run's problem (default none). */
  problem?: string | null;
  /** Every call fails with this. */
  error?: string;
}

/** The Storage page of the mock `Ipc`. */
export function createStorageMock(data: StorageMockData = {}): StorageCommands {
  let retention = data.retention ?? "7d";
  let kept: MockKept[] = (data.kept ?? [{ age: 2 * HOUR }, { age: 2 * DAY }, { age: 5 * DAY }]).map((k) => ({
    ...k,
  }));
  const choice = (value: string) => {
    if (data.error) throw new IpcFailure({ kind: "store", message: data.error, retryable: false });
    const c = RETENTION_CHOICES.find((r) => r.value === value);
    if (c === undefined) {
      throw new IpcFailure({
        kind: "invalid_setting",
        message: "This is not one of the choices.",
        retryable: false,
      });
    }
    return c;
  };
  const due = (ms: number | null) => (ms === null ? [] : kept.filter((k) => k.age >= ms));
  const view = (): StorageView => ({
    retention,
    options: RETENTION_CHOICES.map((o) => ({ ...o })),
    unknown: [],
    problem: data.problem ?? null,
  });
  return {
    storageSettings: async () => {
      choice(retention);
      return view();
    },
    previewRetention: async (value): Promise<RetentionCount> => {
      const d = due(choice(value).keep_ms);
      return { meetings: d.filter((k) => !k.inUse).length, in_use: d.filter((k) => k.inUse).length };
    },
    setStorageSettings: async (value): Promise<StorageSaved> => {
      const c = choice(value);
      retention = value;
      const d = due(c.keep_ms);
      const gone = d.filter((k) => !k.inUse && !k.stuck);
      kept = kept.filter((k) => !gone.includes(k));
      return {
        view: view(),
        deleted: gone.length,
        some_left: gone.length < d.length ? SOME_LEFT_MESSAGE : null,
      };
    },
  };
}
