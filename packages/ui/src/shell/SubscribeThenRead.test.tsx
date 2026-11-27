// The window first reads its meeting list only once its sync subscription is in place, so a
// change made at start-up (the removal repair, a first sync pass) is never shown stale (T14.49f).

import { act, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { type Ipc, type MeetingListItem, type SyncEvent, createMockIpc } from "../ipc";
import { renderApp } from "../test/app";
import { createManualTimer } from "../time";

const at = (hour: number) => new Date(2026, 10, 2, hour, 0).getTime();
const NOW = at(16);

const FIRST: MeetingListItem = { id: "m-first", title: "Gravel path quote", started_at_ms: at(14), duration_ms: 30 * 60_000, state: "Ready", folder_id: null, new_summary: false, summary_elsewhere: false };
const SECOND: MeetingListItem = { id: "m-second", title: "Hedge trimming", started_at_ms: at(11), duration_ms: 20 * 60_000, state: "Ready", folder_id: null, new_summary: false, summary_elsewhere: false };

/** An app whose sync subscription resolves (or fails) only when the test says so. */
function open() {
  const timer = createManualTimer(NOW);
  const base = createMockIpc({ timer, now: NOW, meetings: [FIRST, SECOND] });
  let handler: ((event: SyncEvent) => void) | null = null;
  let settle: { resolve: (stop: () => void) => void; reject: (e: unknown) => void } | null = null;
  const reads = { n: 0, beforeSubscribed: 0 };
  let subscribed = false;
  const ipc: Ipc = {
    ...base,
    listMeetings: async () => {
      reads.n += 1;
      if (!subscribed) reads.beforeSubscribed += 1;
      return base.listMeetings();
    },
    onSyncEvent: (h) => {
      handler = h;
      return new Promise<() => void>((resolve, reject) => {
        settle = { resolve, reject };
      });
    },
  };
  renderApp(ipc, { timer, now: () => NOW });
  return {
    base,
    reads,
    send: (event: SyncEvent) => act(() => handler?.(event)),
    subscribe: async () => {
      subscribed = true;
      await act(async () => settle?.resolve(() => {}));
    },
    refuse: async () => {
      subscribed = true;
      await act(async () => settle?.reject(new Error("invented listener failure")));
    },
  };
}

const list = () => screen.getByRole("list", { name: "Meetings" });

describe("the first list read", () => {
  it("waits for the sync subscription, and a change heard before it is in the list", async () => {
    const s = open();
    await act(async () => {});
    expect(s.reads.n).toBe(0);

    // A start-up repair renames a meeting and says so between the subscription's call and its
    // answer: nothing is read yet, so nothing stale is shown.
    await s.base.renameMeeting(SECOND.id, "Hedge trimming, back fence");
    s.send({ meetings: [SECOND.id], people: true });
    await act(async () => {});
    expect(s.reads.n).toBe(0);

    // One flush after the subscription is in place is enough for the first read (the control
    // for the two flushes above), and it already holds the change.
    await s.subscribe();
    expect(s.reads.n).toBeGreaterThanOrEqual(1);
    expect(s.reads.beforeSubscribed).toBe(0);
    expect(await within(list()).findByRole("button", { name: /^Hedge trimming, back fence/ })).toBeInTheDocument();
    expect(within(list()).queryByRole("button", { name: /^Hedge trimming(?!, back fence)/ })).toBeNull();

    // A change after the first read is heard and read again.
    const before = s.reads.n;
    await s.base.renameMeeting(FIRST.id, "Gravel path quote, revised");
    s.send({ meetings: [FIRST.id], people: false });
    await act(async () => {});
    expect(s.reads.n).toBe(before + 1);
    expect(await within(list()).findByRole("button", { name: /^Gravel path quote, revised/ })).toBeInTheDocument();
  });

  it("still reads the list when the subscription could not be made", async () => {
    const s = open();
    await act(async () => {});
    expect(s.reads.n).toBe(0);
    await s.refuse();
    expect(s.reads.n).toBe(1);
    expect(s.reads.beforeSubscribed).toBe(0);
    expect(await within(list()).findByRole("button", { name: /^Gravel path quote/ })).toBeInTheDocument();
  });
});
