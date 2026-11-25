// The mock's copy of `export_settings::check_folder` follows the shared case table, which the
// runtime's tests run too (T14.29). Every name is invented.

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/vault_folder_cases.json";
import { IpcFailure } from "./ipc";
import { checkFolder } from "./mockSettings";

/** What the desktop sends for a refusal: the runtime's text as a sentence (`obsidian::sentence`). */
function sentence(text: string): string {
  const s = text.charAt(0).toUpperCase() + text.slice(1);
  return s.endsWith(".") ? s : `${s}.`;
}

type Case = { folder: string; stored?: string; refused?: string };

describe("checkFolder", () => {
  it("follows the shared case table", () => {
    const cases = table.cases as Case[];
    expect(cases.length).toBeGreaterThanOrEqual(30);
    for (const c of cases) {
      if (c.stored !== undefined) {
        expect([c.folder, checkFolder(c.folder)]).toEqual([c.folder, c.stored]);
        continue;
      }
      let error: unknown = null;
      try {
        checkFolder(c.folder);
      } catch (e) {
        error = e;
      }
      expect(error, c.folder).toBeInstanceOf(IpcFailure);
      const failure = error as IpcFailure;
      expect([c.folder, failure.error.kind, failure.error.message]).toEqual([
        c.folder,
        "invalid_setting",
        sentence(c.refused ?? ""),
      ]);
    }
  });
});
