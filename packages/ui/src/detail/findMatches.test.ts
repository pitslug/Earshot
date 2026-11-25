import { describe, expect, it } from "vitest";

import {
  FIND_IDLE,
  FIND_TOO_SHORT,
  type FindRow,
  findCountText,
  findInTranscript,
  findSpeakers,
  speakerKey,
  textMatches,
} from "./findMatches";

/** A row of an invented transcript: `who` is the speaker id and its shown name. */
const row = (who: string | null, text: string, name = who ?? "Unknown speaker"): FindRow => ({
  speaker_id: who,
  speaker: name,
  text,
});

const ROWS: FindRow[] = [
  row("ana", "The gravel arrives on Monday."),
  row("ana", "Then we spread it."),
  row("ben", "Gravel is heavy."),
  row("ana", "Who orders the gravel?"),
  row("ben", "I will."),
  row("ben", "Today."),
];

describe("transcript find", () => {
  it("transcript find: a speaker alone steps through turn starts", () => {
    expect(findInTranscript(ROWS, "ana", "")).toEqual({ kind: "turns", lines: [0, 3] });
    expect(findInTranscript(ROWS, "ben", "  ")).toEqual({ kind: "turns", lines: [2, 4] });
  });

  it("transcript find: nothing chosen finds nothing", () => {
    expect(findInTranscript(ROWS, null, "")).toEqual({ kind: "idle" });
    expect(findInTranscript(ROWS, null, " \t ")).toEqual({ kind: "idle" });
  });

  it("transcript find: text alone finds every occurrence, case ignored", () => {
    expect(findInTranscript(ROWS, null, "GRAVEL")).toEqual({
      kind: "matches",
      matches: [
        { line: 0, start: 4, end: 10 },
        { line: 2, start: 0, end: 6 },
        { line: 3, start: 15, end: 21 },
      ],
    });
  });

  it("transcript find: a Latin letter alone is no text filter; a speaker alone needs none", () => {
    expect(findInTranscript(ROWS, null, "g")).toEqual({ kind: "idle" });
    expect(findInTranscript(ROWS, null, " G ")).toEqual({ kind: "idle" });
    expect(findInTranscript(ROWS, "ana", "g")).toEqual({ kind: "turns", lines: [0, 3] });
    expect(findCountText({ kind: "idle" }, 0, "g")).toBe(FIND_TOO_SHORT);
    expect(findCountText({ kind: "idle" }, 0, " ")).toBe(FIND_IDLE);
    // One digit, one CJK character or one letter outside the basic plane is a real search.
    const other: FindRow[] = [row("ana", "Gate 7 opens."), row("ben", "会议在东京。"), row("ana", "x 𐐀y")];
    expect(findInTranscript(other, null, "7")).toEqual({ kind: "matches", matches: [{ line: 0, start: 5, end: 6 }] });
    expect(findInTranscript(other, null, "东")).toEqual({ kind: "matches", matches: [{ line: 1, start: 3, end: 4 }] });
    expect(findInTranscript(other, null, " 𐐨 ")).toEqual({ kind: "matches", matches: [{ line: 2, start: 2, end: 4 }] });
    // Two characters look.
    expect(findInTranscript(ROWS, null, "IT")).toEqual({
      kind: "matches",
      matches: [{ line: 1, start: 15, end: 17 }],
    });
  });

  it("transcript find: text is trimmed before matching", () => {
    expect(findInTranscript(ROWS, null, "  heavy ")).toEqual({
      kind: "matches",
      matches: [{ line: 2, start: 10, end: 15 }],
    });
  });

  it("transcript find: speaker and text find matches only in that speaker's lines", () => {
    expect(findInTranscript(ROWS, "ben", "gravel")).toEqual({
      kind: "matches",
      matches: [{ line: 2, start: 0, end: 6 }],
    });
    expect(findInTranscript(ROWS, "ben", "monday")).toEqual({ kind: "matches", matches: [] });
  });

  it("transcript find: occurrences do not overlap, adjacent ones all count", () => {
    expect(textMatches("aaaa", "aa")).toEqual([
      [0, 2],
      [2, 4],
    ]);
    expect(textMatches("aaa", "aa")).toEqual([[0, 2]]);
    expect(textMatches("abab", "ab")).toEqual([
      [0, 2],
      [2, 4],
    ]);
    expect(textMatches("abc", "")).toEqual([]);
    expect(textMatches("ab", "abc")).toEqual([]);
  });

  it("transcript find: non-ASCII case folds and maps back to the shown text", () => {
    expect(textMatches("Ärger über ÄRGER", "ärger")).toEqual([
      [0, 5],
      [11, 16],
    ]);
    expect(textMatches("ΣΟΦΙΑ", "σοφια")).toEqual([[0, 5]]);
    // A letter outside the basic plane counts as one character, its two code units kept.
    expect(textMatches("x 𐐀y", "𐐨y")).toEqual([[2, 5]]);
    // A capital that lowercases to two characters still maps back to its own range.
    expect(textMatches("İstanbul", "i̇stan")).toEqual([[0, 5]]);
  });

  it("transcript find: a draft line without a speaker id is keyed by its shown name", () => {
    const draft = [row(null, "one", "Speaker 1"), row(null, "two", "Speaker 1"), row(null, "three", "Speaker 2")];
    expect(speakerKey(draft[0])).toBe(speakerKey(draft[1]));
    expect(speakerKey(draft[0])).not.toBe(speakerKey(draft[2]));
    expect(findInTranscript(draft, speakerKey(draft[2]), "")).toEqual({ kind: "turns", lines: [2] });
  });

  it("transcript find: the speakers are those with lines, in order of first line", () => {
    expect(findSpeakers(ROWS)).toEqual([
      { key: "ana", name: "ana" },
      { key: "ben", name: "ben" },
    ]);
    expect(findSpeakers([])).toEqual([]);
  });
});
