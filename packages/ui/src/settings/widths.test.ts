// T12.95: every Settings page and part, and the first-run wizard that reuses them, takes its
// widths from the names in `parts.tsx`; no file picks a width of its own. Only `maxWidth` is
// guarded: a literal `width:` (a progress bar's, a sidebar's) is out of scope.

import { describe, expect, it } from "vitest";

import { BOX_WIDTH, FIELD_WIDTH, NARROW_WIDTH, PAGE_WIDTH, TEXT_WIDTH } from "./parts";

const SOURCES = import.meta.glob(
  ["./**/*.{ts,tsx}", "!./**/*.test.{ts,tsx}", "../firstRun/**/*.{ts,tsx}", "!../firstRun/**/*.test.{ts,tsx}"],
  { query: "?raw", import: "default", eager: true },
) as Record<string, string>;

/** A `maxWidth` whose whole value is not one of the five names (a literal, a ternary, an
 * expression), with white space and line breaks allowed anywhere around it. */
const NOT_A_NAME = /maxWidth\s*:(?!\s*(?:PAGE|TEXT|BOX|FIELD|NARROW)_WIDTH\s*[,}])/g;

/** Each `maxWidth` not given one of the names, as `file:line`. */
function unnamedMaxWidths(file: string, source: string): string[] {
  return [...source.matchAll(NOT_A_NAME)].map((m) => `${file}:${source.slice(0, m.index).split("\n").length}`);
}

describe("Settings widths", () => {
  it("are five distinct values named once in parts.tsx", () => {
    const widths = [PAGE_WIDTH, TEXT_WIDTH, FIELD_WIDTH, NARROW_WIDTH, BOX_WIDTH];
    for (const width of widths) expect(width).not.toBe("");
    expect(new Set(widths).size).toBe(widths.length);
  });

  it("finds a maxWidth not given a name (controls)", () => {
    expect(unnamedMaxWidths("a.tsx", 'x: {\n  maxWidth: "520px",\n}')).toEqual(["a.tsx:2"]);
    expect(unnamedMaxWidths("a.tsx", "  maxWidth: `${n}px`,")).toEqual(["a.tsx:1"]);
    expect(unnamedMaxWidths("a.tsx", "  maxWidth: 0,")).toEqual(["a.tsx:1"]);
    expect(unnamedMaxWidths("a.tsx", "  maxWidth: wide ? FIELD_WIDTH : TEXT_WIDTH,")).toEqual(["a.tsx:1"]);
    expect(unnamedMaxWidths("a.tsx", 'x: {\n  maxWidth:\n    "520px",\n}')).toEqual(["a.tsx:2"]);
    expect(unnamedMaxWidths("a.tsx", "  maxWidth: FIELD_WIDTH_X,")).toEqual(["a.tsx:1"]);
    expect(unnamedMaxWidths("a.tsx", "  maxWidth: FIELD_WIDTH,")).toEqual([]);
    expect(unnamedMaxWidths("a.tsx", "  maxWidth:\n    TEXT_WIDTH,")).toEqual([]);
    expect(unnamedMaxWidths("a.tsx", "  x: { maxWidth: BOX_WIDTH }")).toEqual([]);
  });

  it("leaves no other maxWidth in a Settings file or the first-run wizard", () => {
    const files = Object.keys(SOURCES);
    expect(files).toContain("./parts.tsx");
    expect(files).toContain("./BenchmarkSection.tsx");
    expect(files).toContain("../firstRun/FirstRunWizard.tsx");
    const found = Object.entries(SOURCES).flatMap(([file, source]) => unnamedMaxWidths(file, source));
    expect(found).toEqual([]);
  });
});
