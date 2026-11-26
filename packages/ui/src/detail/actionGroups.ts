// The summary's actions as the meeting page shows them (T14.60): "My actions" and "Other
// actions", each in meeting order, with one item for actions the summary lists once per owner.
// Display only: the stored summary, Copy summary and the Obsidian note keep every action as
// written.

import type { ActionView, Citation } from "../ipc";

/** One shown action: one or more stored actions with the same task, due date and citations. */
export interface ActionItem {
  /** The stored actions it shows, by index in the summary's `actions`, in summary order. */
  indices: number[];
  /** Their owners, each once, in summary order. */
  owners: string[];
  task: string;
  due: string | null;
  citations: Citation[];
}

/** How the actions are shown: two groups when the user is known, else one list. */
export type ActionGroups =
  | { kind: "split"; mine: ActionItem[]; others: ActionItem[] }
  | { kind: "single"; all: ActionItem[] };

/** What makes two stored actions one item. The due date is part of it as well as the task and
 * citations: an item shows one date, so two dates stay two items rather than losing one. */
function itemKey(a: ActionView): string {
  return JSON.stringify([a.task, a.due, a.citations.map((c) => c.segment_id)]);
}

/** Where an item sits in the meeting: its earliest cited line, or after every cited item when it
 * cites none (summary order among those). */
function meetingPlace(item: ActionItem): number {
  let first = Number.POSITIVE_INFINITY;
  for (const c of item.citations) if (c.row !== null && c.row < first) first = c.row;
  return first;
}

/** The stored actions as items, merged by task, due date and citations, in meeting order. */
export function mergeActions(actions: ActionView[]): ActionItem[] {
  const items: ActionItem[] = [];
  const byKey = new Map<string, ActionItem>();
  actions.forEach((a, i) => {
    const key = itemKey(a);
    const found = byKey.get(key);
    if (found !== undefined) {
      found.indices.push(i);
      if (!found.owners.includes(a.owner)) found.owners.push(a.owner);
      return;
    }
    const item: ActionItem = { indices: [i], owners: [a.owner], task: a.task, due: a.due, citations: a.citations };
    byKey.set(key, item);
    items.push(item);
  });
  // A stable sort: items citing the same first line keep summary order.
  return items
    .map((item, order) => ({ item, order, place: meetingPlace(item) }))
    .sort((a, b) => (a.place === b.place ? a.order - b.order : a.place < b.place ? -1 : 1))
    .map((x) => x.item);
}

/**
 * The actions split into the user's own and everyone else's. `isUser` says whether an owner is
 * the user; `null` when the user has no name set, so whose an action is cannot be known: one
 * list then. An item any of whose owners is the user is the user's.
 */
export function actionGroups(actions: ActionView[], isUser: ((owner: string) => boolean) | null): ActionGroups {
  const items = mergeActions(actions);
  if (isUser === null) return { kind: "single", all: items };
  const mine: ActionItem[] = [];
  const others: ActionItem[] = [];
  for (const item of items) (item.owners.some(isUser) ? mine : others).push(item);
  return { kind: "split", mine, others };
}

/** Names joined for reading: "A", "A and B", "A, B and C". */
export function joinNames(names: string[]): string {
  if (names.length <= 1) return names[0] ?? "";
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}
