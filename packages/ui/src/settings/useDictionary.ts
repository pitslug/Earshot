// The Dictionary page's state (T12.15a): the stored words, the word being typed and what to
// say about the last change. Every change stores the whole list at once (the save rule: Add
// and Delete apply at once); the rules (trimming, repeats, limits) are the app's, so nothing
// is decided here: a repeat is noticed only because the stored list did not grow.

import { useCallback, useEffect, useState } from "react";

import { type SummaryVocabularyView, useIpc } from "../ipc";
import { errorText } from "./useModels";

export type DictionaryLoad =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "loaded"; view: SummaryVocabularyView };

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
  /** Stores the list with the typed word added; the field empties once it is stored. */
  add(): Promise<void>;
  /** Stores the list without `word`. */
  remove(word: string): Promise<void>;
}

/** What the page says when the typed word was already in the list (in any case). */
export const ALREADY_THERE = "That word is already in your dictionary.";

/** Loads the Dictionary words and changes them. */
export function useDictionary(): DictionaryState {
  const ipc = useIpc();
  const [load, setLoad] = useState<DictionaryLoad>({ status: "loading" });
  const [draft, setDraftText] = useState("");
  const [addMessage, setAddMessage] = useState<DictionaryState["addMessage"]>(null);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    ipc.summaryVocabulary().then(
      (view) => {
        if (live) setLoad({ status: "loaded", view });
      },
      (e: unknown) => {
        if (live) setLoad({ status: "failed", message: errorText(e) });
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  const words = load.status === "loaded" ? load.view.words : [];

  const store = useCallback(
    async (next: string[]) => {
      setBusy(true);
      try {
        const view = await ipc.setSummaryVocabulary(next);
        setLoad({ status: "loaded", view });
        return view;
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

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
        const view = await store([...words, draft]);
        setDraftText("");
        setAddMessage(view.words.length > words.length ? null : { intent: "warning", text: ALREADY_THERE });
      } catch (e) {
        setAddMessage({ intent: "error", text: errorText(e) });
      }
    },
    remove: async (word) => {
      setAddMessage(null);
      try {
        await store(words.filter((w) => w !== word));
        setDeleteError(null);
      } catch (e) {
        setDeleteError(errorText(e));
      }
    },
  };
}
