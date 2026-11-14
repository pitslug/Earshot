// The mock's folder order follows the shared case table, which the runtime's tests run through
// `folders::list` too (T12.45). Every name is invented.

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/folder_sort_cases.json";
import { createMockIpc } from "./mock";
import baseLetters from "../../../../fixtures/ui/base_letter_cases.json";
import { BASE_OF, folderOrder } from "./mockFolders";

describe("folderOrder", () => {
  it("follows the shared case table", () => {
    expect(table.cases.length).toBeGreaterThanOrEqual(8);
    for (const c of table.cases) {
      expect([c.names, [...c.names].sort(folderOrder)]).toEqual([c.names, c.sorted]);
    }
  });

  it("folds exactly the letters core's table folds, to the same base letters", () => {
    expect(baseLetters.cases.length).toBeGreaterThan(50);
    expect(new Map(baseLetters.cases.map((c) => [c.letter, c.base]))).toEqual(BASE_OF);
  });

  it("is the order the mock lists folders in", async () => {
    const c = table.cases[1];
    const ipc = createMockIpc({ meetings: [], folders: [] });
    for (const name of c.names) await ipc.createFolder(name);
    expect((await ipc.listFolders()).map((f) => f.name)).toEqual(c.sorted);
  });
});
