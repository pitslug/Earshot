// The Dictionary page's state (T12.15a): the stored words, the word being typed and what to
// say about the last change. Add and Delete apply at once, one word each (T11.3e: the page
// never sends a whole list, so it cannot remove a word another computer added since it read
// the list); the rules (trimming, repeats, limits, words not in use) are the app's, and a
// repeat is what the add result says, never inferred from the list.

import { useCallback, useState } from "react";

import { type SummaryVocabularyView, errorText, useIpc } from "../ipc";
import { type Load, useLoad } from "./parts";

export type DictionaryLoad = Load<SummaryVocabularyView>;

export interface DictionaryState {
  load: DictionaryLoad;
  /** The word in the Add field. */
  draft: string;
  setDraft(text: string): void;
  /** Why the last Add was refused, or that its word was already there (null when neither). */
  addMessage: { intent: "error" | "warning"; text: string } | null;
  /** Why the last Delete failed (null when it did not). */
  deleteError: string | null;
  busy: boolean;
  /** Adds the typed word; the field empties once it is stored or found already there. */
  add(): Promise<void>;
  /** Removes `word`. */
  remove(word: string): Promise<void>;
}

/** What the page says when the typed word was already in the list (in any case). */
export const ALREADY_THERE = "That word is already in your dictionary.";

/** Loads the Dictionary words and changes them. */
export function useDictionary(): DictionaryState {
  const ipc = useIpc();
  const [load, setLoad] = useLoad((ipc) => ipc.summaryVocabulary());
  const [draft, setDraftText] = useState("");
  const [addMessage, setAddMessage] = useState<DictionaryState["addMessage"]>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const run = useCallback(async <T,>(change: () => Promise<T>) => {
    setBusy(true);
    try {
      return await change();
    } finally {
      setBusy(false);
    }
  }, []);

  return {
    load,
    draft,
    setDraft: (text) => {
      setDraftText(text);
      setAddMessage(null);
    },
    addMessage,
    deleteError,
    busy,
    add: async () => {
      if (draft.trim() === "" || busy) return;
      setDeleteError(null);
      try {
        const result = await run(() => ipc.addSummaryVocabularyWord(draft));
        setLoad({ status: "loaded", view: result.view });
        setDraftText("");
        setAddMessage(result.added ? null : { intent: "warning", text: ALREADY_THERE });
      } catch (e) {
        setAddMessage({ intent: "error", text: errorText(e) });
      }
    },
    remove: async (word) => {
      setAddMessage(null);
      try {
        const view = await run(() => ipc.removeSummaryVocabularyWord(word));
        setLoad({ status: "loaded", view });
        setDeleteError(null);
      } catch (e) {
        setDeleteError(errorText(e));
      }
    },
  };
}
