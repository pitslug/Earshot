// T12.95: every Settings page and part, and the first-run wizard that reuses them, takes its
// widths from the names in `parts.tsx`; no file picks a width of its own.

import { describe, expect, it } from "vitest";

import { BOX_WIDTH, FIELD_WIDTH, NARROW_WIDTH, PAGE_WIDTH, TEXT_WIDTH } from "./parts";

const SOURCES = import.meta.glob(
  ["./*.{ts,tsx}", "!./*.test.{ts,tsx}", "../firstRun/*.{ts,tsx}", "!../firstRun/*.test.{ts,tsx}"],
  { query: "?raw", import: "default", eager: true },
) as Record<string, string>;

/** Each `maxWidth:` given a literal (a string, template or number) rather than a name, as
 * `file:line`. */
function literalMaxWidths(file: string, source: string): string[] {
  return source.split("\n").flatMap((line, i) => (/maxWidth\s*:\s*["'`\d]/.test(line) ? [`${file}:${i + 1}`] : []));
}

describe("Settings widths", () => {
  it("are named once in parts.tsx", () => {
    for (const width of [PAGE_WIDTH, TEXT_WIDTH, FIELD_WIDTH, NARROW_WIDTH, BOX_WIDTH]) {
      expect(typeof width).toBe("string");
    }
  });

  it("finds a literal maxWidth (a positive control)", () => {
    expect(literalMaxWidths("a.tsx", 'x: {\n  maxWidth: "520px",\n}')).toEqual(["a.tsx:2"]);
    expect(literalMaxWidths("a.tsx", "  maxWidth: `${n}px`,")).toEqual(["a.tsx:1"]);
    expect(literalMaxWidths("a.tsx", "  maxWidth: 0,")).toEqual(["a.tsx:1"]);
    expect(literalMaxWidths("a.tsx", "  maxWidth: FIELD_WIDTH,")).toEqual([]);
  });

  it("leaves no literal maxWidth in a Settings file or the first-run wizard", () => {
    const files = Object.keys(SOURCES);
    expect(files).toContain("./parts.tsx");
    expect(files).toContain("./BenchmarkSection.tsx");
    expect(files).toContain("../firstRun/FirstRunWizard.tsx");
    const found = Object.entries(SOURCES).flatMap(([file, source]) => literalMaxWidths(file, source));
    expect(found).toEqual([]);
  });
});
