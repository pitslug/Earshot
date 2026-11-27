// The mock's damaged-items rules follow the app's (`damage.rs`, docs/sync-spec.md 17): the banner
// while anything is found and not dismissed, Repair refused with sync off or while one runs.

import { describe, expect, it } from "vitest";

import { createManualTimer } from "../time";
import type { DamageEvent } from "./bindings";
import { DAMAGED_COPY, kindLine, kindLines, readingText } from "./damagedCopy";
import { createMockIpc } from "./mock";
import { MOCK_REPAIR_STEP_MS } from "./mockDamage";

describe("the damaged-items mock", () => {
  it("shows the banner until dismissed, and again only when a kind holds more", async () => {
    const ipc = createMockIpc({ damaged: { counts: { people: 1 } } });
    expect(await ipc.damageBanner()).toBe(true);
    const heard: DamageEvent[] = [];
    await ipc.onDamageEvent((e) => heard.push(e));
    await ipc.dismissDamageBanner();
    expect(await ipc.damageBanner()).toBe(false);
    expect(heard.at(-1)?.shown).toBe(false);
    // Control: nothing damaged, no banner.
    expect(await createMockIpc().damageBanner()).toBe(false);
  });

  it("refuses Repair with sync off, and while one runs", async () => {
    await expect(createMockIpc({ damaged: { counts: { parts: 1 }, syncOn: false } }).repairFromSync()).rejects.toMatchObject({
      error: { kind: "invalid_setting", message: DAMAGED_COPY.no_repair_sync_off },
    });
    const timer = createManualTimer(0);
    const ipc = createMockIpc({ timer, damaged: { counts: { parts: 1, voice: 2 }, syncOn: true, switchesOff: { voice: true } } });
    await ipc.repairFromSync();
    await expect(ipc.repairFromSync()).rejects.toMatchObject({ error: { kind: "busy", message: DAMAGED_COPY.busy } });
    timer.advance(2 * MOCK_REPAIR_STEP_MS);
    // A Repair heals what the server holds; the switched-off voice data stays.
    const after = await ipc.damagedItems();
    expect(after.repair).toEqual({ state: "done", left: { meetings: 0, parts: 0, people: 0, voice: 2, folders: 0, words: 0 } });
    expect(after.kinds_off).toEqual({ voice: true, words: false });
    expect(await ipc.damageBanner()).toBe(true);
  });

  it("fills the copy's one and many forms and the percentage", () => {
    expect(kindLine("people", 1)).toBe(DAMAGED_COPY.kinds.people.one);
    expect(kindLine("people", 12)).toBe(DAMAGED_COPY.kinds.people.many.replace("{count}", "12"));
    expect(kindLines({ meetings: 0, parts: 0, people: 0, voice: 0, folders: 0, words: 0 })).toEqual([]);
    expect(readingText(null)).toBe(DAMAGED_COPY.repair_reading_start);
    expect(readingText(7)).toBe(DAMAGED_COPY.repair_reading.replace("{percent}", "7"));
  });
});
