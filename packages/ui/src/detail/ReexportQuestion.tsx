// The question before exporting a meeting that was exported before (T14.63): exporting again
// replaces the whole Obsidian note. No title, by the owner's wording: the sentence is the
// question, and it names the dialog for a screen reader.

import {
  Button,
  Dialog,
  DialogActions,
  DialogBody,
  DialogContent,
  DialogSurface,
  useId,
} from "@fluentui/react-components";

import { REEXPORT_COPY } from "./reexportCopy";

/** Open while `open`; "Export again" calls `onExport`, "Cancel" (or Esc) only `onClose`. */
export function ReexportQuestion({
  open,
  text = REEXPORT_COPY.single,
  onExport,
  onClose,
}: {
  open: boolean;
  /** The sentence; the single meeting's by default (a batch passes `reexportBatchQuestion`). */
  text?: string;
  onExport(): void;
  onClose(): void;
}) {
  const textId = useId("reexport-text");
  return (
    <Dialog
      modalType="alert"
      open={open}
      onOpenChange={(_, d) => {
        if (!d.open) onClose();
      }}
    >
      <DialogSurface aria-labelledby={textId}>
        <DialogBody>
          <DialogContent id={textId}>{text}</DialogContent>
          <DialogActions>
            <Button
              appearance="primary"
              onClick={() => {
                onClose();
                onExport();
              }}
            >
              {REEXPORT_COPY.confirm}
            </Button>
            <Button onClick={onClose}>{REEXPORT_COPY.cancel}</Button>
          </DialogActions>
        </DialogBody>
      </DialogSurface>
    </Dialog>
  );
}
