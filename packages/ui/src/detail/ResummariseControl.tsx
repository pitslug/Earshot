// Re-summarise by hand (T12.63b, docs/summary-order.md 5): a split button whose primary action is
// "Use my order" (the order of summary writers, as a meeting's own summary runs it); its menu
// offers "Use my order", then every writer that can run on this computer now and each of the
// user's other computers that would write it now, each labelled as on the Summaries page. A
// writer chosen there writes it alone, with no next step. On a meeting kept on this computer the
// writers that would send it elsewhere are disabled with the reason until "Allow". The writers
// are read when the page opens; writers and computers are read again whenever the menu opens
// (the computers only then: asking for them reaches the relay, T14.17).

import {
  Caption1,
  Menu,
  MenuDivider,
  MenuItem,
  MenuList,
  MenuPopover,
  MenuTrigger,
  SplitButton,
  type MenuButtonProps,
} from "@fluentui/react-components";
import { useCallback, useEffect, useState } from "react";

import {
  ORDER_COPY,
  type ResummariseView,
  type StepChoice,
  type SummaryHelpersView,
  fillCopy,
  stepKey,
  useIpc,
} from "../ipc";

/** A computer's label in the menu: "On <name> (<writer>)" (the order's step label; an unknown
 * name is the table's `unnamed`, as Rust names it). */
export function helperLabel(name: string | null, writer: string | null): string {
  const device = name !== null && name.trim() !== "" ? name : ORDER_COPY.unnamed;
  return writer === null
    ? fillCopy(ORDER_COPY.steps.computer_no_writer, { device })
    : fillCopy(ORDER_COPY.steps.computer, { device, writer });
}

/** What the menu says while it reads the writers and computers. */
export const LOOKING = ORDER_COPY.resummarise.looking;

export interface ResummariseControlProps {
  /** The meeting. */
  meeting: string;
  /** The button's words ("Re-summarise" or "Summarise"). */
  label: string;
  disabled: boolean;
  /** Runs the order of summary writers ("Use my order"). */
  onOrder(): void;
  /** Runs one writer on this computer, alone. */
  onWriter(step: StepChoice): void;
  /** Asks one of the user's other computers; the page shows the outcome. */
  onAsk(device: string): void;
}

/** The header's Re-summarise: "Use my order", or one writer chosen from its menu. */
export function ResummariseControl({ meeting, label, disabled, onOrder, onWriter, onAsk }: ResummariseControlProps) {
  const ipc = useIpc();
  const [writers, setWriters] = useState<ResummariseView | null>(null);
  const [helpers, setHelpers] = useState<SummaryHelpersView | null>(null);
  const [loading, setLoading] = useState(false);
  const load = useCallback(async () => {
    setLoading(true);
    // Each list stands alone: one that cannot be read leaves the other and "Use my order".
    const [w, h] = await Promise.allSettled([ipc.resummariseWriters(meeting), ipc.summaryHelpers()]);
    if (w.status === "fulfilled") setWriters(w.value);
    if (h.status === "fulfilled") setHelpers(h.value);
    setLoading(false);
  }, [ipc, meeting]);
  // On opening the page only this computer's writers are read: the other computers come from
  // the relay, which is asked only when the menu opens (T14.17), never for every meeting shown.
  useEffect(() => {
    let live = true;
    ipc.resummariseWriters(meeting).then(
      (w) => {
        if (live) setWriters(w);
      },
      () => {},
    );
    return () => {
      live = false;
    };
  }, [ipc, meeting]);

  const computers = helpers?.helpers.filter((h) => h.available) ?? [];
  const items = writers?.writers ?? [];
  return (
    <Menu
      positioning="below-end"
      onOpenChange={(_, d) => {
        if (d.open) void load();
      }}
    >
      <MenuTrigger disableButtonEnhancement>
        {(triggerProps: MenuButtonProps) => (
          <SplitButton
            disabled={disabled}
            menuButton={{ ...triggerProps, "aria-label": fillCopy(ORDER_COPY.resummarise.choose, { label }) }}
            primaryActionButton={{ onClick: onOrder }}
          >
            {label}
          </SplitButton>
        )}
      </MenuTrigger>
      <MenuPopover>
        <MenuList>
          <MenuItem onClick={onOrder}>{ORDER_COPY.resummarise.order}</MenuItem>
          {(items.length > 0 || computers.length > 0) && <MenuDivider />}
          {items.map((w) => (
            <MenuItem
              key={stepKey(w.step)}
              disabled={w.disabled !== null}
              secondaryContent={w.disabled !== null ? <Caption1>{w.disabled}</Caption1> : undefined}
              onClick={() => onWriter(w.step)}
            >
              {w.label}
            </MenuItem>
          ))}
          {computers.map((h) => (
            <MenuItem key={h.id} onClick={() => onAsk(h.id)}>
              {helperLabel(h.name, h.writer)}
            </MenuItem>
          ))}
          {loading && items.length === 0 && computers.length === 0 && <MenuItem disabled>{LOOKING}</MenuItem>}
        </MenuList>
      </MenuPopover>
    </Menu>
  );
}
