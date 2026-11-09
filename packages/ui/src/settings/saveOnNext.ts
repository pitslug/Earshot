// How a Settings part saves in the first-run wizard, which has no Save buttons (docs/settings-design.md):
// the part registers what it would save, and the wizard's Next runs it before moving on.

import { useEffect, useRef } from "react";

/** Saves a part's fields; resolves false when refused, after the part has shown why. */
export type SaveStep = () => Promise<boolean>;

/** Given to a Settings part in the wizard: registers its save and returns how to take it back.
 * A part given this shows no Save button of its own. */
export type SaveOnNext = (save: SaveStep) => () => void;

/** Registers `save` with `saveOnNext` for as long as the part is on screen. */
export function useSaveOnNext(saveOnNext: SaveOnNext | undefined, save: SaveStep): void {
  const latest = useRef(save);
  latest.current = save;
  useEffect(() => saveOnNext?.(() => latest.current()), [saveOnNext]);
}
