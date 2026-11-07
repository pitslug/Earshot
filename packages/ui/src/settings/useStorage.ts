// The Storage page's state: the stored retention choice, a choice waiting for the user to
// confirm the recordings it deletes, and the notices. The counts come from the app's own
// preview (the plan of the deletion); nothing is decided here. After every save, whatever
// its outcome, the page shows the choice the app reports as stored.

import { useCallback, useEffect, useState } from "react";

import { type RetentionCount, type StorageView, useIpc } from "../ipc";
import { errorText } from "./useModels";

export type StorageLoad =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "loaded"; view: StorageView };

/** A choice that deletes recordings, waiting for Delete or Cancel. */
export interface PendingChoice {
  value: string;
  label: string;
  count: RetentionCount;
}

export interface StorageNotice {
  intent: "success" | "warning" | "error";
  title?: string;
  message: string;
}

export interface StoragePageState {
  load: StorageLoad;
  pending: PendingChoice | null;
  notice: StorageNotice | null;
  busy: boolean;
  /** Picks a choice: saved at once, or held in `pending` when it deletes recordings. */
  choose(value: string): Promise<void>;
  /** Saves the pending choice (its recordings are deleted). */
  confirm(): Promise<void>;
  /** Drops the pending choice; the stored one stays. */
  cancel(): void;
  dismiss(): void;
}

/** "1 meeting" / "3 meetings". */
export function meetings(n: number): string {
  return n === 1 ? "1 meeting" : `${n} meetings`;
}

/** What the confirmation says a choice deletes (the count is meetings). */
export function deleteText(c: RetentionCount): string {
  if (c.in_use === 0) return `Recordings of ${meetings(c.meetings)} will be deleted now.`;
  if (c.meetings === 0) {
    return `Recordings of ${meetings(c.in_use)} will be deleted as soon as they are no longer in use.`;
  }
  return `Recordings of ${meetings(c.meetings)} will be deleted now, and of ${meetings(c.in_use)} more as soon as they are no longer in use.`;
}

/** What the page says after a save deleted `deleted` meetings' recordings. */
export function deletedText(deleted: number): string {
  return `Recordings of ${meetings(deleted)} deleted.`;
}

/** Loads the Storage page and saves its changes. */
export function useStorage(): StoragePageState {
  const ipc = useIpc();
  const [load, setLoad] = useState<StorageLoad>({ status: "loading" });
  const [pending, setPending] = useState<PendingChoice | null>(null);
  const [notice, setNotice] = useState<StorageNotice | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    ipc.storageSettings().then(
      (view) => {
        if (live) setLoad({ status: "loaded", view });
      },
      (e: unknown) => {
        if (live) setLoad({ status: "failed", message: errorText(e) });
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  const save = useCallback(
    async (value: string) => {
      setBusy(true);
      try {
        const saved = await ipc.setStorageSettings(value);
        setLoad({ status: "loaded", view: saved.view });
        const done = saved.deleted > 0 ? deletedText(saved.deleted) : null;
        if (saved.some_left !== null) {
          setNotice({ intent: "warning", message: done === null ? saved.some_left : `${done} ${saved.some_left}` });
        } else {
          setNotice(done === null ? null : { intent: "success", message: done });
        }
      } catch (e) {
        setNotice({ intent: "error", title: "This setting was not saved", message: errorText(e) });
        // Show what is stored, whatever the failure was.
        try {
          setLoad({ status: "loaded", view: await ipc.storageSettings() });
        } catch {
          // The last view stays; the notice says why.
        }
      } finally {
        setBusy(false);
        setPending(null);
      }
    },
    [ipc],
  );

  const choose = useCallback(
    async (value: string) => {
      if (load.status !== "loaded" || value === load.view.retention) return;
      const label = load.view.options.find((o) => o.value === value)?.label ?? value;
      setBusy(true);
      let count: RetentionCount;
      try {
        count = await ipc.previewRetention(value);
      } catch (e) {
        setNotice({ intent: "error", title: "This setting was not saved", message: errorText(e) });
        setBusy(false);
        return;
      }
      setBusy(false);
      if (count.meetings + count.in_use > 0) {
        setPending({ value, label, count });
      } else {
        await save(value);
      }
    },
    [ipc, load, save],
  );

  const confirm = useCallback(async () => {
    if (pending !== null) await save(pending.value);
  }, [pending, save]);

  return {
    load,
    pending,
    notice,
    busy,
    choose,
    confirm,
    cancel: () => setPending(null),
    dismiss: () => setNotice(null),
  };
}
