// The mock's meeting search: the same rules as `earshot_runtime::search` over the sample
// meetings (literal text, case ignored; title, then transcript, then summary; newest first).

import type {
  MeetingDetail,
  MeetingListItem,
  SearchMatch,
  SearchResultItem,
  SearchResultsView,
  SummaryView,
} from "./ipc";

/** The most meetings one search returns (`search::MAX_RESULTS`). */
export const SEARCH_MAX_RESULTS = 50;

const SNIPPET_BEFORE = 30;
const SNIPPET_AFTER = 70;

function snippetOf(text: string, query: string): string | null {
  const at = text.toLowerCase().indexOf(query);
  if (at < 0) return null;
  const before = Array.from(text.slice(0, at));
  const after = Array.from(text.slice(at + query.length));
  return [
    before.length > SNIPPET_BEFORE ? "..." : "",
    before.slice(-SNIPPET_BEFORE).join(""),
    text.slice(at, at + query.length),
    after.slice(0, SNIPPET_AFTER).join(""),
    after.length > SNIPPET_AFTER ? "..." : "",
  ].join("");
}

function summaryTexts(s: SummaryView): string[] {
  return [
    s.title,
    s.tldr,
    ...s.decisions.map((d) => d.text),
    ...s.actions.flatMap((a) => [a.task, a.owner, ...(a.due === null ? [] : [a.due])]),
    ...s.notes.flatMap((n) => [n.heading, ...n.bullets.map((b) => b.text)]),
    ...s.open_questions,
  ];
}

function hitIn(row: MeetingListItem, detail: MeetingDetail, query: string): SearchResultItem | null {
  const hit = (matched: SearchMatch, snippet: string, line_ms: number | null): SearchResultItem => ({
    id: row.id,
    title: row.title,
    started_at_ms: row.started_at_ms,
    matched,
    snippet,
    line_ms,
  });
  const title = snippetOf(row.title, query);
  if (title !== null) return hit("Title", title, null);
  for (const line of detail.transcript) {
    const snippet = snippetOf(line.text, query);
    if (snippet !== null) return hit("Transcript", snippet, line.start_ms);
  }
  for (const text of detail.summary === null ? [] : summaryTexts(detail.summary)) {
    const snippet = snippetOf(text, query);
    if (snippet !== null) return hit("Summary", snippet, null);
  }
  return null;
}

/** What a search of `meetings` for `query` finds. */
export function searchSample(
  meetings: MeetingListItem[],
  detailOf: (row: MeetingListItem) => MeetingDetail,
  query: string,
): SearchResultsView {
  const wanted = query.trim().toLowerCase();
  if (wanted === "") return { items: [], capped: false };
  const items = [...meetings]
    .sort((a, b) => b.started_at_ms - a.started_at_ms || b.id.localeCompare(a.id))
    .flatMap((row) => hitIn(row, detailOf(row), wanted) ?? []);
  return { items: items.slice(0, SEARCH_MAX_RESULTS), capped: items.length > SEARCH_MAX_RESULTS };
}
