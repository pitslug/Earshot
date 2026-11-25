// The mock's record of what was sent runs the app's shared table (T14.51): Rust's
// `meeting_events_snapshot_cases_are_the_shared_table` runs the same file.

import { describe, expect, it } from "vitest";

import cases from "../../../../fixtures/ui/meeting_snapshot_cases.json";
import type { MeetingEvent, MeetingSnapshot } from "./bindings";
import { createSentRecord } from "./mockSnapshot";

interface Case {
  name: string;
  events: { seq: number; at_ms: number; event: unknown }[];
  now_ms: number;
  expect: unknown;
}

describe("mock meeting snapshot", () => {
  it("has the shared cases", () => {
    expect((cases.cases as Case[]).length).toBeGreaterThanOrEqual(5);
  });
  for (const c of cases.cases as Case[]) {
    it(`mock snapshot: ${c.name}`, () => {
      const r = createSentRecord();
      for (const sent of c.events) r.apply(sent.seq, sent.event as MeetingEvent, sent.at_ms);
      expect(r.snapshot(c.now_ms)).toEqual(c.expect as MeetingSnapshot);
    });
  }
});
