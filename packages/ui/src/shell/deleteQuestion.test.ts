// T12.38: the UI's one copy of the deletion refusals (`deleteCopy.ts`, read by the delete
// question and the mock) says what `deletion.rs` says, pinned to one table
// (`fixtures/ui/delete_copy_cases.json`, also run by the desktop's Rust tests).

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/delete_copy_cases.json";
import { MOCK_DELETE_COPY } from "../ipc";
import { DELETE_COPY } from "./deleteCopy";
import { deleteQuestion } from "./deleteQuestion";

describe("delete question copy", () => {
  it("is the shared table, word for word", () => {
    const { _comment: _, ...copy } = table;
    expect(DELETE_COPY).toEqual(copy);
    expect(MOCK_DELETE_COPY).toBe(DELETE_COPY);
  });

  it("says why a meeting cannot be deleted now in the app's words", () => {
    expect(deleteQuestion("Recording")).toEqual({ text: table.recording, confirm: null });
    expect(deleteQuestion("NeedsRestart")).toEqual({ text: table.needs_restart, confirm: null });
    expect(deleteQuestion("Retranscribing")).toEqual({ text: table.retranscribing, confirm: null });
    expect(deleteQuestion("Resummarising")).toEqual({ text: table.resummarising, confirm: null });
    expect(deleteQuestion("Free").confirm).toBe("Delete meeting");
    expect(deleteQuestion("StopsFinishing").confirm).toBe("Stop and delete");
  });

  it("uses no em dash and no run of spaces", () => {
    const all = (["Free", "StopsFinishing", "Recording", "Retranscribing", "Resummarising", "NeedsRestart"] as const)
      .map((a) => deleteQuestion(a).text)
      .concat(Object.values(DELETE_COPY));
    for (const text of all) {
      expect(text).not.toMatch(/—|  /);
    }
  });
});
