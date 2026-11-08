# Folders in the side panel: design for T9.5

Status: decided by the owner (2026-11-08), reviewed (spec review CLEAR with advisories, folded in). Building as T9.5a to T9.5c.

## The design

One level of named folders, each meeting in at most one, with "All meetings" as today's list. It takes one new synced table and one new column. Deleting a folder asks what happens to its meetings, and keeping them is the default. Export stays as it is.

## What the user sees

- The side panel gets a **Folders** group above the dated list. Picking a folder shows only its meetings, still grouped by date. **All meetings** is the default and shows everything, so nobody who ignores folders sees a change beyond that one group.
- The meeting row menu (right-click, Shift+F10) gets **Move to folder**, listing the folders, **New folder** and **No folder**.
- A folder's own menu has **Rename** and **Delete**. Delete asks first, says how many meetings the folder holds, and offers two choices: keep the meetings, which then have no folder and still show in All meetings (the default), or delete them too. There is no separate root folder: All meetings is where a meeting with no folder shows.
- Search stays across all meetings whichever folder is showing, and a result says nothing about folders in this version.
- With no folders yet, the group shows only a quiet **New folder**.

Drag and drop is left out of the first version: the menu is keyboard-accessible already and drag needs its own accessibility work.

## The choices

| Choice | Chosen | Alternative | Why |
|---|---|---|---|
| A meeting belongs to | One folder | Many labels | A folder answers "where is it". `Meeting.tags` already exists for labels, and is exported to Obsidian. |
| Levels | One | Nesting | Nesting adds cycles, moves and a tree UI, for a personal list of meetings. Adding levels later is possible; removing them is not. |
| Storage | `folder` table and `meeting.folder_id` | A reserved tag on `Meeting.tags` | A tag needs no migration, but a rename rewrites every meeting, and reserved tags would leak into Obsidian notes. |
| Export | Unchanged | Vault subfolders matching | `find_note` finds a moved note by its id, so this can come later without duplicating notes. |

## Data model

- **New table `folder`** with `{META}` and `name`. It joins `SYNCED_TABLES`, and `Folder` is a `SyncableClass` row with repo contract cases, as the other synced rows are. No `sort_key`: folders are listed by name, folded per character, because nothing reorders them and a synced order field would conflict.
- **New nullable column `meeting.folder_id BLOB`**, by an appended migration, with no foreign key: sync may deliver a meeting before its folder. Existing meetings read as no folder, and nothing is backfilled. `Meeting.folder_id` is `Option<EntityId>` with `#[serde(default)]`, so older JSON (the `accept m5` inputs) still reads. Core `delete_meeting` clears it, and the repo contract asserts that.
- Names go through the existing name rule (trimmed, length limit, no control characters). Two live folders may not share a name, ignoring case, folded per character as search folds. The rule is a runtime check over live rows inside the edit transaction, never a SQLite index: tombstones clear every name to the same value, and sync may deliver a pair, which then both show. The refusal is an `EditError` that never quotes the name.
- **Downgrade:** this is the first migration an installed database runs, so an older Earshot opening it now gets `SchemaTooNew`. The app shows a start-up screen saying the data was saved by a newer Earshot and to install it, rather than failing every command.

## Rules that must hold

- **Deleting a folder and keeping its meetings** (the default) tombstones it, clears its name, and clears `folder_id` on its meetings, in one transaction. Meetings and their content are untouched.
- **Deleting a folder and its meetings** deletes each meeting through `Services::delete_meeting`, then the folder as above. People and voiceprints stay, as with any meeting deletion. A meeting is filed only after it was recorded, so a recording meeting is never in a folder (the owner, 2026-11-08); the move into a folder is refused while the meeting records. A filed meeting can still be busy (finishing, or a re-transcribe or re-summarise job), and `delete_meeting` then refuses it with `MeetingBusy`: that is one more failed deletion under the next rule, not a separate case. If any deletion fails, the folder stays with the meetings it still holds, and the app says how many were deleted and how many are left (the owner, 2026-11-08).
- **Deleting a meeting** clears its `folder_id` with the rest of its row. A folder left empty stays until the user deletes it.
- **A folder name is content**: it can say who or what a meeting was about. It stays on the device as other content does, never goes in a log or notification, and is covered by the store's overwrite-on-delete behaviour.
- **The delete loop holds no lock:** each `Services::delete_meeting` takes the store and activity locks itself. The closing transaction (tombstone the folder, clear its name, clear `folder_id`) selects the folder's meetings inside that transaction, not from the list read before the loop, and is followed by `Store::flush_erasures`.
- **A meeting pointing at a tombstoned or unknown folder reads as no folder.** This is what makes sync safe when one device deletes a folder while another moves a meeting into it. The rule exists once, in `earshot_runtime::folders`, which owns every folder read and edit; the IPC sends the resolved folder, never a raw `folder_id`.
- **Recording and processing are unaffected.** Moving a meeting needs no lease beyond a normal edit, and works during a final pass, because `folder_id` is the user's field. The final pass and summary re-read the row under the store lock and change only their own fields, as AGENTS requires.

## Tests to write

- A committed version-1 database migrates: meetings read as no folder, `user_version` is 2, and reopening changes nothing.
- A folder delete with its meetings where one meeting is busy (held lease) leaves the folder holding that meeting and reports the counts.
- A move during `store_summary` keeps `folder_id`.
- A move is refused while the meeting records.
- Deleting two folders in a row works (the cleared names do not collide).

## Sync (T11.1)

`folder` rows and `meeting.folder_id` follow last-writer-wins with tombstones like everything else. The move is one field on the meeting, so moving on one device and retitling on another merge only if T11.1 merges per field. If it merges per row, one of the two edits is lost, which is the same limit a title and tags already have. T11.1 should say which, and this design does not depend on the answer.

## Tasks (PLAN.md T9.5a to T9.5c)

1. **T9.5a Model and migration:** `Folder`, the repo trait and contract, the migration, `SYNCED_TABLES`, `meeting.folder_id`. Reviewer required (data model, as T5.1 and T5.2).
2. **T9.5b Runtime:** `earshot_runtime::folders`: create, rename, move, delete with either choice, resolve, through `edit::write` and `Services::delete_meeting`, with `EditError` cases and fixed copy.
3. **T9.5c UI:** the IPC commands, the group, the filter, the menus and the delete dialog, in `AppShell` for the reasons AGENTS gives about dialogs that outlive their view, plus the newer-database start-up screen. Screens checked light and dark.

## The owner's decisions (2026-11-08)

1. One folder per meeting.
2. One level. Nesting can be added later if it is needed.
3. "All meetings" is the view each time the app opens.
4. Deleting a folder asks whether to delete its meetings or move them out. Moving them is the default.
