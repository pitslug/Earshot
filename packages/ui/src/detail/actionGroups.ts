// The summary's actions as the meeting page shows them (T14.60): "My actions" and "Other
// actions", each in meeting order, with one item for actions the summary lists once per owner.
// Display only: the stored summary, Copy summary and the Obsidian note keep every action as
// written. The owner rule is pinned by `fixtures/ui/action_owner_cases.json`, which the Rust
// side reads too (T14.61).

import type { ActionView, Citation } from "../ipc";
import { personNameKey } from "./personFilter";

/** One shown action: one or more stored actions with the same task, due date and citations. */
export interface ActionItem {
  /** The stored actions it shows, by index in the summary's `actions`, in summary order. */
  indices: number[];
  /** The names its owners hold, each once (folded as people's names are compared, the first
   * spelling kept), in summary order. */
  owners: string[];
  task: string;
  due: string | null;
  citations: Citation[];
}

/** How the actions are shown: two groups when the user is known, else one list. */
export type ActionGroups =
  | { kind: "split"; mine: ActionItem[]; others: ActionItem[] }
  | { kind: "single"; all: ActionItem[] };

/** Owners that name no one person: kept whole, as written. Compared without case. */
export const GROUP_OWNERS = ["everyone", "all", "team"];

/** The names in one action's owner: split on ", ", " and " and " & ", each trimmed, empty ones
 * dropped. "Everyone", "All" and "Team" stay whole. */
export function ownerNames(owner: string): string[] {
  const whole = owner.trim();
  if (GROUP_OWNERS.includes(whole.toLowerCase())) return [whole];
  return whole
    .split(/, | and | & /)
    .map((n) => n.trim())
    .filter((n) => n !== "");
}

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

/** Adds `names` to `owners` unless a name with the same folded key is there already. */
function addOwners(owners: string[], names: string[]) {
  for (const name of names) {
    const key = personNameKey(name);
    if (!owners.some((o) => personNameKey(o) === key)) owners.push(name);
  }
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
      addOwners(found.owners, ownerNames(a.owner));
      return;
    }
    const item: ActionItem = { indices: [i], owners: [], task: a.task, due: a.due, citations: a.citations };
    addOwners(item.owners, ownerNames(a.owner));
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
 * The actions split into the user's own and everyone else's. `isUser` says whether a name is
 * the user; `null` when the user has no name set, so whose an action is cannot be known: one
 * list then. An item any of whose owner names is the user is the user's.
 */
export function actionGroups(actions: ActionView[], isUser: ((name: string) => boolean) | null): ActionGroups {
  const items = mergeActions(actions);
  if (isUser === null) return { kind: "single", all: items };
  const mine: ActionItem[] = [];
  const others: ActionItem[] = [];
  for (const item of items) (item.owners.some(isUser) ? mine : others).push(item);
  return { kind: "split", mine, others };
}

/** The names shown after an item: in the user's group the others who share it, else every
 * owner. */
export function shownOwners(item: ActionItem, mine: boolean, isUser: ((name: string) => boolean) | null): string[] {
  return mine && isUser !== null ? item.owners.filter((o) => !isUser(o)) : item.owners;
}

/** Names joined for reading: "A", "A and B", "A, B and C". */
export function joinNames(names: string[]): string {
  if (names.length <= 1) return names[0] ?? "";
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}
