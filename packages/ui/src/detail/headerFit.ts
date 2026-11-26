// Whether the meeting header's buttons fit on one row with their labels (T14.55). When they do
// not, Speakers, Export to Obsidian and Copy summary show only their icons.
//
// The space the labelled buttons need is measured only while they are labelled, and kept until
// the set of buttons changes, so drawing the icons never changes the answer (no back and forth
// between the two forms). jsdom does no layout: tests set `headerFit.measure`.

import { type RefObject, useLayoutEffect, useRef, useState } from "react";

/** What the button row needs with its labels, and what its container offers, in pixels. */
export interface RowSpace {
  need: number;
  available: number;
}

/** The row's buttons side by side with their gaps, against the width of the row's container. */
export function measureRow(row: HTMLElement): RowSpace {
  const children = Array.from(row.children) as HTMLElement[];
  const gap = parseFloat(getComputedStyle(row).columnGap);
  const gaps = Number.isNaN(gap) ? 0 : gap * Math.max(0, children.length - 1);
  const need = children.reduce((sum, c) => sum + c.getBoundingClientRect().width, 0) + gaps;
  const available = row.parentElement?.clientWidth ?? row.clientWidth;
  return { need, available };
}

/** The measurement the hook uses: the DOM's, or a test's. */
export const headerFit: { measure: (row: HTMLElement) => RowSpace } = { measure: measureRow };

/** Whether labels fit: the space they need is within what is available. */
export function labelsFit(space: RowSpace): boolean {
  return space.need <= space.available;
}

/**
 * True while the buttons in `row` fit on one row with their labels. `buttons` names the set of
 * buttons and their labels: when it changes, the labelled row is drawn and measured again
 * before the page is painted.
 */
export function useLabelsFit(row: RefObject<HTMLElement | null>, buttons: string): boolean {
  const [fit, setFit] = useState(true);
  // The labelled row's need, measured for `measuredFor`; null until measured.
  const need = useRef<number | null>(null);
  const measuredFor = useRef<string | null>(null);

  useLayoutEffect(() => {
    const el = row.current;
    if (el === null) return;
    if (measuredFor.current !== buttons) {
      need.current = null;
      measuredFor.current = buttons;
    }
    if (need.current !== null) return;
    // Measure only the labelled row: draw it first if the icons are showing.
    if (!fit) {
      setFit(true);
      return;
    }
    const space = headerFit.measure(el);
    need.current = space.need;
    setFit(labelsFit(space));
  }, [row, buttons, fit]);

  useLayoutEffect(() => {
    const el = row.current;
    if (el === null) return;
    const container = el.parentElement ?? el;
    const watch = new ResizeObserver(() => {
      if (need.current === null) return;
      const { available } = headerFit.measure(el);
      setFit(labelsFit({ need: need.current, available }));
    });
    watch.observe(container);
    return () => watch.disconnect();
  }, [row]);

  return fit;
}
