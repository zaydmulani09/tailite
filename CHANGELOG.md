# Changelog

## 0.1.0 (2026-09-28)

First release.

- `tailite watch`: follow a live WAL-mode database and print every committed row change
  with before/after values, as text or JSON Lines. `--snapshot` emits existing rows first.
- `tailite diff`: row-level difference between two database files, decoding only the pages
  that differ.
- Library: `Tail::open`, `Tail::poll`, `Tail::snapshot`, `tailite::diff`.
- Pinned read snapshot so checkpoints (including RESTART and TRUNCATE) never overwrite
  frames or page images tailite still needs.
- Incremental page index (owner, parent, overflow) maintained along changed paths, with
  orphan detection for pages SQLite frees without rewriting.
- Rowid tables, WITHOUT ROWID tables, AUTOINCREMENT, overflow chains and in-place blob
  overwrites, REAL affinity, ALTER TABLE ADD COLUMN defaults, generated columns, UTF-16.
