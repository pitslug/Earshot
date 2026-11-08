// The Folders group of the side panel (T9.5c, docs/folders-design.md): "All meetings", then
// the folders by name, each with a menu (right-click, the Menu key or Shift+F10) to rename or
// delete it; with no folders only a quiet "New folder". The folder chosen filters the dated
// list below it. Every change goes through `AppShell`, which owns the delete dialog.

import {
  Button,
  Caption1Strong,
  Field,
  Input,
  Menu,
  MenuItem,
  MenuList,
  MenuPopover,
  MenuTrigger,
  makeStyles,
  mergeClasses,
  tokens,
  useRestoreFocusTarget,
} from "@fluentui/react-components";
import { useId, useState } from "react";

import type { FolderListItem } from "../ipc";
import { openMenuFromKeyboard } from "./menuKeys";

const useStyles = makeStyles({
  group: {
    marginBottom: tokens.spacingVerticalM,
  },
  heading: {
    display: "block",
    margin: 0,
    paddingTop: tokens.spacingVerticalXS,
    paddingBottom: tokens.spacingVerticalXS,
    paddingLeft: tokens.spacingHorizontalS,
    color: tokens.colorNeutralForeground3,
  },
  list: {
    listStyleType: "none",
    margin: 0,
    padding: 0,
  },
  row: {
    width: "100%",
    justifyContent: "flex-start",
    textAlign: "left",
    paddingLeft: tokens.spacingHorizontalS,
    paddingRight: tokens.spacingHorizontalS,
    fontWeight: tokens.fontWeightRegular,
  },
  rowSelected: {
    backgroundColor: tokens.colorSubtleBackgroundSelected,
  },
  name: {
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
  },
  newFolder: {
    justifyContent: "flex-start",
    color: tokens.colorNeutralForeground3,
  },
  field: {
    display: "flex",
    flexDirection: "column",
    gap: tokens.spacingVerticalXS,
    paddingLeft: tokens.spacingHorizontalS,
    paddingRight: tokens.spacingHorizontalS,
    paddingTop: tokens.spacingVerticalXS,
  },
  fieldActions: {
    display: "flex",
    gap: tokens.spacingHorizontalXS,
  },
});

/** The inline name input that is open, if any. */
export type FolderEdit = { kind: "new"; moveMeeting: string | null } | { kind: "rename"; folder: FolderListItem };

export interface FolderGroupProps {
  folders: FolderListItem[];
  /** The folder shown, `null` for "All meetings". */
  selected: string | null;
  onSelect: (folder: string | null) => void;
  edit: FolderEdit | null;
  onNewFolder: () => void;
  onRename: (folder: FolderListItem) => void;
  onDelete: (folder: FolderListItem) => void;
  /** Saves the open name input; resolves with the refusal's copy, or `null` once it is done. */
  onSubmitName: (name: string) => Promise<string | null>;
  onCancelEdit: () => void;
  /** Where focus goes back to after a dialog or an input closes. */
  buttonRef: (key: string, el: HTMLButtonElement | null) => void;
}

/** The key of "All meetings" and "New folder" in `buttonRef`. */
export const ALL_MEETINGS_KEY = "all";
export const NEW_FOLDER_KEY = "new";

function FolderIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <path d="M2 4.5a1 1 0 0 1 1-1h3l1.5 1.5H13a1 1 0 0 1 1 1V12a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1z" />
    </svg>
  );
}

function AllIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <path d="M3 4h10M3 8h10M3 12h10" />
    </svg>
  );
}

function PlusIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">
      <path d="M8 3.5v9M3.5 8h9" />
    </svg>
  );
}

/** The Folders group. */
export function FolderGroup({
  folders,
  selected,
  onSelect,
  edit,
  onNewFolder,
  onRename,
  onDelete,
  onSubmitName,
  onCancelEdit,
  buttonRef,
}: FolderGroupProps) {
  const styles = useStyles();
  const headingId = useId();
  // The delete dialog opens with no trigger of its own: on Cancel it hands focus back here.
  const restoreFocus = useRestoreFocusTarget();
  const newField =
    edit?.kind === "new" ? (
      <FolderNameField
        label={edit.moveMeeting === null ? "New folder name" : "New folder name (the meeting moves into it)"}
        initial=""
        onSubmit={onSubmitName}
        onCancel={onCancelEdit}
      />
    ) : (
      <Button
        ref={(el: HTMLButtonElement | null) => buttonRef(NEW_FOLDER_KEY, el)}
        appearance="transparent"
        size="small"
        icon={<PlusIcon />}
        className={styles.newFolder}
        onClick={onNewFolder}
      >
        New folder
      </Button>
    );

  // Nothing filed yet: only the quiet way to start.
  if (folders.length === 0) return <div className={styles.group}>{newField}</div>;

  return (
    <div className={styles.group} role="group" aria-labelledby={headingId}>
      <Caption1Strong as="h2" id={headingId} className={styles.heading}>
        Folders
      </Caption1Strong>
      <ul className={styles.list} aria-label="Folders">
        <li>
          <Button
            ref={(el: HTMLButtonElement | null) => buttonRef(ALL_MEETINGS_KEY, el)}
            appearance="subtle"
            icon={<AllIcon />}
            className={mergeClasses(styles.row, selected === null && styles.rowSelected)}
            aria-current={selected === null ? "true" : undefined}
            onClick={() => onSelect(null)}
          >
            All meetings
          </Button>
        </li>
        {folders.map((f) =>
          edit?.kind === "rename" && edit.folder.id === f.id ? (
            <li key={f.id}>
              <FolderNameField label="Folder name" initial={f.name} onSubmit={onSubmitName} onCancel={onCancelEdit} />
            </li>
          ) : (
            <li key={f.id}>
              <Menu openOnContext>
                <MenuTrigger disableButtonEnhancement>
                  <Button
                    {...restoreFocus}
                    ref={(el: HTMLButtonElement | null) => buttonRef(f.id, el)}
                    appearance="subtle"
                    icon={<FolderIcon />}
                    className={mergeClasses(styles.row, selected === f.id && styles.rowSelected)}
                    aria-current={selected === f.id ? "true" : undefined}
                    onClick={() => onSelect(f.id)}
                    onKeyDown={openMenuFromKeyboard}
                  >
                    <span className={styles.name}>{f.name}</span>
                  </Button>
                </MenuTrigger>
                <MenuPopover>
                  <MenuList>
                    <MenuItem onClick={() => onRename(f)}>Rename</MenuItem>
                    <MenuItem onClick={() => onDelete(f)}>Delete...</MenuItem>
                  </MenuList>
                </MenuPopover>
              </Menu>
            </li>
          ),
        )}
      </ul>
      {newField}
    </div>
  );
}

/** The inline name input: Enter saves, Escape cancels; a refusal shows under it. */
function FolderNameField({
  label,
  initial,
  onSubmit,
  onCancel,
}: {
  label: string;
  initial: string;
  onSubmit: (name: string) => Promise<string | null>;
  onCancel: () => void;
}) {
  const styles = useStyles();
  const [value, setValue] = useState(initial);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const save = async () => {
    if (busy) return;
    setBusy(true);
    const refused = await onSubmit(value);
    // Done: the input is gone.
    if (refused === null) return;
    setError(refused);
    setBusy(false);
  };
  return (
    <div className={styles.field}>
      <Field
        label={label}
        size="small"
        validationState={error !== null ? "error" : "none"}
        validationMessage={error ?? undefined}
      >
        <Input
          size="small"
          autoFocus
          value={value}
          onChange={(_, d) => {
            setValue(d.value);
            setError(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              void save();
            } else if (e.key === "Escape") {
              e.preventDefault();
              e.stopPropagation();
              onCancel();
            }
          }}
        />
      </Field>
      <div className={styles.fieldActions}>
        <Button size="small" appearance="primary" disabledFocusable={busy} onClick={() => void save()}>
          Save
        </Button>
        <Button size="small" disabledFocusable={busy} onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </div>
  );
}
