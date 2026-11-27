// Damaged items (T14.65, docs/sync-spec.md 17): the one banner every view shows while the last
// scan found something damaged, and its Details dialog with Repair from sync. Counts only:
// nothing here names a damaged row. Every word comes from `ipc/damagedCopy.ts`.

import {
  Body1,
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
  Spinner,
  makeStyles,
  tokens,
  useRestoreFocusTarget,
} from "@fluentui/react-components";
import { useEffect, useLayoutEffect, useRef, useState } from "react";

import { errorText, useIpc } from "../ipc";
import type { DamageCountsView, DamagedView, KindsOff, RepairView } from "../ipc/bindings";
import {
  DAMAGED_COPY,
  kindLines,
  kindOffLines,
  leftOutsideSwitchedOff,
  readingText,
  totalDamaged,
} from "../ipc/damagedCopy";

const useStyles = makeStyles({
  content: { display: "flex", flexDirection: "column", rowGap: tokens.spacingVerticalM },
  list: { margin: 0, paddingInlineStart: tokens.spacingHorizontalXL },
});

/** Whether the banner shows: the stored record at mount, then every `DamageEvent`. Subscribes
 * before it reads, so an event sent meanwhile is not lost; the read never overrides one. */
export function useDamageBanner(): [boolean, (shown: boolean) => void] {
  const ipc = useIpc();
  const [shown, setShown] = useState(false);
  useEffect(() => {
    let live = true;
    let heard = false;
    let stop: (() => void) | null = null;
    void ipc
      .onDamageEvent((e) => {
        if (!live) return;
        heard = true;
        setShown(e.shown);
      })
      .then(
        (s) => {
          if (!live) s();
          else stop = s;
          return ipc.damageBanner();
        },
        () => ipc.damageBanner(),
      )
      .then(
        (on) => {
          if (live && !heard) setShown(on);
        },
        () => {},
      );
    return () => {
      live = false;
      stop?.();
    };
  }, [ipc]);
  return [shown, setShown];
}

export interface DamageBannerProps {
  onDetails(): void;
  onDismiss(): void;
  /** Called as the banner goes while focus is inside it (Dismiss pressed): the caller moves
   * focus, as for the window's error bars. */
  onGoneWithFocus(): void;
}

/** "Some items on this computer are damaged.", with Details and Dismiss. */
export function DamageBanner({ onDetails, onDismiss, onGoneWithFocus }: DamageBannerProps) {
  const bar = useRef<HTMLDivElement | null>(null);
  const gone = useRef(onGoneWithFocus);
  gone.current = onGoneWithFocus;
  const restoreFocus = useRestoreFocusTarget();
  useLayoutEffect(() => {
    const el = bar.current;
    return () => {
      if (el !== null && el.contains(el.ownerDocument.activeElement)) gone.current();
    };
  }, []);
  return (
    <MessageBar ref={bar} intent="warning" role="status">
      <MessageBarBody>{DAMAGED_COPY.banner}</MessageBarBody>
      <MessageBarActions>
        <Button size="small" onClick={onDetails} {...restoreFocus}>
          {DAMAGED_COPY.details_button}
        </Button>
        <Button size="small" appearance="transparent" onClick={onDismiss}>
          {DAMAGED_COPY.dismiss}
        </Button>
      </MessageBarActions>
    </MessageBar>
  );
}

/** The kinds' lines, then the sentence for each switched-off kind among them. */
function KindList({ counts, off }: { counts: DamageCountsView; off: KindsOff }) {
  const styles = useStyles();
  const lines = kindLines(counts);
  return (
    <>
      {lines.length > 0 && (
        <ul className={styles.list} aria-label={DAMAGED_COPY.details_title}>
          {lines.map((l) => (
            <li key={l.kind}>
              <Body1>{l.text}</Body1>
            </li>
          ))}
        </ul>
      )}
      {kindOffLines(counts, off).map((text) => (
        <Body1 key={text}>{text}</Body1>
      ))}
    </>
  );
}

/** What a finished Repair left (17.4, Done). */
function RepairResult({ left, off }: { left: DamageCountsView; off: KindsOff }) {
  if (totalDamaged(left) === 0) return <Body1>{DAMAGED_COPY.result_none}</Body1>;
  return (
    <>
      <Body1>{DAMAGED_COPY.result_some}</Body1>
      <KindList counts={left} off={off} />
      {leftOutsideSwitchedOff(left, off) && <Body1>{DAMAGED_COPY.nothing_healable}</Body1>}
    </>
  );
}

export interface DamagedDetailsProps {
  open: boolean;
  onClose(): void;
}

/** Details: a scan's counts by kind, the switched-off kinds and Repair from sync. Follows
 * `DamageEvent`s while open, so a Repair's progress, stop and result show as they come. */
export function DamagedDetails({ open, onClose }: DamagedDetailsProps) {
  const ipc = useIpc();
  const styles = useStyles();
  const [view, setView] = useState<DamagedView | null>(null);
  const [repair, setRepair] = useState<RepairView>({ state: "idle" });
  const [loadError, setLoadError] = useState<string | null>(null);
  const [repairError, setRepairError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);

  useEffect(() => {
    if (!open) return;
    let live = true;
    let heard = false;
    let stop: (() => void) | null = null;
    setView(null);
    setLoadError(null);
    setRepairError(null);
    void ipc
      .onDamageEvent((e) => {
        if (!live) return;
        heard = true;
        setRepair(e.repair);
      })
      .then(
        (s) => {
          if (!live) s();
          else stop = s;
        },
        () => {},
      )
      .then(() => ipc.damagedItems())
      .then(
        (v) => {
          if (!live) return;
          setView(v);
          // An event heard since the request is newer than the answer's Repair.
          if (!heard) setRepair(v.repair);
        },
        (e: unknown) => {
          if (live) setLoadError(errorText(e));
        },
      );
    return () => {
      live = false;
      stop?.();
    };
  }, [ipc, open]);

  const startRepair = () => {
    setStarting(true);
    setRepairError(null);
    ipc.repairFromSync().then(
      () => setStarting(false),
      (e: unknown) => {
        setStarting(false);
        setRepairError(errorText(e));
      },
    );
  };

  const repairArea = (v: DamagedView) => {
    const left = repair.state === "done" ? repair.left : v.counts;
    if (!v.sync_repairs) {
      return totalDamaged(left) > 0 ? <Body1>{DAMAGED_COPY.no_repair_sync_off}</Body1> : null;
    }
    if (repair.state === "reading") return <Body1 role="status">{readingText(repair.percent)}</Body1>;
    if (repair.state === "stopped") return <Body1 role="status">{DAMAGED_COPY.repair_stopped}</Body1>;
    if (totalDamaged(left) === 0) return null;
    return (
      <>
        <Body1>{DAMAGED_COPY.repair_intro}</Body1>
        <div>
          <Button appearance="primary" disabled={starting} onClick={startRepair}>
            {DAMAGED_COPY.repair_button}
          </Button>
        </div>
      </>
    );
  };

  return (
    <Dialog open={open} onOpenChange={(_, d) => (d.open ? undefined : onClose())}>
      <DialogSurface>
        <DialogBody>
          <DialogTitle>{DAMAGED_COPY.details_title}</DialogTitle>
          <DialogContent className={styles.content}>
            {loadError !== null && (
              <MessageBar intent="error">
                <MessageBarBody>{loadError}</MessageBarBody>
              </MessageBar>
            )}
            {view === null && loadError === null && <Spinner size="tiny" />}
            {view !== null && (
              <>
                {repair.state === "done" ? (
                  <RepairResult left={repair.left} off={view.kinds_off} />
                ) : (
                  <KindList counts={view.counts} off={view.kinds_off} />
                )}
                {repairArea(view)}
                {repairError !== null && (
                  <MessageBar intent="error">
                    <MessageBarBody>{repairError}</MessageBarBody>
                  </MessageBar>
                )}
              </>
            )}
          </DialogContent>
          <DialogActions>
            <Button appearance="secondary" onClick={onClose}>
              Close
            </Button>
          </DialogActions>
        </DialogBody>
      </DialogSurface>
    </Dialog>
  );
}
