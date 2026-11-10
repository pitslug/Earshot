import { describe, expect, it } from "vitest";

import { saveStep } from "./saveOnNext";

describe("saving a first-run step on Next", () => {
  it("runs every part even after one refuses, and says the step was not all saved", async () => {
    const ran: string[] = [];
    const part = (name: string, ok: boolean) => async () => {
      ran.push(name);
      return ok;
    };
    expect(await saveStep([part("endpoint", false), part("name", true), part("vault", false)])).toBe(false);
    // Each part either saved or showed its own reason: none is left unsaved without one.
    expect(ran).toEqual(["endpoint", "name", "vault"]);
  });

  it("saves a step whose parts all saved, and a step with none", async () => {
    expect(await saveStep([async () => true, async () => true])).toBe(true);
    expect(await saveStep([])).toBe(true);
  });
});
