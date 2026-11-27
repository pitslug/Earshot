// Damaged items in the window (T14.65, docs/sync-spec.md 17.9 test 14): the banner, Dismiss,
// Details in both count forms, the sync-off, kind-off and nothing-healable sentences, a Repair's
// reading, stop and both results. Every expected word comes from the copy table.

import { act, cleanup, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { createMockIpc } from "../ipc";
import type { DamageCountsView } from "../ipc/bindings";
import { DAMAGED_COPY, DAMAGE_KINDS, NO_DAMAGE, readingText } from "../ipc/damagedCopy";
import { type DamageMockData, MOCK_REPAIR_STEP_MS } from "../ipc/mockDamage";
import { listLoaded, openSettings, renderApp } from "../test/app";
import { createManualTimer } from "../time";

const NOW = new Date(2026, 10, 2, 16, 0).getTime();

function setup(damaged?: DamageMockData) {
  const timer = createManualTimer(NOW);
  const calls: string[] = [];
  const base = createMockIpc({ timer, now: NOW, damaged });
  const ipc = {
    ...base,
    dismissDamageBanner: async () => {
      calls.push("dismissDamageBanner");
      return base.dismissDamageBanner();
    },
    repairFromSync: async () => {
      calls.push("repairFromSync");
      return base.repairFromSync();
    },
  };
  const user = userEvent.setup();
  renderApp(ipc, { timer, now: () => NOW });
  return { user, timer, calls, ipc };
}

/** Lets every pending promise settle once (the banner's read answers in one flush). */
const flush = () => act(async () => {});

const banner = () => screen.findByText(DAMAGED_COPY.banner);

async function openDetails(user: ReturnType<typeof userEvent.setup>): Promise<HTMLElement> {
  await banner();
  await user.click(screen.getByRole("button", { name: DAMAGED_COPY.details_button }));
  const dialog = await screen.findByRole("dialog", { name: DAMAGED_COPY.details_title });
  // The scan's answer is in once the spinner has gone.
  await waitFor(() => expect(within(dialog).queryByRole("progressbar")).toBeNull());
  return dialog;
}

const every = (n: number): DamageCountsView => ({
  meetings: n,
  parts: n,
  people: n,
  voice: n,
  folders: n,
  words: n,
});

describe("the damaged-items banner", () => {
  it("shows while something is damaged, on every view, and Dismiss hides it", async () => {
    const { user, calls } = setup({ counts: { parts: 2 } });
    await listLoaded();
    expect(await banner()).toBeInTheDocument();
    // On the Settings view too.
    await openSettings(user, "General");
    expect(await banner()).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: DAMAGED_COPY.dismiss }));
    await waitFor(() => expect(screen.queryByText(DAMAGED_COPY.banner)).toBeNull());
    expect(calls).toEqual(["dismissDamageBanner"]);
  });

  it("stays away when dismissed at the same counts", async () => {
    setup({ counts: { parts: 2 }, dismissedAt: { parts: 2 } });
    await listLoaded();
    await flush();
    expect(screen.queryByText(DAMAGED_COPY.banner)).toBeNull();
  });

  it("control: dismissed at fewer in one kind, it shows", async () => {
    setup({ counts: { parts: 2, words: 1 }, dismissedAt: { parts: 2 } });
    await listLoaded();
    expect(await banner()).toBeInTheDocument();
  });

  it("control: nothing damaged, nothing renders", async () => {
    setup();
    await listLoaded();
    await flush();
    expect(screen.queryByText(DAMAGED_COPY.banner)).toBeNull();
    expect(screen.queryByRole("button", { name: DAMAGED_COPY.details_button })).toBeNull();
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});

describe("Details", () => {
  for (const [form, n] of [
    ["one", 1],
    ["many", 3],
  ] as const) {
    it(`lists every damaged kind in its ${form} form, in order`, async () => {
      const { user } = setup({ counts: every(n), syncOn: true, switchesOff: { voice: false, words: false } });
      const dialog = await openDetails(user);
      const items = within(within(dialog).getByRole("list", { name: DAMAGED_COPY.details_title })).getAllByRole(
        "listitem",
      );
      const forms = DAMAGE_KINDS.map((kind) => DAMAGED_COPY.kinds[kind][form].replace("{count}", String(n)));
      expect(items.map((li) => li.textContent)).toEqual(forms);
    });
  }

  it("lists only the kinds with something damaged", async () => {
    const { user } = setup({ counts: { folders: 2 }, syncOn: true });
    const dialog = await openDetails(user);
    const items = within(dialog).getAllByRole("listitem");
    expect(items.map((li) => li.textContent)).toEqual([DAMAGED_COPY.kinds.folders.many.replace("{count}", "2")]);
  });

  it("with sync off says this computer can't repair them, and offers nothing to press", async () => {
    const { user } = setup({ counts: { parts: 1 }, syncOn: false });
    const dialog = await openDetails(user);
    expect(within(dialog).getByText(DAMAGED_COPY.no_repair_sync_off)).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: DAMAGED_COPY.repair_button })).toBeNull();
    expect(within(dialog).queryByText(DAMAGED_COPY.repair_intro)).toBeNull();
  });

  it("control: with sync on offers Repair, which starts reading", async () => {
    const { user, calls } = setup({ counts: { parts: 1 }, syncOn: true });
    const dialog = await openDetails(user);
    expect(within(dialog).queryByText(DAMAGED_COPY.no_repair_sync_off)).toBeNull();
    expect(within(dialog).getByText(DAMAGED_COPY.repair_intro)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: DAMAGED_COPY.repair_button }));
    expect(await within(dialog).findByText(DAMAGED_COPY.repair_reading_start)).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: DAMAGED_COPY.repair_button })).toBeNull();
    expect(calls).toEqual(["repairFromSync"]);
  });

  it("names each damaged kind whose own sync switch is off", async () => {
    const { user } = setup({ counts: { voice: 1, words: 2 }, syncOn: true, switchesOff: { voice: true, words: true } });
    const dialog = await openDetails(user);
    expect(within(dialog).getByText(DAMAGED_COPY.kind_off_voice)).toBeInTheDocument();
    expect(within(dialog).getByText(DAMAGED_COPY.kind_off_words)).toBeInTheDocument();
  });

  it("control: with both switches on, no kind-off sentence", async () => {
    const { user } = setup({ counts: { voice: 1, words: 2 }, syncOn: true, switchesOff: { voice: false, words: false } });
    const dialog = await openDetails(user);
    expect(within(dialog).getByText(DAMAGED_COPY.kinds.voice.one)).toBeInTheDocument();
    expect(within(dialog).queryByText(DAMAGED_COPY.kind_off_voice)).toBeNull();
    expect(within(dialog).queryByText(DAMAGED_COPY.kind_off_words)).toBeNull();
  });
});

describe("a Repair in Details", () => {
  it("reads with no percentage until one is known, then with it", async () => {
    for (const percent of [null, 42]) {
      const { user, ipc } = setup({ counts: { parts: 1 }, syncOn: true, repair: { state: "reading", percent } });
      const dialog = await openDetails(user);
      expect(within(dialog).getByText(readingText(percent))).toBeInTheDocument();
      if (percent === null) {
        expect(within(dialog).getByText(DAMAGED_COPY.repair_reading_start)).toBeInTheDocument();
      } else {
        expect(within(dialog).getByText(DAMAGED_COPY.repair_reading.replace("{percent}", "42"))).toBeInTheDocument();
        expect(within(dialog).queryByText(DAMAGED_COPY.repair_reading_start)).toBeNull();
      }
      expect(within(dialog).queryByText(DAMAGED_COPY.repair_stopped)).toBeNull();
      await expect(ipc.repairFromSync()).rejects.toMatchObject({ error: { kind: "busy", message: DAMAGED_COPY.busy } });
      await user.click(within(dialog).getByRole("button", { name: DAMAGED_COPY.close }));
      await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
      cleanup();
    }
  });

  it("says it is waiting while sync has stopped", async () => {
    const { user } = setup({ counts: { parts: 1 }, syncOn: true, repair: { state: "stopped" } });
    const dialog = await openDetails(user);
    expect(within(dialog).getByText(DAMAGED_COPY.repair_stopped)).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: DAMAGED_COPY.repair_button })).toBeNull();
  });

  it("control: with sync running and nothing started, no stopped or reading line", async () => {
    const { user } = setup({ counts: { parts: 1 }, syncOn: true });
    const dialog = await openDetails(user);
    expect(within(dialog).queryByText(DAMAGED_COPY.repair_stopped)).toBeNull();
    expect(within(dialog).queryByText(DAMAGED_COPY.repair_reading_start)).toBeNull();
  });

  it("follows the Repair to the end: progress, then nothing left", async () => {
    const { user, timer } = setup({ counts: { parts: 2, people: 1 }, syncOn: true });
    const dialog = await openDetails(user);
    await user.click(within(dialog).getByRole("button", { name: DAMAGED_COPY.repair_button }));
    expect(await within(dialog).findByText(DAMAGED_COPY.repair_reading_start)).toBeInTheDocument();
    act(() => timer.advance(MOCK_REPAIR_STEP_MS));
    expect(await within(dialog).findByText(readingText(50))).toBeInTheDocument();
    act(() => timer.advance(MOCK_REPAIR_STEP_MS));
    expect(await within(dialog).findByText(DAMAGED_COPY.result_none)).toBeInTheDocument();
    expect(within(dialog).queryByRole("listitem")).toBeNull();
    // The banner goes with the damage.
    await waitFor(() => expect(screen.queryByText(DAMAGED_COPY.banner)).toBeNull());
  });

  it("shows what is left, the kind-off sentence and nothing-healable for the rest", async () => {
    const left = { ...NO_DAMAGE, parts: 2, voice: 1 };
    const { user } = setup({
      counts: left,
      syncOn: true,
      switchesOff: { voice: true, words: false },
      repair: { state: "done", left },
    });
    const dialog = await openDetails(user);
    expect(within(dialog).getByText(DAMAGED_COPY.result_some)).toBeInTheDocument();
    expect(within(dialog).getAllByRole("listitem").map((li) => li.textContent)).toEqual([
      DAMAGED_COPY.kinds.parts.many.replace("{count}", "2"),
      DAMAGED_COPY.kinds.voice.one,
    ]);
    expect(within(dialog).getByText(DAMAGED_COPY.kind_off_voice)).toBeInTheDocument();
    expect(within(dialog).getByText(DAMAGED_COPY.nothing_healable)).toBeInTheDocument();
    expect(within(dialog).queryByText(DAMAGED_COPY.result_none)).toBeNull();
  });

  it("control: what is left only in a switched-off kind says nothing-healable not", async () => {
    const left = { ...NO_DAMAGE, voice: 1 };
    const { user } = setup({
      counts: left,
      syncOn: true,
      switchesOff: { voice: true },
      repair: { state: "done", left },
    });
    const dialog = await openDetails(user);
    expect(within(dialog).getByText(DAMAGED_COPY.result_some)).toBeInTheDocument();
    expect(within(dialog).getByText(DAMAGED_COPY.kind_off_voice)).toBeInTheDocument();
    expect(within(dialog).queryByText(DAMAGED_COPY.nothing_healable)).toBeNull();
  });
});

describe("Details after the T14.65 review", () => {
  it("says nothing is damaged when its scan finds nothing and no Repair runs or ended", async () => {
    const { user } = setup({ counts: { parts: 1 }, syncOn: true, dismissedAt: { parts: 0 } });
    const dialog = await openDetails(user);
    expect(within(dialog).queryByText(DAMAGED_COPY.nothing_damaged)).toBeNull();
    cleanup();
    // The banner is from the record; Details' own scan finds nothing.
    const base = createMockIpc({ now: NOW, damaged: { counts: { parts: 1 }, syncOn: true } });
    const ipc = {
      ...base,
      damagedItems: async () => ({ ...(await base.damagedItems()), counts: { ...NO_DAMAGE } }),
    };
    const u = userEvent.setup();
    renderApp(ipc, { now: () => NOW });
    const d = await openDetails(u);
    expect(within(d).getByText(DAMAGED_COPY.nothing_damaged)).toBeInTheDocument();
    expect(within(d).queryByRole("button", { name: DAMAGED_COPY.repair_button })).toBeNull();
    expect(within(d).queryByText(DAMAGED_COPY.result_none)).toBeNull();
  });

  it("never shows nothing-left against its own fresh scan: the counts, with Repair offered", async () => {
    const { user } = setup({
      counts: { meetings: 1 },
      syncOn: true,
      repair: { state: "done", left: { ...NO_DAMAGE } },
    });
    const dialog = await openDetails(user);
    expect(within(dialog).queryByText(DAMAGED_COPY.result_none)).toBeNull();
    expect(within(dialog).getByText(DAMAGED_COPY.result_some)).toBeInTheDocument();
    expect(within(dialog).getByText(DAMAGED_COPY.kinds.meetings.one)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: DAMAGED_COPY.repair_button })).toBeInTheDocument();
  });

  it("closes with focus on the window when the banner went while it was open", async () => {
    const { user, timer } = setup({ counts: { parts: 1 }, syncOn: true });
    await banner();
    const details = screen.getByRole("button", { name: DAMAGED_COPY.details_button });
    // The Details button is the dialog's restore target (jsdom cannot prove the hand-back).
    expect(details.getAttribute("data-tabster")).toContain("restorer");
    const dialog = await openDetails(user);
    await user.click(within(dialog).getByRole("button", { name: DAMAGED_COPY.repair_button }));
    act(() => timer.advance(MOCK_REPAIR_STEP_MS));
    act(() => timer.advance(MOCK_REPAIR_STEP_MS));
    expect(await within(dialog).findByText(DAMAGED_COPY.result_none)).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByText(DAMAGED_COPY.banner)).toBeNull());
    await user.click(within(dialog).getByRole("button", { name: DAMAGED_COPY.close }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(document.activeElement).not.toBe(document.body));
    expect(document.activeElement).not.toBeNull();
  });
});

