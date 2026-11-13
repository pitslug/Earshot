// No text the app shows names a Settings page that no longer exists (T12.5b): Models and
// Benchmark were dissolved into System, Transcription and Summaries, Speakers became People, and
// Obsidian and Storage became Notes and storage. The Rust side has the same guard
// (`page_names_tests.rs` in the desktop app); both take the gone pages and the forms from
// `fixtures/ui/page_name_cases.json` and run its cases, so a rule change edits that file.

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/page_name_cases.json";
import { SETTINGS_SECTIONS } from "./SettingsView";

/** The gone page names `text` uses, in the shared table's forms ("the Models page", "Settings,
 * Models", "see Models"), as whole words: no Unicode letter, number or underscore right after. */
function stalePageNames(text: string): string[] {
  const found: string[] = [];
  for (const page of table.gone_pages) {
    for (const form of table.forms.map((f) => f.replace("{page}", page))) {
      // As Rust's `char::is_alphanumeric`: any Unicode letter or number, not only ASCII.
      const at = new RegExp(`${form.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?![\\p{Alphabetic}\\p{N}_])`, "u");
      if (at.test(text)) found.push(form);
    }
  }
  return found;
}

/** The source with comments left out: a line whose first characters open or continue a comment. */
function code(text: string): string {
  return text
    .split("\n")
    .filter((line) => {
      const t = line.trimStart();
      return !(t.startsWith("//") || t.startsWith("*") || t.startsWith("/*"));
    })
    .join("\n");
}

const SOURCES = import.meta.glob(
  [
    "../**/*.{ts,tsx}",
    "!../**/*.test.{ts,tsx}",
    "!../test/**",
    "../../../../apps/desktop/src/**/*.{ts,tsx}",
  ],
  { query: "?raw", import: "default", eager: true },
);

describe("Settings page names", () => {
  it("finds a gone page in text and nothing in the pages that exist", () => {
    expect(table.forms).toHaveLength(3);
    expect(table.cases.length).toBeGreaterThanOrEqual(7);
    for (const c of table.cases) {
      expect(stalePageNames(c.text), c.text).toEqual(c.found);
    }
    for (const page of SETTINGS_SECTIONS) {
      expect(stalePageNames(`Open Settings, ${page}. It is on the ${page} page; see ${page}.`)).toEqual([]);
    }
    // Comments may keep history; shown text may not.
    expect(code("// was on the Models page\nconst x = 1;")).toBe("const x = 1;");
  });

  it("names no gone page in any shown text of the UI", () => {
    const files = Object.keys(SOURCES);
    // The scan reads the whole UI, the mock and the desktop app's own sources.
    expect(files.some((f) => f.endsWith("/SettingsView.tsx"))).toBe(true);
    expect(files.some((f) => f.endsWith("/AppShell.tsx"))).toBe(true);
    expect(files.some((f) => f.endsWith("ipc/mockBenchmark.ts"))).toBe(true);
    expect(files.some((f) => f.endsWith("apps/desktop/src/main.tsx"))).toBe(true);
    expect(files.some((f) => f.includes(".test."))).toBe(false);
    const found = files.flatMap((f) => stalePageNames(code(SOURCES[f])).map((form) => `${f}: ${form}`));
    expect(found).toEqual([]);
  });
});
