// The UI's copy of `vocabulary::dictionary_word` follows the shared case table, which the
// runtime's tests run too (T12.15f). Every word is invented.

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/dictionary_word_cases.json";
import { dictionaryWord, inDictionary } from "./dictionaryWord";

describe("dictionaryWord", () => {
  it("follows the shared case table", () => {
    expect(table.cases.length).toBeGreaterThanOrEqual(10);
    for (const c of table.cases) {
      expect([c.token, dictionaryWord(c.token)]).toEqual([c.token, c.word]);
    }
  });

  it("finds a Dictionary entry with case ignored, and only a whole entry", () => {
    expect(inDictionary(["QXR-7", "Velmora Ridge"], "qxr-7")).toBe(true);
    expect(inDictionary(["QXR-7", "Velmora Ridge"], "Velmora")).toBe(false);
    expect(inDictionary([], "Velmora")).toBe(false);
  });
});
