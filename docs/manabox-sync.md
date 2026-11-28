# ManaBox sync

Settings → **Sync from ManaBox** brings the Scrybox collection, and optionally
Scrybox lists, in line with a ManaBox CSV export. It is safe to run as often as
you like; running the same file twice changes nothing.

## Steps

1. In ManaBox: Collection → ⋯ → Export → CSV. Save the file.
2. In Scrybox: Settings → Sync from ManaBox → **Choose ManaBox CSV**.
3. Read the preview (collection before → after, adds, removes, dates, lists).
   **Review changes** downloads the full list as markdown.
4. **Apply sync**. Past syncs are listed underneath with a downloadable record.

## Rules

- **Owned** = rows whose Binder Type is `binder` or `deck`. `list` rows are
  never owned; with "Also sync ManaBox lists" on, each ManaBox list becomes a
  Scrybox list of the same name.
- **Identity** = exact printing (`mtg-<Scryfall ID>`) + printing (normal /
  foil; etched counts as foil) + condition + language. A row is rejected
  (listed under *Rejected*) when its set code, collector number or name does
  not match the card Scryfall returns for its ID, so a card cannot land as the
  wrong printing.
- **Only ManaBox copies are removed.** Rows written by sync are tagged
  `collection.source = 'manabox'`. Anything added in Scrybox itself (Scan
  Cards, search, Secret Lair, orders) has no tag and is never removed. Those
  copies still count as owned, so sync never adds a card a second time.
- **Dates**: ManaBox's *Added* date becomes Scrybox's date added, for every
  ManaBox copy. Scrybox-added copies keep their own dates.
- **Condition changes**: if you change a card's condition in ManaBox, the next
  sync removes the old ManaBox copy and adds the new one.
- **Lists**: a Scrybox list created by sync is updated to match ManaBox. A list
  you made in Scrybox with the same name is skipped, never overwritten. A
  ManaBox list you deleted in Scrybox is created again on the next sync while
  it still exists in ManaBox.
- **Stale preview protection**: apply re-plans against the collection as it
  is now and refuses (409) if the card count changed since the preview.
- The whole apply is one database transaction: it all lands or none of it does.

## API

- `POST /api/manabox-sync/preview {csv, includeLists}` → `{summary, report}`
- `POST /api/manabox-sync/apply {csv, includeLists, expectBefore}` → `{summary, report}`
- `GET /api/manabox-sync/runs`, `GET /api/manabox-sync/runs/:id/report`

Planning is pure and unit-tested: `server/crates/scrybox-server/src/manabox.rs`
(Rust port of the planning + routes) and `server/crates/scrybox-server/tests/manabox_sync.rs`.
