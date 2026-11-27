// The Summary tab's sections (T14.70): TL;DR, Decisions, Actions and Notes as headings of one
// size, the summary's topic sections as sub-headings under Notes after a divider, and no Notes
// heading when there is no topic. Content is invented.

import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { type SummaryView, sampleDetail } from "../ipc";
import { ITEM, setup } from "../test/meetingDetail";

const base = sampleDetail(ITEM);
const summary = base.summary!;

const topic = (heading: string, bullet: string): SummaryView["notes"][number] => ({
  heading,
  bullets: [{ text: bullet, citations: [] }],
});

/** The sample meeting with these topic sections. */
function withNotes(notes: SummaryView["notes"]) {
  return { details: { [ITEM.id]: { ...base, summary: { ...summary, notes } } } };
}

/** The page's plain separators: the Speakers panel's resize handle is a focusable separator too. */
const dividers = () => screen.queryAllByRole("separator").filter((s) => !s.hasAttribute("aria-valuenow"));

const headings = (level: number) => screen.getAllByRole("heading", { level }).map((h) => h.textContent);

describe("the Summary tab's sections", () => {
  it("puts the topic sections under one Notes heading, after a divider, as sub-headings", async () => {
    await setup(
      withNotes([topic("Crane booking", "The crane is free on Thursday"), topic("Site access", "Gate two opens at seven")]),
    );
    await screen.findByText(/TL;DR/);
    // One size for the main sections; the topics a level down, like "My actions".
    const main = headings(2);
    expect(main).toEqual(expect.arrayContaining(["TL;DR", "Decisions", "Actions", "Notes"]));
    expect(main).not.toContain("Crane booking");
    expect(main.indexOf("Actions")).toBeLessThan(main.indexOf("Notes"));
    const sub = headings(3);
    expect(sub).toEqual(expect.arrayContaining(["Crane booking", "Site access"]));
    const notes = screen.getByRole("heading", { level: 2, name: "Notes" });
    const crane = screen.getByRole("heading", { level: 3, name: "Crane booking" });
    const access = screen.getByRole("heading", { level: 3, name: "Site access" });
    expect(notes.compareDocumentPosition(crane) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(crane.compareDocumentPosition(access) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    // The thin line comes just before the Notes heading.
    const [divider, ...more] = dividers();
    expect(more).toEqual([]);
    expect(divider.compareDocumentPosition(notes) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(screen.getByText("Gate two opens at seven")).toBeInTheDocument();
  });

  it("draws the main headings larger than the sub-headings", async () => {
    await setup({
      details: {
        [ITEM.id]: {
          ...base,
          summary: {
            ...summary,
            notes: [topic("Crane booking", "The crane is free on Thursday")],
            open_questions: ["Who signs off the crane hire?"],
          },
        },
      },
    });
    await screen.findByText(/TL;DR/);
    // The owner's mock-up (2026-11-28): the main headings at 20px (Fluent's Subtitle1), the
    // sub-headings smaller.
    for (const name of ["TL;DR", "Decisions", "Actions", "Notes", "Open questions"]) {
      const h = screen.getByRole("heading", { level: 2, name });
      expect(h).toHaveClass("fui-Subtitle1");
    }
    for (const name of ["My actions", "Other actions", "Crane booking"]) {
      const h = screen.getByRole("heading", { level: 3, name });
      expect(h).not.toHaveClass("fui-Subtitle1");
      expect(h).toHaveClass("fui-Body1Strong");
    }
  });

  it("shows no Notes heading and no divider when the summary has no topic section", async () => {
    await setup(withNotes([]));
    await screen.findByText(/TL;DR/);
    expect(screen.getByRole("heading", { level: 2, name: "Actions" })).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Notes" })).toBeNull();
    expect(dividers()).toEqual([]);
  });

  it("no longer points at the Obsidian note for ticking actions", async () => {
    await setup(withNotes([topic("Crane booking", "The crane is free on Thursday")]));
    await screen.findByText(/TL;DR/);
    expect(screen.queryByText(/Tick actions/)).toBeNull();
    // The Summary tab's own content: the element holding the sections (the TL;DR section's
    // parent). Positive control: it holds the Actions and Notes sections.
    const tab = screen.getByRole("heading", { level: 2, name: "TL;DR" }).parentElement!.parentElement!;
    expect(within(tab).getByRole("heading", { level: 2, name: "Actions" })).toBeInTheDocument();
    expect(within(tab).getByRole("heading", { level: 3, name: "Crane booking" })).toBeInTheDocument();
    expect(tab.textContent ?? "").not.toMatch(/Obsidian note/);
  });

  it("edits a topic's sub-heading from its own Edit button", async () => {
    const { user, calls } = await setup(withNotes([topic("Crane booking", "The crane is free on Thursday")]));
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: "Edit heading Crane booking" }));
    const field = screen.getByRole("textbox", { name: "heading Crane booking" });
    expect(field).toHaveValue("Crane booking");
    await user.clear(field);
    await user.type(field, "Crane hire{Enter}");
    expect(await screen.findByRole("heading", { level: 3, name: "Crane hire" })).toBeInTheDocument();
    expect(calls).toContainEqual(["editSummary", ITEM.id, summary.id, { kind: "NoteHeading", section: 0 }, "Crane hire"]);
  });

  it("takes a topic bullet's citation to its line in the transcript", async () => {
    // The bullet cites the first action's line; the button is found inside the bullet only.
    const cited = summary.actions[0].citations;
    const row = cited[0].row;
    if (row === null) throw new Error("the sample's first action cites a line of the transcript");
    const { user } = await setup(
      withNotes([{ heading: "Crane booking", bullets: [{ text: "The crane is free on Thursday", citations: cited }] }]),
    );
    await screen.findByText(/TL;DR/);
    const bullet = screen.getByText("The crane is free on Thursday").closest("li")!;
    const [cite] = within(bullet).getAllByRole("button", { name: /^Show line \d+ in the transcript$/ });
    const line = row + 1;
    expect(cite).toHaveAccessibleName(`Show line ${line} in the transcript`);
    await user.click(cite);
    expect(screen.getByRole("tab", { name: "Transcript" })).toHaveAttribute("aria-selected", "true");
    const rows = within(screen.getByRole("table", { name: "Transcript" })).getAllByRole("row");
    expect(rows[row]).toHaveAttribute("aria-current", "true");
  });
});
