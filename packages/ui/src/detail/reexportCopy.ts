// The question before exporting a meeting again (T14.63): a re-export replaces the whole
// Obsidian note. The one TypeScript copy, read from `fixtures/ui/reexport_copy_cases.json`,
// which `earshot_runtime::export`'s tests check its own copy against.

import table from "../../../../fixtures/ui/reexport_copy_cases.json";

const { _comment: _, ...copy } = table;

export const REEXPORT_COPY: Readonly<Omit<typeof table, "_comment">> = copy;
