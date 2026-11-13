// The search above the meeting list (T9.7): the box, its debounced search and the results that
// take the list's place while it holds text. The search text lives only in this state.

import {
  Body1,
  Button,
  Caption1,
  SearchBox,
  makeStyles,
  mergeClasses,
  tokens,
} from "@fluentui/react-components";
import { type KeyboardEvent, useEffect, useState } from "react";

import { type Ipc, IpcFailure, type SearchResultItem, type SearchResultsView } from "../ipc";
import { clock } from "../meeting/liveMeeting";
import type { Timer } from "../time";
import { clockTime, displayTitle } from "./meetings";

/** How long typing must pause before a search runs. */
export const SEARCH_DEBOUNCE_MS = 200;

/** The error kind of a search a newer one replaced before it answered (`ipc::SEARCH_SUPERSEDED_KIND`
 * in Rust, T9.24): the app stops it and the newer search's answer is the one wanted. */
export const SEARCH_SUPERSEDED_KIND = "superseded";

const useStyles = makeStyles({
  box: {
    width: "100%",
    maxWidth: "100%",
  },
  list: {
    listStyleType: "none",
    margin: 0,
    padding: 0,
  },
  note: {
    display: "block",
    paddingTop: tokens.spacingVerticalXS,
    paddingBottom: tokens.spacingVerticalS,
    paddingLeft: tokens.spacingHorizontalS,
    paddingRight: tokens.spacingHorizontalS,
    color: tokens.colorNeutralForeground3,
  },
  row: {
    width: "100%",
    height: "auto",
    justifyContent: "flex-start",
    textAlign: "left",
    paddingTop: tokens.spacingVerticalS,
    paddingBottom: tokens.spacingVerticalS,
    paddingLeft: tokens.spacingHorizontalS,
    paddingRight: tokens.spacingHorizontalS,
  },
  rowSelected: {
    backgroundColor: tokens.colorSubtleBackgroundSelected,
  },
  rowText: {
    display: "flex",
    flexDirection: "column",
    minWidth: 0,
    width: "100%",
  },
  rowTitle: {
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
  },
  rowMeta: {
    color: tokens.colorNeutralForeground3,
  },
  snippet: {
    display: "-webkit-box",
    WebkitBoxOrient: "vertical",
    WebkitLineClamp: 2,
    overflow: "hidden",
    overflowWrap: "anywhere",
    fontWeight: tokens.fontWeightRegular,
    color: tokens.colorNeutralForeground2,
  },
});

/** The last answer: the query it answers, and what was found (`null`: the search failed). */
interface Answer {
  query: string;
  results: SearchResultsView | null;
}

export interface MeetingSearch {
  /** What the box holds. */
  query: string;
  setQuery: (query: string) => void;
  /** Whether results take the list's place: the box holds more than white space. */
  active: boolean;
  /** The latest answer, kept on screen until the next one so results do not flicker. */
  answer: Answer | null;
  /** Whether `answer` answers what the box holds now. */
  current: boolean;
}

/** The search's state. It runs again when `reload` moves, so results follow a meeting deleted or
 * changed while they show. */
export function useMeetingSearch(ipc: Ipc, timer: Timer, reload: number): MeetingSearch {
  const [query, setQuery] = useState("");
  const [answer, setAnswer] = useState<Answer | null>(null);
  const wanted = query.trim();

  useEffect(() => {
    if (wanted === "") {
      setAnswer(null);
      return;
    }
    let live = true;
    const cancel = timer.setTimeout(() => {
      ipc.searchMeetings(wanted).then(
        (results) => {
          if (live) setAnswer({ query: wanted, results });
        },
        // Fixed copy on the screen: an error's own text has no place beside the search text.
        // A search a newer one replaced has no answer: the newer one's is wanted (T9.24).
        (error: unknown) => {
          if (error instanceof IpcFailure && error.error.kind === SEARCH_SUPERSEDED_KIND) return;
          if (live) setAnswer({ query: wanted, results: null });
        },
      );
    }, SEARCH_DEBOUNCE_MS);
    return () => {
      live = false;
      cancel();
    };
  }, [ipc, timer, wanted, reload]);

  return { query, setQuery, active: wanted !== "", answer, current: answer?.query === wanted };
}

/** The search box. Esc clears it. */
export function MeetingSearchBox({
  query,
  onChange,
}: {
  query: string;
  onChange: (query: string) => void;
}) {
  const styles = useStyles();
  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Escape" && query !== "") {
      e.preventDefault();
      onChange("");
    }
  };
  return (
    <div role="search">
      <SearchBox
        className={styles.box}
        aria-label="Search meetings"
        placeholder="Search meetings"
        value={query}
        onChange={(_, data) => onChange(data.value)}
        onKeyDown={onKeyDown}
        dismiss={{ "aria-label": "Clear search" }}
      />
    </div>
  );
}

/** A result's second line: "Mon 2 Nov 14:00 · Transcript 12:04". */
export function resultLine(item: SearchResultItem): string {
  const date = new Date(item.started_at_ms).toLocaleDateString(undefined, {
    weekday: "short",
    day: "numeric",
    month: "short",
  });
  const started = `${date} ${clockTime(item.started_at_ms)}`;
  switch (item.matched) {
    case "Title":
      return started;
    case "Summary":
      return `${started} · Summary`;
    case "Transcript":
      return item.line_ms === null
        ? `${started} · Transcript`
        : `${started} · Transcript ${clock(item.line_ms)}`;
  }
}

/** What a search says beside its results; `null` when it has nothing to say. */
function statusText(search: MeetingSearch): string | null {
  if (!search.current || search.answer === null) return null;
  const { results } = search.answer;
  if (results === null) return "The search did not finish. Try again.";
  if (results.items.length === 0) return "No meetings found";
  if (results.capped) {
    return `Showing the newest ${results.items.length} matches. Add a word to narrow them.`;
  }
  return null;
}

/** The results, in the meeting list's place. */
export function MeetingSearchResults({
  search,
  selectedId,
  onPick,
}: {
  search: MeetingSearch;
  selectedId: string | null;
  onPick: (id: string) => void;
}) {
  const styles = useStyles();
  const items = search.answer?.results?.items ?? [];
  const status = statusText(search);
  return (
    <>
      <div role="status">
        {status !== null && <Caption1 className={styles.note}>{status}</Caption1>}
      </div>
      {items.length > 0 && (
        <ul className={styles.list} aria-label="Search results">
          {items.map((item) => {
            const isSelected = item.id === selectedId;
            return (
              <li key={item.id}>
                <Button
                  appearance="subtle"
                  className={mergeClasses(styles.row, isSelected && styles.rowSelected)}
                  aria-current={isSelected ? "true" : undefined}
                  onClick={() => onPick(item.id)}
                >
                  <span className={styles.rowText}>
                    <Body1 className={styles.rowTitle}>{displayTitle(item)}</Body1>
                    <Caption1 className={styles.rowMeta}>{resultLine(item)}</Caption1>
                    {/* A title match shows itself in the line above. */}
                    {item.matched !== "Title" && (
                      <Caption1 className={styles.snippet}>{item.snippet}</Caption1>
                    )}
                  </span>
                </Button>
              </li>
            );
          })}
        </ul>
      )}
    </>
  );
}
