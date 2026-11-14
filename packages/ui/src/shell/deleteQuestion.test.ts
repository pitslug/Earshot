// T12.38: the delete question and the mock say what `deletion.rs` says, from one table
// (`fixtures/ui/delete_copy_cases.json`, also run by the desktop's Rust tests).

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/delete_copy_cases.json";
import { MOCK_DELETE_COPY } from "../ipc";
import { deleteQuestion } from "./deleteQuestion";

describe("delete question copy", () => {
  it("says why a meeting cannot be deleted now in the app's words", () => {
    expect(deleteQuestion("Recording")).toEqual({ text: table.recording, confirm: null });
    expect(deleteQuestion("NeedsRestart")).toEqual({ text: table.needs_restart, confirm: null });
    expect(deleteQuestion("Retranscribing")).toEqual({ text: table.retranscribing, confirm: null });
    expect(deleteQuestion("Resummarising")).toEqual({ text: table.resummarising, confirm: null });
    expect(deleteQuestion("Free").confirm).toBe("Delete meeting");
    expect(deleteQuestion("StopsFinishing").confirm).toBe("Stop and delete");
  });

  it("keeps the mock's refusals in step", () => {
    expect(MOCK_DELETE_COPY).toEqual({
      recording: table.recording,
      finishing: table.finishing,
      retranscribing: table.retranscribing,
      resummarising: table.resummarising,
    });
  });

  it("uses no em dash and no run of spaces", () => {
    const all = (["Free", "StopsFinishing", "Recording", "Retranscribing", "Resummarising", "NeedsRestart"] as const)
      .map((a) => deleteQuestion(a).text)
      .concat(Object.values(table).filter((v) => typeof v === "string"));
    for (const text of all) {
      expect(text).not.toMatch(/—|  /);
    }
  });
});
