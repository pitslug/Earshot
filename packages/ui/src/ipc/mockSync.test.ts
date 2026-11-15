// The Sync mock against the shared table (T11.3a): every `address_cases` row gives its copy
// through the set up call, as the desktop's own tests run the same rows through the app's rule;
// refused commands use `command_problems`, the status line `problems`, and both maps hold the
// same words.

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/sync_copy_cases.json";
import { IpcFailure } from "./ipc";
import {
  ADDRESS_TOO_LONG,
  MOCK_SYNC_CODE,
  MOCK_SYNC_SECRET,
  type SyncCommandName,
  canonicalCode,
  createSyncMock,
} from "./mockSync";

async function refusal(work: Promise<unknown>): Promise<IpcFailure | null> {
  try {
    await work;
    return null;
  } catch (e) {
    if (e instanceof IpcFailure) return e;
    throw e;
  }
}

describe("the Sync mock", () => {
  it("gives every address case its copy", async () => {
    expect(table.address_cases.length).toBeGreaterThanOrEqual(10);
    for (const c of table.address_cases) {
      const e = await refusal(createSyncMock().setUpSync(c.input, MOCK_SYNC_SECRET));
      if (c.copy === null) {
        expect(e, c.input).toBeNull();
      } else {
        expect(e?.error.kind, c.input).toBe("invalid_setting");
        expect(e?.error.message, c.input).toBe(c.copy);
      }
    }
  });

  it("refuses an address over 2048 characters", async () => {
    const at = `https://${"a".repeat(2_040)}.example.com`;
    const e = await refusal(createSyncMock().setUpSync(at, MOCK_SYNC_SECRET));
    expect(e?.error).toMatchObject({ kind: "invalid_setting", message: ADDRESS_TOO_LONG });
  });

  it("has the same words in the status and command copy, the stopping ones among them", () => {
    const status = Object.keys(table.problems).sort();
    expect(Object.keys(table.command_problems).sort()).toEqual(status);
    for (const word of table.stops) expect(status, word).toContain(word);
  });

  it("refuses a missing or wrong secret and pairing codes with the command copy", async () => {
    const sync = createSyncMock();
    const missing = await refusal(sync.setUpSync("https://sync.example.com", "  "));
    expect(missing?.error).toMatchObject({ kind: "invalid_setting", message: table.secret_missing });
    const wrong = await refusal(sync.setUpSync("https://sync.example.com", "not it"));
    expect(wrong?.error).toMatchObject({ kind: "sync", message: table.command_problems.bad_secret });
    for (const bad of ["", "AAAAA-BBBBB", "UUUUU-UUUUU-UUUUU-UUUUU-UUUUUU"]) {
      const e = await refusal(sync.joinSync("https://sync.example.com", bad));
      expect(e?.error, bad).toMatchObject({ kind: "sync", message: table.command_problems.bad_code });
    }
    const other = await refusal(sync.joinSync("https://sync.example.com", "00000-11111-22222-33333-444444"));
    expect(other?.error).toMatchObject({ kind: "sync", message: table.command_problems.no_slot });
    expect((await sync.syncView()).set_up).toBe(false);
    // Lower case, spaces and the look-alike letters read as the offered code.
    const typed = MOCK_SYNC_CODE.toLowerCase().replace(/-/g, " ").replace("0", "o").replace("1", "l");
    expect(canonicalCode(typed)).toBe(canonicalCode(MOCK_SYNC_CODE));
    const joined = await sync.joinSync("https://sync.example.com", typed);
    expect(joined).toMatchObject({ set_up: true, on: true, address: "https://sync.example.com" });
  });

  it("refuses every command with its command copy", async () => {
    const commands: SyncCommandName[] = [
      "syncDevices",
      "offerSyncCode",
      "setSyncOn",
      "setSyncVoiceprints",
      "setSyncDictionary",
      "syncNow",
      "removeSyncDevice",
    ];
    for (const name of commands) {
      const sync = createSyncMock({ setUp: true, refuse: { [name]: "unreachable" } });
      const call = sync[name] as (...args: unknown[]) => Promise<unknown>;
      const e = await refusal(call(true));
      expect(e?.error, name).toMatchObject({ kind: "sync", message: table.command_problems.unreachable });
    }
  });

  it("names its problems with the status copy", async () => {
    const view = await createSyncMock({ setUp: true, problem: "key_mismatch" }).syncView();
    expect(view.problem).toBe(table.problems.key_mismatch);
    expect(view.stopped).toBe(true);
  });

  it("starts a stopped sync on sync now", async () => {
    const sync = createSyncMock({ setUp: true, running: false });
    expect((await sync.syncView()).running).toBe(false);
    expect((await sync.syncNow()).running).toBe(true);
  });
});
