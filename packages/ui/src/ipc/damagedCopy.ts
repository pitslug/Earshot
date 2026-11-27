// The damaged-items copy (T14.65, docs/sync-spec.md 17.8): the banner, Details, Repair from
// sync and the one refusal line. The text lives only in `fixtures/ui/damaged_copy_cases.json`,
// which the desktop's tests read too; this module only fills it in.

import table from "../../../../fixtures/ui/damaged_copy_cases.json";
import type { DamageCountsView, KindsOff } from "./bindings";

/** The whole copy table. */
export const DAMAGED_COPY = table;

/** The IPC error kind of an action refused because of a damaged row. */
export const DAMAGED_KIND = "damaged";

/** The kinds in the order Details lists them. */
export const DAMAGE_KINDS = ["meetings", "parts", "people", "voice", "folders", "words"] as const;

/** One kind of damaged item. */
export type DamageKind = (typeof DAMAGE_KINDS)[number];

/** No damage in any kind. */
export const NO_DAMAGE: DamageCountsView = { meetings: 0, parts: 0, people: 0, voice: 0, folders: 0, words: 0 };

/** The plain-words line for `count` items of `kind` (one or many form). */
export function kindLine(kind: DamageKind, count: number): string {
  const forms = table.kinds[kind];
  return count === 1 ? forms.one : forms.many.replace("{count}", () => String(count));
}

/** The lines for every kind with something damaged, in Details' order. */
export function kindLines(counts: DamageCountsView): { kind: DamageKind; text: string }[] {
  return DAMAGE_KINDS.filter((kind) => counts[kind] > 0).map((kind) => ({ kind, text: kindLine(kind, counts[kind]) }));
}

/** Every damaged item, all kinds together. */
export function totalDamaged(counts: DamageCountsView): number {
  return DAMAGE_KINDS.reduce((sum, kind) => sum + counts[kind], 0);
}

/** Whether any kind holds more than it did in `before` (what brings a dismissed banner back). */
export function moreInAnyKind(now: DamageCountsView, before: DamageCountsView): boolean {
  return DAMAGE_KINDS.some((kind) => now[kind] > before[kind]);
}

/** The switched-off kinds' sentences for what is damaged in `counts`, in Details' order. */
export function kindOffLines(counts: DamageCountsView, off: KindsOff): string[] {
  const lines: string[] = [];
  if (off.voice && counts.voice > 0) lines.push(table.kind_off_voice);
  if (off.words && counts.words > 0) lines.push(table.kind_off_words);
  return lines;
}

/** Whether something damaged in `counts` lies outside the switched-off kinds, so the sync
 * server had no good copy of it (`nothing_healable`). */
export function leftOutsideSwitchedOff(counts: DamageCountsView, off: KindsOff): boolean {
  const inOff = (off.voice ? counts.voice : 0) + (off.words ? counts.words : 0);
  return totalDamaged(counts) - inOff > 0;
}

/** What Details says while Repair reads: with the percentage once known. */
export function readingText(percent: number | null): string {
  return percent === null
    ? table.repair_reading_start
    : table.repair_reading.replace("{percent}", () => String(percent));
}
