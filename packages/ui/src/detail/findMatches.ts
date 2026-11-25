// Find in a meeting's transcript (T12.98): by speaker, by text, or both. Pure: the bar
// (`TranscriptFind.tsx`) and the lines draw what this returns. Text is matched on each line's
// shown text, case ignored per character as meeting search folds it
// (`earshot_runtime::search::fold_case`, Rust `char::to_lowercase`), with every folded
// character mapped back to the range of the character it came from.

import type { TranscriptRow } from "../ipc";

/** What find reads of a transcript line. */
export type FindRow = Pick<TranscriptRow, "speaker_id" | "speaker" | "text">;

/** A speaker the bar offers. */
export interface FindSpeaker {
  /** `speakerKey` of their lines. */
  key: string;
  /** The name the transcript shows. */
  name: string;
}

/** One text match: line `line` (index in the shown transcript), code units `start..end` of its
 * text. */
export interface FindMatch {
  line: number;
  start: number;
  end: number;
}

/** What find found. `idle`: no speaker chosen and no text typed. */
export type FindResult =
  | { kind: "idle" }
  | { kind: "turns"; lines: number[] }
  | { kind: "matches"; matches: FindMatch[] };

/** Which speaker a line is: its speaker row, or for a line with none (a live draft before
 * speakers were found, a line no speaker was given) the name it shows. */
export function speakerKey(r: FindRow): string {
  return r.speaker_id ?? `name:${r.speaker}`;
}

/** The speakers with lines in `rows`, each once, in order of their first line. */
export function findSpeakers(rows: FindRow[]): FindSpeaker[] {
  const seen = new Set<string>();
  const out: FindSpeaker[] = [];
  for (const r of rows) {
    const key = speakerKey(r);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ key, name: r.speaker });
  }
  return out;
}

/** `text` folded per character, each folded character with the code-unit range of the
 * character it came from. */
function folded(text: string): { chars: string[]; from: number[]; to: number[] } {
  const chars: string[] = [];
  const from: number[] = [];
  const to: number[] = [];
  let at = 0;
  for (const ch of text) {
    const end = at + ch.length;
    for (const f of ch.toLowerCase()) {
      chars.push(f);
      from.push(at);
      to.push(end);
    }
    at = end;
  }
  return { chars, from, to };
}

/** Every occurrence of `query` in `text`, case ignored, left to right and not overlapping, as
 * code-unit ranges of `text`. An empty query finds nothing. */
export function textMatches(text: string, query: string): [number, number][] {
  const q = [...query].flatMap((c) => [...c.toLowerCase()]);
  if (q.length === 0) return [];
  const t = folded(text);
  const out: [number, number][] = [];
  let i = 0;
  while (i + q.length <= t.chars.length) {
    let k = 0;
    while (k < q.length && t.chars[i + k] === q[k]) k += 1;
    if (k === q.length) {
      out.push([t.from[i], t.to[i + q.length - 1]]);
      i += q.length;
    } else i += 1;
  }
  return out;
}

/** The fewest characters (code points, after trimming) text find looks for (T14.17): one letter
 * matches nearly every line of a long transcript. */
export const FIND_MIN_CHARS = 2;

/** How long typing in the find box must pause before find looks (T14.17), so a long transcript
 * is not searched and redrawn on every keystroke. Clearing the box applies at once. */
export const FIND_PAUSE_MS = 250;

/** `text` trimmed, or "" when it is shorter than [`FIND_MIN_CHARS`]. */
export function findQuery(text: string): string {
  const query = text.trim();
  return [...query].length < FIND_MIN_CHARS ? "" : query;
}

/** What find finds in `rows` for speaker `speaker` (a `speakerKey`, or `null` for any) and text
 * `text` (trimmed; empty or shorter than [`FIND_MIN_CHARS`] means no text filter). A speaker
 * alone gives the start of each of their turns (consecutive lines of one speaker are one turn);
 * text gives every match, in that speaker's lines only when one is chosen. */
export function findInTranscript(rows: FindRow[], speaker: string | null, text: string): FindResult {
  const query = findQuery(text);
  if (query === "") {
    if (speaker === null) return { kind: "idle" };
    const lines: number[] = [];
    rows.forEach((r, i) => {
      if (speakerKey(r) === speaker && (i === 0 || speakerKey(rows[i - 1]) !== speaker)) lines.push(i);
    });
    return { kind: "turns", lines };
  }
  const matches: FindMatch[] = [];
  rows.forEach((r, line) => {
    if (speaker !== null && speakerKey(r) !== speaker) return;
    for (const [start, end] of textMatches(r.text, query)) matches.push({ line, start, end });
  });
  return { kind: "matches", matches };
}

/** How many places `result` steps through. */
export function findCount(result: FindResult): number {
  return result.kind === "turns" ? result.lines.length : result.kind === "matches" ? result.matches.length : 0;
}

/** The line place `at` of `result` is on. */
export function findLine(result: FindResult, at: number): number | null {
  if (result.kind === "turns") return result.lines[at] ?? null;
  if (result.kind === "matches") return result.matches[at]?.line ?? null;
  return null;
}

/** The count the bar shows for place `at` of `result`; `typed` is the find box's text, so a
 * box too short to look for says so. */
export function findCountText(result: FindResult, at: number, typed = ""): string {
  if (result.kind === "idle") {
    return typed.trim() === "" ? "Pick a speaker or type" : "Type 2 or more characters";
  }
  const n = findCount(result);
  if (n === 0) return "No matches";
  return `${result.kind === "turns" ? "Turn" : "Match"} ${at + 1} of ${n}`;
}
