// The question before exporting a meeting again (T14.63): a re-export replaces the whole
// Obsidian note. The one TypeScript copy, read from `fixtures/ui/reexport_copy_cases.json`,
// which `earshot_runtime::export`'s tests check its own copy against.

import table from "../../../../fixtures/ui/reexport_copy_cases.json";

const { _comment: _, ...copy } = table;

export const REEXPORT_COPY: Readonly<Omit<typeof table, "_comment">> = copy;

/** What to ask once before exporting several meetings when `exportedBefore` of them were
 * exported before; `null` when none were (`earshot_runtime::export::reexport_batch_warning`). */
export function reexportBatchQuestion(exportedBefore: number): string | null {
  if (exportedBefore <= 0) return null;
  if (exportedBefore === 1) return REEXPORT_COPY.batch_one;
  return REEXPORT_COPY.batch_many.replace("{count}", String(exportedBefore));
}
