// The mock's copy of the Summaries and Dictionary rules runs the table the app's own tests run
// (`fixtures/ui/summary_settings_cases.json`, T12.20), so its refusals and messages cannot drift
// from Rust.

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/summary_settings_cases.json";
import { IpcFailure, type SummarySettingsValue } from "./ipc";
import {
  CLAUDE_CODE_EFFORT_NEEDS_NEWER,
  CLAUDE_CODE_NOT_INSTALLED,
  CODEX_NOT_INSTALLED,
  MAX_STYLE_CHARS,
  MAX_VOCABULARY_CHARS,
  MAX_VOCABULARY_WORDS,
  MAX_VOCABULARY_WORD_CHARS,
  MOCK_ANTHROPIC_URL,
  createSummariesMock,
} from "./mockSummaries";

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

  it("says what the app says, with its limits", async () => {
    const m = table.messages;
    expect(CLAUDE_CODE_EFFORT_NEEDS_NEWER).toBe(m.effort_needs_newer);
    expect(MOCK_ANTHROPIC_URL).toBe(m.anthropic_url);
    expect(CLAUDE_CODE_NOT_INSTALLED.message).toBe(m.claude_code_not_installed.message);
    expect(CLAUDE_CODE_NOT_INSTALLED.fix).toBe(m.claude_code_not_installed.fix);
    expect(CODEX_NOT_INSTALLED.message).toBe(m.codex_not_installed.message);
    expect(CODEX_NOT_INSTALLED.fix).toBe(m.codex_not_installed.fix);
    const mock = createSummariesMock();
    const view = await mock.setSummarySettings({ ...base, writer: "ClaudeCode" });
    expect(view.live_unavailable).toBe(m.live_needs_local);
    expect(view.anthropic_url).toBe(m.anthropic_url);
    expect(MAX_STYLE_CHARS).toBe(table.limits.max_style_chars);
    expect(MAX_VOCABULARY_WORDS).toBe(table.limits.max_vocabulary_words);
    expect(MAX_VOCABULARY_WORD_CHARS).toBe(table.limits.max_vocabulary_word_chars);
    expect(MAX_VOCABULARY_CHARS).toBe(table.limits.max_vocabulary_chars);
  });
});
