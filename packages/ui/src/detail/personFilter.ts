// The Speakers panel's person filter (T12.80): which saved people a typed name lists, and
// which person a whole typed name means. The fold mirrors `earshot_runtime::edit::
// person_name_key` (case folded one character at a time, each Latin letter with a diacritic
// folded to its base letters from the table Rust generates, white space trimmed and every run
// of it one space); `fixtures/ui/person_filter_cases.json` pins both.

import type { PersonItem } from "../ipc";
import { BASE_LETTERS } from "./baseLetters";

/** A name as the filter compares it. */
export function personNameKey(name: string): string {
  let folded = "";
  for (const c of name) {
    for (const lower of c.toLowerCase()) folded += BASE_LETTERS[lower] ?? lower;
  }
  return folded
    .split(/\p{White_Space}+/u)
    .filter((part) => part !== "")
    .join(" ");
}

/** Whether a person called `name` is listed for the typed `query`: any part of the name,
 * case and accents ignored. An empty query lists everyone. */
export function personMatches(name: string, query: string): boolean {
  return personNameKey(name).includes(personNameKey(query));
}

/** The persons listed for `query`, in the order given. */
export function personsMatching(query: string, persons: PersonItem[]): PersonItem[] {
  return persons.filter((p) => personMatches(p.name, query));
}

/** Every person other than the user whose whole name is `name`, case and accents ignored. */
export function personsNamed(name: string, persons: PersonItem[]): PersonItem[] {
  const key = personNameKey(name);
  if (key === "") return [];
  return persons.filter((p) => !p.is_me && personNameKey(p.name) === key);
}

/** How many meetings a person is in, as the list says it beside their name. */
export function meetingsText(n: number): string {
  if (n === 0) return "no meetings yet";
  return n === 1 ? "1 meeting" : `${n} meetings`;
}
