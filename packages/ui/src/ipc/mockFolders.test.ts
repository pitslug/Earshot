// The mock lists folders by lower-case name, by code point (T12.54). The app's own order, with
// accented letters beside their base letters, is the runtime's (`folders::name_order`, pinned by
// `fixtures/ui/folder_sort_cases.json`); the UI shows the order it is sent. Every name is invented.

import { describe, expect, it } from "vitest";

import { createMockIpc } from "./mock";

describe("the mock's folder order", () => {
  it("is the order the mock lists folders in", async () => {
    const ipc = createMockIpc({ meetings: [], folders: [] });
    for (const name of ["Charlie", "beta", "Alpha"]) await ipc.createFolder(name);
    expect((await ipc.listFolders()).map((f) => f.name)).toEqual(["Alpha", "beta", "Charlie"]);
  });
});
