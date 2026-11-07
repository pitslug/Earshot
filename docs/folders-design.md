# Folders in the side panel: design for T9.5

Status: for the owner to decide. Nothing is built.

## Recommendation

One level of named folders, each meeting in at most one, with "All meetings" as today's list. It takes one new synced table and one new column. Deleting a folder never deletes a meeting. Export stays as it is.

## What the user sees

- The side panel gets a **Folders** group above the dated list. Picking a folder shows only its meetings, still grouped by date. **All meetings** is the default and shows everything, so nobody who ignores folders sees a change beyond that one group.
- The meeting row menu (right-click, Shift+F10) gets **Move to folder**, listing the folders, **New folder** and **No folder**.
- A folder's own menu has **Rename** and **Delete**. Delete asks first, says how many meetings it holds, and says they stay in All meetings.
- Search stays across all meetings whichever folder is showing, and a result says nothing about folders in this version.
- With no folders yet, the group shows only a quiet **New folder**.

Drag and drop is left out of the first version: the menu is keyboard-accessible already and drag needs its own accessibility work.

## The choices

| Choice | Recommended | Alternative | Why |
|---|---|---|---|
| A meeting belongs to | One folder | Many labels | A folder answers "where is it". `Meeting.tags` already exists for labels, and is exported to Obsidian. |
| Levels | One | Nesting | Nesting adds cycles, moves and a tree UI, for a personal list of meetings. Adding levels later is possible; removing them is not. |
| Storage | `folder` table and `meeting.folder_id` | A reserved tag on `Meeting.tags` | A tag needs no migration, but a rename rewrites every meeting, and reserved tags would leak into Obsidian notes. |
| Export | Unchanged | Vault subfolders matching | `find_note` finds a moved note by its id, so this can come later without duplicating notes. |

## Data model

- **New table `folder`** with `{META}`, `name` and `sort_key`. It joins `SYNCED_TABLES`, and `Folder` is a `SyncableClass` row with repo contract cases, as the other synced rows are.
- **New nullable column `meeting.folder_id`**, by an appended migration. Existing meetings read as no folder, and nothing is backfilled.
- Names go through the existing name rule (trimmed, length limit, no control characters). Two folders may not share a name, ignoring case. Sync can still create a pair, and both then show.

## Rules that must hold

- **Deleting a folder** tombstones it, clears its name, and clears `folder_id` on its meetings, in one transaction. Meetings and their content are untouched.
- **Deleting a meeting** clears its `folder_id` with the rest of its row. A folder left empty stays until the user deletes it.
- **A folder name is content**: it can say who or what a meeting was about. It stays on the device as other content does, never goes in a log or notification, and is covered by the store's overwrite-on-delete behaviour.
- **A meeting pointing at a tombstoned or unknown folder reads as no folder.** This is what makes sync safe when one device deletes a folder while another moves a meeting into it.
- **Recording and processing are unaffected.** Moving a meeting needs no lease beyond a normal edit, and works during a final pass, because `folder_id` is the user's field. The final pass and summary re-read the row under the store lock and change only their own fields, as AGENTS requires.

## Sync (T11.1)

`folder` rows and `meeting.folder_id` follow last-writer-wins with tombstones like everything else. The move is one field on the meeting, so moving on one device and retitling on another merge only if T11.1 merges per field. If it merges per row, one of the two edits is lost, which is the same limit a title and tags already have. T11.1 should say which, and this design does not depend on the answer.

## Tasks if agreed

1. **Model and migration:** `Folder`, the repo trait and contract, the migration, `SYNCED_TABLES`. Reviewer required (data model, as T5.1 and T5.2).
2. **Runtime edits:** create, rename, delete, move, through `edit::write`, with `EditError` cases and fixed copy.
3. **UI:** the group, the filter, the menus and the delete dialog, in `AppShell` for the reasons AGENTS gives about dialogs that outlive their view. Screens checked light and dark.

## Decisions for the owner

1. One folder per meeting, or labels?
2. One level, or nesting?
3. Should "All meetings" stay the default view each time the app opens, or should it reopen the last folder? The recommendation is All meetings, so a new recording is always visible.
