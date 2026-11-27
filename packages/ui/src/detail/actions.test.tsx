// The summary's actions on the meeting page (T14.60): "My actions" and "Other actions", shared
// actions merged into one item, and one list when the user has no name set. Names are invented.

import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { type ActionView, type Citation, sampleDetail, samplePersons } from "../ipc";
import { ITEM, setup } from "../test/meetingDetail";
import ownerTable from "../../../../fixtures/ui/action_owner_cases.json";
import { actionGroups, joinNames, mergeActions, ownerNames, shownOwners } from "./actionGroups";
import { userNamed } from "./SpeakersPanel";

const base = sampleDetail(ITEM);
const summary = base.summary!;
// Citations of real lines of the sample transcript: the earlier and the later one.
const early: Citation[] = summary.actions[0].citations;
const late: Citation[] = summary.actions[1].citations;

const act = (owner: string, task: string, citations: Citation[], due: string | null = null): ActionView => ({
  owner,
  task,
  due,
  citations,
});

/** The sample meeting with these actions. Casey is the user in the sample persons. */
function withActions(actions: ActionView[]) {
  return { details: { [ITEM.id]: { ...base, summary: { ...summary, actions } } } };
}

const items = (name: string) =>
  within(screen.getByRole("list", { name })).getAllByRole("listitem").map((li) => li.textContent ?? "");

describe("mergeActions", () => {
  it("makes one item of actions with the same task, due date and citations, owners in order", () => {
    const merged = mergeActions([
      act("Robin Vale", "Book the crane", late),
      act("Casey", "Book the crane", late),
      act("Robin Vale", "Book the crane", late),
      act("Tamsin Ode", "Book the crane", early),
    ]);
    expect(merged.map((m) => [m.indices, m.owners])).toEqual([
      // In meeting order: the earlier citation first.
      [[3], ["Tamsin Ode"]],
      [[0, 1, 2], ["Robin Vale", "Casey"]],
    ]);
  });

  it("keeps actions apart when their due dates differ", () => {
    expect(mergeActions([act("A", "Ship it", early, "Friday"), act("B", "Ship it", early, "Monday")])).toHaveLength(2);
  });

  it("puts actions that cite nothing after the cited ones, in summary order", () => {
    const merged = mergeActions([act("A", "One", []), act("B", "Two", late), act("C", "Three", [])]);
    expect(merged.map((m) => m.task)).toEqual(["Two", "One", "Three"]);
  });
});

interface OwnerCase {
  note: string;
  owner: string;
  user: string | null;
  group: "mine" | "others" | "single";
  shown: string[];
}

describe("action owners (fixtures/ui/action_owner_cases.json)", () => {
  for (const c of (ownerTable as { cases: OwnerCase[] }).cases) {
    it(c.note, () => {
      const persons = c.user === null ? [] : [{ ...samplePersons()[0], name: c.user, is_me: true }];
      const isUser = c.user === null ? null : (name: string) => userNamed(name, persons) !== null;
      const groups = actionGroups([act(c.owner, "Invented task", early)], isUser);
      const [group, item] =
        groups.kind === "single"
          ? (["single", groups.all[0]] as const)
          : groups.mine.length > 0
            ? (["mine", groups.mine[0]] as const)
            : (["others", groups.others[0]] as const);
      expect(group).toBe(c.group);
      expect(shownOwners(item, group === "mine", isUser)).toEqual(c.shown);
    });
  }

  it("folds one name across merged actions, keeping the first spelling", () => {
    const [item] = mergeActions([act("Robin Vale", "Same", early), act("ROBIN  VALE", "Same", early)]);
    expect(item.owners).toEqual(["Robin Vale"]);
    expect(item.indices).toEqual([0, 1]);
  });

  it("splits only on the listed separators", () => {
    expect(ownerNames(" Robin Vale ")).toEqual(["Robin Vale"]);
    expect(ownerNames("Robin Vale, , Tamsin Ode")).toEqual(["Robin Vale", "Tamsin Ode"]);
    expect(ownerNames("Everyone")).toEqual(["Everyone"]);
    expect(ownerNames("ALL")).toEqual(["ALL"]);
  });
});

describe("actionGroups", () => {
  it("puts an item any of whose owners is the user in the user's group", () => {
    const groups = actionGroups(
      [act("Robin Vale", "Shared", early), act("Casey", "Shared", early), act("Tamsin Ode", "Theirs", late)],
      (o) => o === "Casey",
    );
    expect(groups.kind).toBe("split");
    if (groups.kind !== "split") return;
    expect(groups.mine.map((i) => i.task)).toEqual(["Shared"]);
    expect(groups.others.map((i) => i.task)).toEqual(["Theirs"]);
  });

  it("is one list when the user has no name", () => {
    expect(actionGroups([act("Casey", "Mine", early)], null)).toEqual({
      kind: "single",
      all: mergeActions([act("Casey", "Mine", early)]),
    });
  });

  it("joins names for reading", () => {
    expect(joinNames(["A"])).toBe("A");
    expect(joinNames(["A", "B"])).toBe("A and B");
    expect(joinNames(["A", "B", "C"])).toBe("A, B and C");
  });
});

describe("the meeting page's actions", () => {
  it("shows the user's actions, then everyone else's, with owners only where they say something", async () => {
    await setup(
      withActions([
        act("Robin Vale", "Order the seals", late),
        act("Casey", "Call the vendor", early),
        act("Robin Vale", "Book the crane", late),
        act("Casey", "Book the crane", late),
        act("Tamsin Ode", "Check the drawings", early),
        act("Robin Vale", "Check the drawings", early),
      ]),
    );
    await screen.findByText(/TL;DR/);
    expect(screen.getByRole("heading", { name: "My actions" })).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Other actions" })).toBeInTheDocument();
    const [call, crane] = items("My actions");
    expect(call).toMatch(/^Call the vendor\[\d+\]$/);
    // A shared action: once, under the user, with the others after its citation.
    expect(crane).toMatch(/^Book the crane\[\d+\] · with Robin Vale$/);
    // Everyone else's in meeting order, owners after each, a shared one once with both names.
    expect(items("Other actions")).toEqual([
      expect.stringMatching(/^Check the drawings\[\d+\] · Tamsin Ode and Robin Vale$/),
      expect.stringMatching(/^Order the seals\[\d+\] · Robin Vale$/),
    ]);
    expect(screen.queryByText(/Tick actions/)).toBeNull();
  });

  it("puts an owner naming the user among others under My actions, the others after \"with\"", async () => {
    await setup(withActions([act("casey & Robin Vale", "Book the crane", late)]));
    await screen.findByText(/TL;DR/);
    expect(items("My actions")).toEqual([expect.stringMatching(/^Book the crane\[\d+\] · with Robin Vale$/)]);
    expect(screen.queryByRole("list", { name: "Other actions" })).toBeNull();
  });

  it("leaves out a group with nothing in it", async () => {
    await setup(withActions([act("Robin Vale", "Order the seals", late)]));
    await screen.findByText(/TL;DR/);
    expect(screen.queryByRole("list", { name: "My actions" })).toBeNull();
    expect(screen.queryByRole("heading", { name: "My actions" })).toBeNull();
    expect(items("Other actions")).toHaveLength(1);
  });

  it("shows one list with owners and no headings when the user has no name", async () => {
    const persons = samplePersons().map((p) => (p.is_me ? { ...p, name: "Me" } : p));
    await setup({
      persons,
      ...withActions([
        act("Casey", "Call the vendor", early),
        act("Robin Vale", "Book the crane", late),
        act("Tamsin Ode", "Book the crane", late),
      ]),
    });
    await screen.findByText(/TL;DR/);
    expect(screen.queryByRole("heading", { name: "My actions" })).toBeNull();
    expect(screen.queryByRole("heading", { name: "Other actions" })).toBeNull();
    expect(items("Actions")).toEqual([
      expect.stringMatching(/^Call the vendor\[\d+\] · Casey$/),
      expect.stringMatching(/^Book the crane\[\d+\] · Robin Vale and Tamsin Ode$/),
    ]);
  });

  it("edits the stored action an item shows", async () => {
    const { user, calls } = await setup(withActions([act("Robin Vale", "Order the seals", late)]));
    await screen.findByText(/TL;DR/);
    await user.click(screen.getByRole("button", { name: "Edit action 1" }));
    const field = screen.getByRole("textbox", { name: "action 1" });
    await user.clear(field);
    await user.type(field, "Order the gaskets{Enter}");
    expect(await screen.findByText("Order the gaskets")).toBeInTheDocument();
    expect(calls).toContainEqual(["editSummary", ITEM.id, summary.id, { kind: "ActionTask", index: 0 }, "Order the gaskets"]);
  });

  it("offers each merged action's own pencils, so one edit changes one stored action", async () => {
    const { user, calls } = await setup(
      withActions([act("Robin Vale", "Book the crane", late), act("Tamsin Ode", "Book the crane", late)]),
    );
    await screen.findByText(/TL;DR/);
    expect(screen.getByRole("button", { name: "Edit action 1 for Robin Vale" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Edit who does action 1 for Robin Vale" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Edit who does action 2 for Tamsin Ode" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Edit action 2 for Tamsin Ode" }));
    const field = screen.getByRole("textbox", { name: "action 2 for Tamsin Ode" });
    await user.clear(field);
    await user.type(field, "Book the big crane{Enter}");
    expect(calls).toContainEqual(["editSummary", ITEM.id, summary.id, { kind: "ActionTask", index: 1 }, "Book the big crane"]);
    // The two now differ: two items, each with its own owner.
    expect(await screen.findByText("Book the big crane")).toBeInTheDocument();
    expect(items("Other actions")).toEqual([
      expect.stringMatching(/^Book the crane\[\d+\] · Robin Vale$/),
      expect.stringMatching(/^Book the big crane\[\d+\] · Tamsin Ode$/),
    ]);
  });
});
