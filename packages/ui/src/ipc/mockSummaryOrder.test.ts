// The mock's order of summary writers against the shared copy table and the Rust rules it mirrors
// (`summary_order::SummaryOrder::with_writer`, `live_summaries_allowed`, the refusals).

import { describe, expect, it } from "vitest";

import table from "../../../../fixtures/ui/summary_order_cases.json";
import { liveAllowed, mirrorWriter, mockDestination, mockStepLabel, withWriter } from "./mockSummaryOrder";
import type { StepChoice } from "./summaryOrder";
import { fillCopy } from "./summaryOrder";

const LOCAL: StepChoice = { kind: "Local" };
const CLAUDE: StepChoice = { kind: "ClaudeCode" };
const CODEX: StepChoice = { kind: "Codex" };
const DESK: StepChoice = { kind: "Computer", device: "0f3a91c2-0000-7000-8000-000000000000" };

describe("mock order of summary writers", () => {
  it("labels each step and its destination from the shared table", () => {
    const cx = {
      provider: "OpenAiCompatible" as const,
      host: "models.example.test",
      computers: new Map([[DESK.kind === "Computer" ? DESK.device : "", { name: "Invented desk", writer: "local model" }]]),
    };
    expect(mockStepLabel(LOCAL, cx)).toBe(table.steps.local);
    expect(mockDestination(LOCAL, cx)).toBe(table.destinations.local);
    expect(mockStepLabel({ kind: "Endpoint" }, cx)).toBe("Model server (models.example.test)");
    expect(mockDestination({ kind: "Endpoint" }, { ...cx, provider: "Anthropic" })).toBe(
      table.destinations.endpoint_anthropic,
    );
    expect(mockStepLabel({ kind: "Endpoint" }, { ...cx, host: null })).toBe(table.steps.endpoint_unset);
    expect(mockStepLabel(DESK, cx)).toBe("On Invented desk (local model)");
    expect(mockDestination(DESK, cx)).toBe("Goes to Invented desk, which writes it with its local model");
    expect(mockStepLabel(DESK, { ...cx, computers: new Map() })).toBe(
      fillCopy(table.steps.computer_no_writer, { device: table.unnamed }),
    );
  });

  it("follows the Rust rules for the old writer setting and live summaries", () => {
    expect(withWriter([DESK, LOCAL], "ClaudeCode")).toEqual([DESK, CLAUDE]);
    expect(withWriter([LOCAL, CLAUDE], "ClaudeCode")).toEqual([CLAUDE]);
    expect(withWriter([DESK, LOCAL], "NoSummary")).toEqual([DESK]);
    expect(withWriter([DESK], "Codex")).toEqual([DESK, CODEX]);
    expect(mirrorWriter([DESK, LOCAL])).toBe("OnThisComputer");
    expect(mirrorWriter([DESK])).toBe("NoSummary");
    expect(liveAllowed([LOCAL, CLAUDE])).toBe(true);
    expect(liveAllowed([DESK, LOCAL])).toBe(true);
    expect(liveAllowed([CLAUDE, LOCAL])).toBe(false);
    expect(liveAllowed([DESK, CLAUDE, LOCAL])).toBe(false);
    expect(liveAllowed([])).toBe(false);
  });

  it("keeps public copy only: no em dash", () => {
    expect(JSON.stringify(table)).not.toContain("—");
  });
});
