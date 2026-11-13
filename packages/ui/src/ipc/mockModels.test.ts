// The mock's refusals on the model pages say what the app says (T12.30): their copy is checked
// against `fixtures/ui/model_copy_cases.json`, which the desktop's own tests check too.

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/model_copy_cases.json";
import { createManualTimer } from "../time";
import { IpcFailure, type ModelSettingsValue } from "./ipc";
import { MOCK_DISPLAY_NAMES, MOCK_FALLBACK_MODEL, createModelsMock } from "./mockModels";

const SETTINGS: ModelSettingsValue = {
  transcription: null,
  transcription_part: "Automatic",
  summary: null,
  summary_part: "Automatic",
  languages: ["en"],
};

async function refusal(run: () => Promise<unknown>): Promise<string> {
  try {
    await run();
  } catch (e) {
    if (e instanceof IpcFailure) return e.error.message;
    throw e;
  }
  throw new Error("not refused");
}

describe("the models mock against the app's copy table", () => {
  it("refuses settings and removals with the app's words", async () => {
    const mock = createModelsMock("desktop", createManualTimer(0));
    const set = (change: Partial<ModelSettingsValue>) => () => mock.setModelSettings({ ...SETTINGS, ...change });
    const f = table.fixed;
    // Every `fixed` entry is shown by the mock and checked below; Rust-only copy lives apart.
    expect(Object.keys(f).sort()).toEqual([
      "no_languages",
      "npu_for_summaries",
      "unknown_language",
      "unknown_model",
      "unknown_summary_model",
      "unknown_transcription_model",
    ]);
    expect(Object.keys(table.rust_only)).toEqual(["model_files_failed"]);
    expect(await refusal(set({ languages: [] }))).toBe(f.no_languages);
    expect(await refusal(set({ languages: ["xx"] }))).toBe(f.unknown_language);
    expect(await refusal(set({ transcription: "no-such-model" }))).toBe(f.unknown_transcription_model);
    expect(await refusal(set({ summary: "no-such-model" }))).toBe(f.unknown_summary_model);
    expect(await refusal(set({ summary_part: "Npu" }))).toBe(f.npu_for_summaries);
    expect(await refusal(set({ transcription: "parakeet-tdt-0.6b-v2", languages: ["en", "de", "fr"] }))).toBe(
      table.templates.model_languages
        .replace("{model}", MOCK_DISPLAY_NAMES["parakeet-tdt-0.6b-v2"])
        .replace("{missing}", "de, fr"),
    );
    expect(await refusal(() => mock.removeModel("no-such-model"))).toBe(f.unknown_model);
    expect(await refusal(() => mock.removeModel(MOCK_FALLBACK_MODEL))).toBe(
      table.templates.chosen.replace("{model}", MOCK_DISPLAY_NAMES[MOCK_FALLBACK_MODEL]),
    );
  });
});
