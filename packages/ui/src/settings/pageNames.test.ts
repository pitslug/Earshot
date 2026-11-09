// No text the app shows names a Settings page that no longer exists (T12.5b): Models and
// Benchmark were dissolved into System, Transcription and Summaries, Speakers became People, and
// Obsidian and Storage became Notes and storage. The Rust side has the same guard
// (`page_names_tests.rs` in the desktop app); a rule change edits both.

import { describe, expect, it } from "vitest";

import { SETTINGS_SECTIONS } from "./SettingsView";

/** The page names that are gone. */
const GONE_PAGES = ["Models", "Benchmark", "Speakers", "Obsidian", "Storage"] as const;

/** The ways text names a page: "the Models page", "Settings, Models". */
function stalePageNames(text: string): string[] {
  const found: string[] = [];
  for (const page of GONE_PAGES) {
    for (const form of [`${page} page`, `Settings, ${page}`]) {
      const at = new RegExp(`${form.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\b`);
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
    expect(stalePageNames("Open Settings, Models to download them.")).toEqual(["Settings, Models"]);
    expect(stalePageNames("Download it on the Models page.")).toEqual(["Models page"]);
    expect(stalePageNames("Choose them on the Speakers page or in Settings, Storage.")).toEqual([
      "Speakers page",
      "Settings, Storage",
    ]);
    for (const page of SETTINGS_SECTIONS) {
      expect(stalePageNames(`Open Settings, ${page}. It is on the ${page} page.`)).toEqual([]);
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
