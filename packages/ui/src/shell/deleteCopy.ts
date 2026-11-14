// The words of a meeting deletion that cannot go now (T12.38), as `deletion.rs` says them. The one
// TypeScript copy, read from `fixtures/ui/delete_copy_cases.json`, which the Rust tests check
// `deletion.rs` against; the delete question and the mock both read it.

import table from "../../../../fixtures/ui/delete_copy_cases.json";

const { _comment: _, ...copy } = table;

export const DELETE_COPY: Readonly<Omit<typeof table, "_comment">> = copy;
