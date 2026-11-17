// The person filter (T12.80) against the shared case table, which
// `earshot_runtime::edit::edit_person_filter_matches_the_shared_cases` also runs.

import { describe, expect, it } from "vitest";

import cases from "../../../../fixtures/ui/person_filter_cases.json";
import type { PersonItem } from "../ipc";
import { meetingsText, personMatches, personNameKey, personsNamed } from "./personFilter";

const person = (id: string, name: string, is_me = false): PersonItem => ({
  id,
  name,
  has_voiceprint: false,
  is_me,
  meetings: 0,
});

describe("personFilter", () => {
  it("folds names as the runtime does", () => {
    expect(cases.keys.length).toBeGreaterThanOrEqual(8);
    for (const c of cases.keys) expect(personNameKey(c.name), JSON.stringify(c.name)).toBe(c.key);
  });

  it("lists a person for any part of their name, case and accents ignored", () => {
    expect(cases.matches.length).toBeGreaterThanOrEqual(8);
    for (const c of cases.matches) {
      expect(personMatches(c.name, c.query), `${c.name} for ${c.query}`).toBe(c.matches);
    }
  });

  it("finds every other person with a whole name, never the user", () => {
    const persons = [
      person("a", "Sam Lee"),
      person("b", "sam  LEE"),
      person("c", "Sam Leeds"),
      person("me", "Sam Lee", true),
    ];
    expect(personsNamed(" Sam Lee ", persons).map((p) => p.id)).toEqual(["a", "b"]);
    expect(personsNamed("", persons)).toEqual([]);
  });

  it("says how many meetings", () => {
    expect([0, 1, 3].map(meetingsText)).toEqual(["no meetings yet", "1 meeting", "3 meetings"]);
  });
});
