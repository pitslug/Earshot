// The summary style section's state (T7.4b): the stored style, the text being edited and the
// refusal to show. The rules (length, characters, trimming) are the app's; nothing is
// decided here.

import { useCallback, useEffect, useState } from "react";

import { type SummaryStyleView, useIpc } from "../ipc";
import { errorText } from "./useModels";

export type SummaryStyleLoad =
  | { status: "loading" }
  | { status: "failed"; message: string }
  | { status: "loaded"; view: SummaryStyleView };

export interface SummaryStyleState {
  load: SummaryStyleLoad;
  /** The text in the field. */
  draft: string;
  setDraft(text: string): void;
  /** Whether the field differs from what is stored. */
  changed: boolean;
  /** True from a successful save until the field changes. */
  saved: boolean;
  /** Why the last save was refused (null when it was not). */
  error: string | null;
  busy: boolean;
  /** Stores the field's text. */
  save(): Promise<void>;
  /** Stores no style (the default) and empties the field. */
  reset(): Promise<void>;
}

/** Loads the summary style and saves it. */
export function useSummaryStyle(): SummaryStyleState {
  const ipc = useIpc();
  const [load, setLoad] = useState<SummaryStyleLoad>({ status: "loading" });
  const [draft, setDraftText] = useState("");
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let live = true;
    ipc.summaryStyle().then(
      (view) => {
        if (!live) return;
        setLoad({ status: "loaded", view });
        setDraftText(view.style);
      },
      (e: unknown) => {
        if (live) setLoad({ status: "failed", message: errorText(e) });
      },
    );
    return () => {
      live = false;
    };
  }, [ipc]);

  const store = useCallback(
    async (text: string) => {
      setBusy(true);
      try {
        const view = await ipc.setSummaryStyle(text);
        setLoad({ status: "loaded", view });
        setDraftText(view.style);
        setSaved(true);
        setError(null);
      } catch (e) {
        setSaved(false);
        setError(errorText(e));
      } finally {
        setBusy(false);
      }
    },
    [ipc],
  );

  const stored = load.status === "loaded" ? load.view.style : "";
  return {
    load,
    draft,
    setDraft: (text) => {
      setDraftText(text);
      setSaved(false);
    },
    // As the app stores it (`SummaryStyle::new`: line ends to LF, trimmed), so spaces or a
    // line end around the saved text are not a change.
    changed: draft.replace(/\r\n?/g, "\n").trim() !== stored,
    saved,
    error,
    busy,
    save: () => store(draft),
    reset: () => store(""),
  };
}
