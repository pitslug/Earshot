import { describe, expect, it } from "vitest";

import type { Citation, SummaryView } from "../ipc";
import { summaryMarkdown } from "./detail";

const CITE: Citation = { segment_id: "0190aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee", row: 41 };

const EMPTY: SummaryView = {
  id: "5a000000-0000-7000-8000-000000000001",
  title: "A title that stays out",
  tldr: "",
  decisions: [],
  actions: [],
  notes: [],
  open_questions: [],
};

describe("summaryMarkdown", () => {
  it("writes every section in order, without title or citations", () => {
    const text = summaryMarkdown({
      ...EMPTY,
      tldr: "The harbour tour moves to spring.",
      decisions: [{ text: "Book the larger boat", citations: [CITE] }],
      actions: [
        { owner: "Robin", task: "Ring the marina", due: "Friday", citations: [CITE] },
        { owner: "Jules", task: "Draft the route", due: null, citations: [] },
      ],
      notes: [
        { heading: "Costs", bullets: [{ text: "Fuel is up", citations: [CITE] }] },
        { heading: "Weather", bullets: [{ text: "Spring is calmer", citations: [] }] },
      ],
      open_questions: ["Who brings the lunch?"],
    });
    expect(text).toBe(
      [
        "## TL;DR",
        "",
        "The harbour tour moves to spring.",
        "",
        "## Decisions",
        "",
        "- Book the larger boat",
        "",
        "## Actions",
        "",
        "- Robin: Ring the marina (by Friday)",
        "- Jules: Draft the route",
        "",
        "## Costs",
        "",
        "- Fuel is up",
        "",
        "## Weather",
        "",
        "- Spring is calmer",
        "",
        "## Open questions",
        "",
        "- Who brings the lunch?",
        "",
      ].join("\n"),
    );
    expect(text).not.toContain(CITE.segment_id);
    expect(text).not.toContain("[42]");
    expect(text).not.toContain(EMPTY.title);
  });

  it("leaves out empty sections and blank items", () => {
    expect(
      summaryMarkdown({
        ...EMPTY,
        tldr: "Only this.",
        decisions: [{ text: "   ", citations: [] }],
        actions: [{ owner: "Robin", task: " ", due: "Friday", citations: [] }],
        notes: [
          { heading: "No bullets", bullets: [] },
          { heading: "Blank bullets", bullets: [{ text: "", citations: [] }] },
        ],
        open_questions: [" "],
      }),
    ).toBe("## TL;DR\n\nOnly this.\n");
  });

  it("keeps a section whose TL;DR is empty out and starts at the first real one", () => {
    expect(
      summaryMarkdown({ ...EMPTY, decisions: [{ text: "Go ahead", citations: [] }] }),
    ).toBe("## Decisions\n\n- Go ahead\n");
  });

  it("is empty when the summary holds nothing", () => {
    expect(summaryMarkdown(EMPTY)).toBe("");
    expect(summaryMarkdown({ ...EMPTY, tldr: " \n " })).toBe("");
  });

  it("leaves out an owner or due date that is blank", () => {
    expect(
      summaryMarkdown({
        ...EMPTY,
        actions: [
          { owner: " ", task: "Ring the marina", due: "Friday", citations: [] },
          { owner: "Robin", task: "Draft the route", due: " ", citations: [] },
          { owner: "", task: "Pack the charts", due: null, citations: [] },
        ],
      }),
    ).toBe(
      "## Actions\n\n- Ring the marina (by Friday)\n- Robin: Draft the route\n- Pack the charts\n",
    );
  });

  it("keeps each list item and heading on one line", () => {
    expect(
      summaryMarkdown({
        ...EMPTY,
        tldr: "First line.\n\nSecond line.",
        decisions: [{ text: "Book the boat\nand the crew", citations: [] }],
        actions: [{ owner: "Robin\nLee", task: "Ring\r\nthe marina", due: "next\nweek", citations: [] }],
        notes: [{ heading: "Costs\nand fuel", bullets: [{ text: "Up\n  again", citations: [] }] }],
        open_questions: ["Who\nbrings lunch?"],
      }),
    ).toBe(
      [
        "## TL;DR",
        "",
        "First line.",
        "",
        "Second line.",
        "",
        "## Decisions",
        "",
        "- Book the boat and the crew",
        "",
        "## Actions",
        "",
        "- Robin Lee: Ring the marina (by next week)",
        "",
        "## Costs and fuel",
        "",
        "- Up again",
        "",
        "## Open questions",
        "",
        "- Who brings lunch?",
        "",
      ].join("\n"),
    );
  });

  it("writes the bullets of a note with no heading on their own", () => {
    expect(
      summaryMarkdown({
        ...EMPTY,
        tldr: "Short.",
        notes: [{ heading: " ", bullets: [{ text: "A bullet", citations: [] }] }],
      }),
    ).toBe("## TL;DR\n\nShort.\n\n- A bullet\n");
  });
});
