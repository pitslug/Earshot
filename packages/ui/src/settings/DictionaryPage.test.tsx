// The Dictionary page (T12.15a): every word is invented.

import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { type Ipc, IpcFailure, IpcProvider, type MockIpcData, createMockIpc } from "../ipc";
import { installMatchMedia } from "../test/matchMedia";
import { EarshotThemeProvider } from "../theme/EarshotThemeProvider";
import { ADD_LABEL, DICTIONARY_INTRO, DictionaryPage } from "./DictionaryPage";
import { ALREADY_THERE } from "./useDictionary";

function setup(data: MockIpcData = {}, wrap: (ipc: Ipc) => Ipc = (i) => i) {
  const ipc = wrap(createMockIpc({ meetings: [], ...data }));
  render(
    <EarshotThemeProvider>
      <IpcProvider ipc={ipc}>
        <DictionaryPage />
      </IpcProvider>
    </EarshotThemeProvider>,
  );
  return { ipc, user: userEvent.setup() };
}

function shown(): string[] {
  const list = screen.queryByRole("list", { name: "Your words" });
  if (!list) return [];
  return within(list)
    .getAllByRole("listitem")
    .map((li) => li.firstChild?.textContent ?? "");
}

beforeEach(() => {
  installMatchMedia(false);
});

describe("Dictionary", () => {
  it("says what the words are for and starts with none", async () => {
    setup();
    expect(screen.getByRole("heading", { level: 1, name: "Dictionary" })).toBeInTheDocument();
    expect(screen.getByText(DICTIONARY_INTRO)).toBeInTheDocument();
    expect(DICTIONARY_INTRO).toMatch(/summaries/i);
    expect(DICTIONARY_INTRO).toMatch(/names and acronyms|names, acronyms/);
    expect(DICTIONARY_INTRO).toMatch(/transcripts will use them later/);
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

  it("mock follows the app's rules: trimmed, inner spaces joined, repeats dropped, limits", async () => {
    const ipc = createMockIpc({ meetings: [] });
    expect((await ipc.setSummaryVocabulary([" Ostrelby   Ridge ", "", "ostrelby ridge", "QXR-7"])).words).toEqual([
      "Ostrelby Ridge",
      "QXR-7",
    ]);
    await expect(ipc.setSummaryVocabulary(["a\tb"])).rejects.toThrow(IpcFailure);
    const many = Array.from({ length: 101 }, (_, i) => `w${i}`);
    await expect(ipc.setSummaryVocabulary(many)).rejects.toThrow("at most 100 words; this one has 101");
    expect((await ipc.summaryVocabulary()).words).toEqual(["Ostrelby Ridge", "QXR-7"]);
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
