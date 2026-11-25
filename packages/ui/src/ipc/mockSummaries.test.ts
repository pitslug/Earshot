// The mock's copy of the Summaries and Dictionary rules runs the table the app's own tests run
// (`fixtures/ui/summary_settings_cases.json`, T12.20), so its refusals and messages cannot drift
// from Rust.

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/summary_settings_cases.json";
import { IpcFailure, type SummarySettingsValue } from "./ipc";
import { createMockIpc } from "./mock";
import { createSummariesMock } from "./mockSummaries";
import { peopleOf } from "./mockWords";

type Outcome<T> = { stored: T } | { error: string };

async function outcome<T>(run: () => Promise<T>): Promise<Outcome<T>> {
  try {
    return { stored: await run() };
  } catch (e) {
    if (e instanceof IpcFailure && e.error.kind === "invalid_setting") return { error: e.error.message };
    throw e;
  }
}

const base = table.base as SummarySettingsValue;

describe("the Summaries mock against the app's case table", () => {
  it("refuses and stores settings as the app does", async () => {
    expect(table.settings.length).toBeGreaterThanOrEqual(28);
    const wrong: string[] = [];
    for (const c of table.settings) {
      const mock = createSummariesMock();
      const input = { ...base, ...c.change } as SummarySettingsValue;
      const got = await outcome(async () => (await mock.setSummarySettings(input)).value);
      if (c.error !== null) {
        if (!("error" in got) || got.error !== c.error) wrong.push(`${c.name}: ${JSON.stringify(got)}`);
        if (JSON.stringify((await mock.summarySettings()).value) !== JSON.stringify(base)) {
          wrong.push(`${c.name}: a refusal stored something`);
        }
      } else {
        const want = { ...input, ...("stored" in c ? c.stored : {}) };
        if (!("stored" in got) || JSON.stringify(got.stored) !== JSON.stringify(want)) {
          wrong.push(`${c.name}: ${JSON.stringify(got)}`);
        }
      }
    }
    expect(wrong).toEqual([]);
  });

  it("counts a saved API key only for the address it was saved for, as the app does (T14.31)", async () => {
    const cases = table.key_binding.cases;
    expect(cases.length).toBeGreaterThanOrEqual(7);
    const anthropic = (url: string) => ({ ...base, provider: "Anthropic", url }) as SummarySettingsValue;
    const wrong: string[] = [];
    for (const c of cases) {
      const mock =
        c.saved_for === null
          ? createSummariesMock({ apiKeySaved: true, apiKeyFromBefore: true })
          : createSummariesMock();
      if (c.saved_for !== null) {
        await mock.setSummarySettings(anthropic(c.saved_for));
        await mock.setApiKey("sk-invented-key");
      }
      await mock.setSummarySettings(anthropic(c.now));
      const got = await mock.hasApiKey();
      if (got !== c.bound) wrong.push(`${c.name}: ${String(got)}`);
    }
    expect(wrong).toEqual([]);
  });

  it("keeps the Dictionary words as the app does", async () => {
    expect(table.vocabulary.length).toBeGreaterThanOrEqual(13);
    const wrong: string[] = [];
    for (const c of table.vocabulary) {
      const mock = createSummariesMock();
      const got = await outcome(async () => (await mock.setSummaryVocabulary(c.words)).words);
      const want = c.error !== null ? { error: c.error } : { stored: c.stored };
      if (JSON.stringify(got) !== JSON.stringify(want)) wrong.push(`${c.name}: ${JSON.stringify(got)}`);
    }
    expect(wrong).toEqual([]);
  });

  it("drops the quotes around a Dictionary word as the app does (T12.78)", async () => {
    const cases = table.dictionary_adds.cases;
    expect(cases.length).toBeGreaterThanOrEqual(12);
    const wrong: string[] = [];
    for (const c of cases) {
      // Stored as it is: the way an earlier version may have stored it.
      const mock = createSummariesMock({ vocabulary: c.before });
      const added = await outcome(() => mock.addSummaryVocabularyWord(c.add));
      if (c.error !== null) {
        if (!("error" in added) || added.error !== c.error) wrong.push(`${c.add}: ${JSON.stringify(added)}`);
      } else if (
        !("stored" in added) ||
        added.stored.added !== c.added ||
        c.word === null ||
        !added.stored.view.words.includes(c.word)
      ) {
        wrong.push(`${c.add}: ${JSON.stringify(added)}`);
      }
      if ("remove" in c && typeof c.remove === "string") await mock.removeSummaryVocabularyWord(c.remove);
      const stored = (await mock.summaryVocabulary()).words;
      if (JSON.stringify(stored) !== JSON.stringify(c.stored)) wrong.push(`${c.add}: stored ${JSON.stringify(stored)}`);
    }
    expect(wrong).toEqual([]);
  });

  it("keeps the Dictionary words as the app does, one word at a time (T11.3e)", async () => {
    // The page's own command: each case's words added in turn (blank ones are never sent) end
    // in the stored list, or the first refusal is the case's message.
    const wrong: string[] = [];
    for (const c of table.vocabulary) {
      const mock = createSummariesMock();
      let got: Outcome<string[]> | null = null;
      for (const w of c.words.filter((w) => w.trim() !== "")) {
        const added = await outcome(() => mock.addSummaryVocabularyWord(w));
        if ("error" in added) {
          got = added;
          break;
        }
      }
      got ??= { stored: (await mock.summaryVocabulary()).words };
      const want = c.error !== null ? { error: c.error } : { stored: c.stored };
      if (JSON.stringify(got) !== JSON.stringify(want)) wrong.push(`${c.name}: ${JSON.stringify(got)}`);
    }
    expect(wrong).toEqual([]);
  });

  it("keeps the summary style as the app does", async () => {
    expect(table.style.length).toBeGreaterThanOrEqual(7);
    const wrong: string[] = [];
    for (const c of table.style) {
      const mock = createSummariesMock();
      const got = await outcome(async () => (await mock.setSummaryStyle(c.text)).style);
      const want = c.error !== null ? { error: c.error } : { stored: c.stored };
      if (JSON.stringify(got) !== JSON.stringify(want)) wrong.push(`${c.name}: ${JSON.stringify(got)}`);
    }
    expect(wrong).toEqual([]);
  });

  it("offers the app's summary templates and refuses others with its words (T12.17)", async () => {
    const mock = createSummariesMock();
    expect((await mock.summaryStyle()).template).toBe("standard");
    for (const t of table.templates.options) {
      expect((await mock.setSummaryTemplate(t.value)).template).toBe(t.value);
    }
    expect(await outcome(() => mock.setSummaryTemplate("terse"))).toEqual({ error: table.templates.unknown_error });
  });

  it("cleans the saved people's names as the app does (T12.91)", async () => {
    expect(table.people.cases.length).toBeGreaterThanOrEqual(8);
    for (const c of table.people.cases) {
      const persons = c.names.map((name, i) => ({
        id: `p${String(i).padStart(3, "0")}`,
        name,
        has_voiceprint: false,
        is_me: false,
        meetings: 1,
      }));
      // Names in the table are already in the People page's order.
      const mock = createSummariesMock({ vocabulary: c.dictionary }, "surface", () => persons);
      const view = await mock.summaryVocabulary();
      // The Dictionary page lists no names (T12.97): the rule is the mock's word list's own.
      expect(peopleOf(view.words, persons), c.name).toEqual(c.people);
      expect(view, c.name).not.toHaveProperty("people");
      expect(view.people_left_out, c.name).toBe(c.left_out);
      const copy = table.messages.people_left_out;
      const note = c.left_out === 0 ? null : c.left_out === 1 ? copy.one : copy.many.replace("{n}", String(c.left_out));
      expect(view.people_left_out_note, c.name).toBe(note);
    }
  });

  it("says what the app says, with its limits", async () => {
    // The fixed messages and limits are read from the table itself; what the mock builds
    // from them is checked here.
    const m = table.messages;
    const mock = createSummariesMock();
    const view = await mock.setSummarySettings({ ...base, writer: "ClaudeCode" });
    expect(view.live_unavailable).toBe(m.live_needs_local);
    expect(view.anthropic_url).toBe(m.anthropic_url);
    const style = await mock.summaryStyle();
    expect(style.max_chars).toBe(table.limits.max_style_chars);
    const words = await mock.summaryVocabulary();
    expect([words.max_words, words.max_word_chars, words.max_chars]).toEqual([
      table.limits.max_vocabulary_words,
      table.limits.max_vocabulary_word_chars,
      table.limits.max_vocabulary_chars,
    ]);
  });

  it("says how many Dictionary words are not in use as the app does (T11.3e)", async () => {
    const copy = table.messages.not_in_use;
    expect(await createSummariesMock().summaryVocabulary()).toMatchObject({ not_in_use: 0, not_in_use_note: null });
    expect(await createSummariesMock({ notInUse: 1 }).summaryVocabulary()).toMatchObject({
      not_in_use: 1,
      not_in_use_note: copy.one,
    });
    expect(await createSummariesMock({ notInUse: 4 }).summaryVocabulary()).toMatchObject({
      not_in_use: 4,
      not_in_use_note: copy.many.replace("{n}", "4"),
    });
    // An Add is refused then, with the app's copy; a word already there is still found.
    const mock = createSummariesMock({ vocabulary: ["Quillmarsh"], notInUse: 1 });
    expect(await outcome(() => mock.addSummaryVocabularyWord("Velmora"))).toEqual({
      error: table.messages.add_not_in_use,
    });
    expect(await mock.addSummaryVocabularyWord("QUILLMARSH")).toMatchObject({ added: false });
  });

  it("refuses a token with no word as the app does (T12.15f)", async () => {
    const ipc = createMockIpc();
    await expect(ipc.addDictionaryWord("--")).rejects.toMatchObject({
      error: { kind: "invalid_setting", message: table.messages.not_a_word },
    });
  });
});
