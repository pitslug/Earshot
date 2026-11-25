// The Dictionary page (T12.15a): every word is invented.

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { type Ipc, IpcFailure, IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { ADD_LABEL, DICTIONARY_INTRO, DictionaryPage, OPEN_PEOPLE_LABEL, SAVED_PEOPLE_NOTE } from "./DictionaryPage";
import { ALREADY_THERE } from "./useDictionary";
import table from "../../../../fixtures/ui/summary_settings_cases.json";

function setup(data: MockIpcData = {}, wrap: (ipc: Ipc) => Ipc = (i) => i) {
  const ipc = wrap(createMockIpc({ meetings: [], ...data }));
  const result = { ipc, user: userEvent.setup(), opened: 0 };
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <DictionaryPage onOpenPeople={() => (result.opened += 1)} />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  return result;
}

function shown(): string[] {
  const list = screen.queryByRole("list", { name: "Your words" });
  if (!list) return [];
  return within(list)
    .getAllByRole("listitem")
    .map((li) => li.firstChild?.textContent ?? "");
}

describe("Dictionary", () => {
  it("notes that saved people are used too, with a way to the People page, and lists no one (T12.97)", async () => {
    const s = setup({
      persons: [
        { id: "p1", name: "Vantrel Oskarine", is_me: false, has_voiceprint: false, meetings: 2 },
        { id: "p3", name: "Bozzle Fenwright", is_me: false, has_voiceprint: false, meetings: 1 },
      ],
    });
    await screen.findByRole("textbox", { name: ADD_LABEL });
    expect(screen.getByText(SAVED_PEOPLE_NOTE)).toBeInTheDocument();
    expect(SAVED_PEOPLE_NOTE).toMatch(/People page/);
    expect(SAVED_PEOPLE_NOTE).not.toMatch(/\u2014/);
    // The People page holds the names; this page repeats none of them.
    expect(screen.queryByRole("list", { name: "Saved people" })).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Saved people" })).not.toBeInTheDocument();
    expect(document.body.textContent).not.toMatch(/Vantrel|Bozzle/);
    expect(JSON.stringify(await s.ipc.summaryVocabulary())).not.toMatch(/Vantrel|Bozzle/);
    await s.user.click(screen.getByRole("button", { name: OPEN_PEOPLE_LABEL }));
    expect(s.opened).toBe(1);
  });

  it("notes saved people even when no one is saved yet, with no left-out note", async () => {
    setup({ persons: [] });
    await screen.findByRole("textbox", { name: ADD_LABEL });
    expect(screen.getByText(SAVED_PEOPLE_NOTE)).toBeInTheDocument();
    expect(screen.queryByText(table.messages.people_left_out.one)).not.toBeInTheDocument();
  });

  it("says how many saved names it left out, and why, without naming them (T12.91a)", async () => {
    const { ipc } = setup({
      persons: [
        { id: "p1", name: "Nim‌a Rostavel", is_me: false, has_voiceprint: false, meetings: 1 },
        { id: "p2", name: "Vantrel Oskarine", is_me: false, has_voiceprint: false, meetings: 2 },
      ],
    });
    expect(await screen.findByText(table.messages.people_left_out.one)).toBeInTheDocument();
    expect(table.messages.people_left_out.one).not.toMatch(/\u2014/);
    // No text of the left-out name reaches the page or the view it was drawn from; the view
    // carries its count (positive control: it is the one left out).
    expect(document.body.textContent).not.toMatch(/Rostavel|Nim/);
    const view = await ipc.summaryVocabulary();
    expect(view.people_left_out).toBe(1);
    expect(JSON.stringify(view)).not.toMatch(/Rostavel|Nim/);
  });

  it("counts every saved name left out (T12.91a)", async () => {
    setup({
      persons: [
        { id: "p1", name: "Nim‌a Rostavel", is_me: false, has_voiceprint: false, meetings: 1 },
        { id: "p2", name: "Kam‍Pellory Vane", is_me: false, has_voiceprint: false, meetings: 1 },
      ],
    });
    expect(
      await screen.findByText(table.messages.people_left_out.many.replace("{n}", "2")),
    ).toBeInTheDocument();
    expect(screen.getByText(SAVED_PEOPLE_NOTE)).toBeInTheDocument();
  });

  it("says how many words are not in use when the joined list passes a limit (T11.3e)", async () => {
    setup({ summaries: { vocabulary: ["Quillmarsh", "QXR-7"], notInUse: 3 } });
    const bar = await screen.findByLabelText("Words not in use");
    expect(bar).toHaveTextContent(table.messages.not_in_use.many.replace("{n}", "3"));
  });

  it("says nothing about words not in use when every word is in use", async () => {
    setup({ summaries: { vocabulary: ["Quillmarsh"] } });
    await waitFor(() => expect(shown()).toEqual(["Quillmarsh"]));
    expect(screen.queryByLabelText("Words not in use")).not.toBeInTheDocument();
  });

  it("says what the words are for and starts with none", async () => {
    setup();
    expect(screen.getByRole("heading", { level: 1, name: "Dictionary" })).toBeInTheDocument();
    expect(screen.getByText(DICTIONARY_INTRO)).toBeInTheDocument();
    expect(DICTIONARY_INTRO).toMatch(/summaries/i);
    expect(DICTIONARY_INTRO).toMatch(/names and acronyms|names, acronyms/);
    expect(DICTIONARY_INTRO).toMatch(/transcripts suggest them where they hear something similar/);
    expect(await screen.findByText("No words yet.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add" })).toBeDisabled();
  });

  it("adds a word with Enter or Add, at once, and empties the field", async () => {
    const s = setup();
    const field = await screen.findByRole("textbox", { name: ADD_LABEL });
    await s.user.type(field, "  Quillmarsh  {Enter}");
    await waitFor(() => expect(shown()).toEqual(["Quillmarsh"]));
    expect(field).toHaveValue("");
    expect((await s.ipc.summaryVocabulary()).words).toEqual(["Quillmarsh"]);
    await s.user.type(field, "QXR-7");
    await s.user.click(screen.getByRole("button", { name: "Add" }));
    await waitFor(() => expect(shown()).toEqual(["Quillmarsh", "QXR-7"]));
    expect((await s.ipc.summaryVocabulary()).words).toEqual(["Quillmarsh", "QXR-7"]);
    expect(screen.getByText(/2 of 100 words, each up to 40 characters/)).toBeInTheDocument();
  });

  it("shows the words in alphabetical order, any case, with Add above them", async () => {
    setup({ summaries: { vocabulary: ["zephyrine", "Brackwell", "apsley", "QXR-7", "Ostrelby Ridge"] } });
    await waitFor(() =>
      expect(shown()).toEqual(["apsley", "Brackwell", "Ostrelby Ridge", "QXR-7", "zephyrine"]),
    );
    const field = screen.getByRole("textbox", { name: ADD_LABEL });
    const list = screen.getByRole("list", { name: "Your words" });
    // The Add field comes before the list, so a long list never pushes it off the page.
    expect(field.compareDocumentPosition(list) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("deletes one word at once and keeps the others in order", async () => {
    const s = setup({ summaries: { vocabulary: ["Quillmarsh", "QXR-7", "Ostrelby Ridge"] } });
    await waitFor(() => expect(shown()).toEqual(["Ostrelby Ridge", "Quillmarsh", "QXR-7"]));
    await s.user.click(screen.getByRole("button", { name: "Delete QXR-7" }));
    await waitFor(() => expect(shown()).toEqual(["Ostrelby Ridge", "Quillmarsh"]));
    expect((await s.ipc.summaryVocabulary()).words).toEqual(["Quillmarsh", "Ostrelby Ridge"]);
    await s.user.click(screen.getByRole("button", { name: "Delete Quillmarsh" }));
    await s.user.click(await screen.findByRole("button", { name: "Delete Ostrelby Ridge" }));
    expect(await screen.findByText("No words yet.")).toBeInTheDocument();
    expect((await s.ipc.summaryVocabulary()).words).toEqual([]);
  });

  it("says when a word is already there in another case and adds nothing", async () => {
    const s = setup({ summaries: { vocabulary: ["Quillmarsh"] } });
    const field = await screen.findByRole("textbox", { name: ADD_LABEL });
    await s.user.type(field, "QUILLMARSH{Enter}");
    expect(await screen.findByText(ALREADY_THERE)).toBeInTheDocument();
    expect(shown()).toEqual(["Quillmarsh"]);
    // Typing again clears the message.
    await s.user.type(field, "Q");
    expect(screen.queryByText(ALREADY_THERE)).toBeNull();
  });

  it("shows the app's refusal, keeps the typed word and stores nothing", async () => {
    const s = setup({ summaries: { vocabulary: ["Quillmarsh"] } });
    const field = await screen.findByRole("textbox", { name: ADD_LABEL });
    const long = "Q".repeat(41);
    await s.user.type(field, `${long}{Enter}`);
    expect(await screen.findByText("A word can have at most 40 characters; this one has 41.")).toBeInTheDocument();
    expect(field).toHaveValue(long);
    expect((await s.ipc.summaryVocabulary()).words).toEqual(["Quillmarsh"]);
  });

  it("mock follows the app's rules: trimmed, inner spaces joined, repeats found, limits", async () => {
    const ipc = createMockIpc({ meetings: [] });
    expect(await ipc.addSummaryVocabularyWord(" Ostrelby   Ridge ")).toMatchObject({ added: true });
    expect(await ipc.addSummaryVocabularyWord("ostrelby ridge")).toMatchObject({ added: false });
    expect((await ipc.addSummaryVocabularyWord("QXR-7")).view.words).toEqual(["Ostrelby Ridge", "QXR-7"]);
    await expect(ipc.addSummaryVocabularyWord("a\tb")).rejects.toThrow(IpcFailure);
    expect((await ipc.removeSummaryVocabularyWord("OSTRELBY  ridge")).words).toEqual(["QXR-7"]);
    const full = createMockIpc({
      meetings: [],
      summaries: { vocabulary: Array.from({ length: 100 }, (_, i) => `w${i}`) },
    });
    await expect(full.addSummaryVocabularyWord("Quillmarsh")).rejects.toThrow("at most 100 words; this one has 101");
  });

  it("refuses an Add while words are not in use, and says so (T11.3e)", async () => {
    const s = setup({ summaries: { vocabulary: ["Quillmarsh"], notInUse: 2 } });
    const field = await screen.findByRole("textbox", { name: ADD_LABEL });
    await s.user.type(field, "Velmora{Enter}");
    expect(await screen.findByText(table.messages.add_not_in_use)).toBeInTheDocument();
    expect(field).toHaveValue("Velmora");
    expect((await s.ipc.summaryVocabulary()).words).toEqual(["Quillmarsh"]);
    // A word already there is still found, not refused, and the page says it was there.
    await s.user.clear(field);
    await s.user.type(field, "quillmarsh{Enter}");
    expect(await screen.findByText(ALREADY_THERE)).toBeInTheDocument();
  });

  it("edits one word at a time and never sends a whole list (T11.3e)", async () => {
    const calls: string[] = [];
    const s = setup({ summaries: { vocabulary: ["Quillmarsh", "QXR-7"] } }, (ipc) => ({
      ...ipc,
      addSummaryVocabularyWord: async (text) => {
        calls.push(`add:${text}`);
        return ipc.addSummaryVocabularyWord(text);
      },
      removeSummaryVocabularyWord: async (word) => {
        calls.push(`remove:${word}`);
        return ipc.removeSummaryVocabularyWord(word);
      },
    }));
    const field = await screen.findByRole("textbox", { name: ADD_LABEL });
    await s.user.type(field, "Velmora{Enter}");
    await waitFor(() => expect(shown()).toEqual(["Quillmarsh", "QXR-7", "Velmora"]));
    await s.user.click(screen.getByRole("button", { name: "Delete QXR-7" }));
    await waitFor(() => expect(shown()).toEqual(["Quillmarsh", "Velmora"]));
    expect(calls).toEqual(["add:Velmora", "remove:QXR-7"]);
  });

  it("names a stored list this version refuses, by its reason", async () => {
    setup({}, (ipc) => ({
      ...ipc,
      summaryVocabulary: async () => ({
        words: [],
        unknown: ["settings/vocabulary: a word list this version refuses: it is not a list of words"],
        max_words: 100,
        max_word_chars: 40,
        max_chars: 1000,
        not_in_use: 0,
        not_in_use_note: null,
        people_left_out: 0,
        people_left_out_note: null,
      }),
    }));
    expect(await screen.findByText("The stored dictionary is not used")).toBeInTheDocument();
    expect(screen.getByText(/it is not a list of words/)).toBeInTheDocument();
  });

  it("says when the words cannot be read", async () => {
    setup({}, (ipc) => ({
      ...ipc,
      summaryVocabulary: async () => {
        throw new IpcFailure({ kind: "store", message: "The database is locked.", retryable: true });
      },
    }));
    expect(await screen.findByText("Your dictionary could not be read")).toBeInTheDocument();
  });
});
