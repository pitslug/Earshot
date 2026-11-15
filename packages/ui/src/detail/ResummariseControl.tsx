// Re-summarise by hand with summary offload (T11.3db, docs/sync.md section 10): "Here" runs this
// computer's writer (the ordinary job), "On <device>" asks one of the user's other computers that
// writes summaries for the others. The other computers are read when the page opens and again
// whenever the menu opens; without sync (and no computer chosen) the plain button stays.

import {
  Button,
  Menu,
  MenuItem,
  MenuList,
  MenuPopover,
  MenuTrigger,
  SplitButton,
  type MenuButtonProps,
} from "@fluentui/react-components";
import { useCallback, useEffect, useState } from "react";

import { type SummaryHelpersView, useIpc } from "../ipc";

/** A computer's label in the menu: "On <name> (<writer>)". */
export function helperLabel(name: string | null, shortId: string, writer: string | null): string {
  const who = name ?? `Computer ${shortId}`;
  return writer === null ? `On ${who}` : `On ${who} (${writer})`;
}

/** What the menu says while it reads the other computers. */
export const LOOKING = "Looking for your other computers";
/** What the menu says when no other computer would write it now. */
export const NONE_AVAILABLE = "No other computer can write it now";

export interface ResummariseControlProps {
  /** The button's words ("Re-summarise" or "Summarise"). */
  label: string;
  disabled: boolean;
  /** Runs this computer's writer. */
  onHere(): void;
  /** Asks `device`; the page shows the outcome. */
  onAsk(device: string): void;
}

/** The header's Re-summarise: a plain button, or a split button with "Here" and the other
 * computers once sync can ask one. */
export function ResummariseControl({ label, disabled, onHere, onAsk }: ResummariseControlProps) {
  const ipc = useIpc();
  const [helpers, setHelpers] = useState<SummaryHelpersView | null>(null);
  const [loading, setLoading] = useState(false);
  const load = useCallback(async () => {
    setLoading(true);
    try {
      setHelpers(await ipc.summaryHelpers());
    } catch {
      // Not read: the menu offers only "Here".
    } finally {
      setLoading(false);
    }
  }, [ipc]);
  useEffect(() => {
    void load();
  }, [load]);

  const offered = helpers !== null && (helpers.sync_ready || helpers.chosen !== null);
  if (!offered) {
    return (
      <Button disabled={disabled} onClick={onHere}>
        {label}
      </Button>
    );
  }
  const available = helpers.helpers.filter((h) => h.available);
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
            menuButton={{ ...triggerProps, "aria-label": `${label}: choose where` }}
            primaryActionButton={{ onClick: onHere }}
          >
            {label}
          </SplitButton>
        )}
      </MenuTrigger>
      <MenuPopover>
        <MenuList>
          <MenuItem onClick={onHere}>Here</MenuItem>
          {available.map((h) => (
            <MenuItem key={h.id} onClick={() => onAsk(h.id)}>
              {helperLabel(h.name, h.short_id, h.writer)}
            </MenuItem>
          ))}
          {available.length === 0 && <MenuItem disabled>{loading ? LOOKING : NONE_AVAILABLE}</MenuItem>}
        </MenuList>
      </MenuPopover>
    </Menu>
  );
}
