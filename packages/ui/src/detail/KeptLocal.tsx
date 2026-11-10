// A meeting kept on this computer (T12.13) on the Summary tab: what it means, and the one way
// back to the summary writer, after a confirm (local to remote only; nothing is sent until the
// user re-summarises).

import {
  Button,
  Dialog,
  DialogActions,
  DialogBody,
  DialogContent,
  DialogSurface,
  DialogTitle,
  MessageBar,
  MessageBarActions,
  MessageBarBody,
  MessageBarTitle,
} from "@fluentui/react-components";
import { useState } from "react";

import type { KeptLocalView } from "../ipc";

/** What the bar says about a kept meeting. */
export const KEPT_LOCAL_TEXT =
  "Its summary is written only by the summary model on this computer, never sent elsewhere.";

/** The confirm's text for `writer`. */
export function allowWriterText(writer: string): string {
  return (
    `When you summarise this meeting, its transcript may be sent to ${writer}. Nothing is sent ` +
    "until you choose Re-summarise. This meeting cannot be kept on this computer again."
  );
}

/** The Summary tab's bar for a meeting kept on this computer; nothing for any other. The
 * dialog stays mounted whatever the meeting, because "Allow" removes the bar it opened from. */
export function KeptLocal({
  keptLocal,
  onAllow,
}: {
  keptLocal: KeptLocalView | null;
  onAllow(): void;
}) {
  const [asking, setAsking] = useState<string | null>(null);
  const writer = keptLocal?.writer ?? null;
  return (
    <>
      {keptLocal !== null && (
        <MessageBar intent="info" layout="multiline">
          <MessageBarBody>
            <MessageBarTitle>Kept on this computer</MessageBarTitle>
            {KEPT_LOCAL_TEXT}
          </MessageBarBody>
          {writer !== null && (
            <MessageBarActions>
              <Button size="small" onClick={() => setAsking(writer)}>
                Let {writer} summarise it
              </Button>
            </MessageBarActions>
          )}
        </MessageBar>
      )}
      <Dialog
        modalType="alert"
        open={asking !== null}
        onOpenChange={(_, d) => {
          if (!d.open) setAsking(null);
        }}
      >
        <DialogSurface>
          <DialogBody>
            <DialogTitle>Let {asking} summarise this meeting?</DialogTitle>
            <DialogContent>{asking !== null && allowWriterText(asking)}</DialogContent>
            <DialogActions>
              <Button
                appearance="primary"
                onClick={() => {
                  setAsking(null);
                  onAllow();
                }}
              >
                Allow
              </Button>
              <Button onClick={() => setAsking(null)}>Cancel</Button>
            </DialogActions>
          </DialogBody>
        </DialogSurface>
      </Dialog>
    </>
  );
}
