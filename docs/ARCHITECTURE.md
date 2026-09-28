# How tailite works

tailite answers one question: *which rows did this committed transaction change, and
what did they look like before and after?* It answers it from outside the writing
process, using only the files SQLite already writes. This document walks the pipeline
from bytes on disk to row events, and explains the one piece of coordination with
SQLite that makes it safe.

```
 app.db-wal ──► wal.rs ──► commits (page -> frame offset)
                                │
 app.db ─────► format.rs ◄──────┤  page images: main file + WAL overlay
                                │
               tracker.rs ──────┘  page -> table, page -> parent, overflow -> row
                   │
                   ▼
               row diff per table ──► Change { table, op, rowid, before, after }
```

| module      | job |
|-------------|-----|
| `wal.rs`    | Parse the log header and frames, verify salts and cumulative checksums, group frames into committed transactions. |
| `format.rs` | Varints, b-tree page headers, cells, overflow chains, records. `Pages` is one consistent database state. |
| `schema.rs` | Read `sqlite_schema`; parse enough of `CREATE TABLE` to name columns, find the rowid alias, the primary key of WITHOUT ROWID tables, REAL affinity, and ADD COLUMN defaults. |
| `tracker.rs`| Map changed pages to tables, find pages that left the tree, decode rows, diff them. |
| `tail.rs`   | The live loop: pinning, polling, applying commits in order. |

## 1. Reading committed transactions from the WAL

A WAL file is a 32-byte header followed by frames, each a 24-byte frame header and a
page image. A frame belongs to the current log generation if its two salts equal the
header's. Every frame carries a checksum that chains from the previous frame (or the
header), so a torn or stale frame breaks the chain. A frame with a non-zero "database
size" field is a commit frame.

tailite applies the same rule SQLite's own recovery uses: a transaction is visible
exactly when its frames, up to and including a commit frame, all have matching salts
and a valid checksum chain. Frames of a transaction still being written, or of one that
was rolled back after its pages spilled into the log, are never surfaced. The cursor
remembers the frame index and the running checksum after the last commit it consumed,
so each poll only reads new frames.

When the header's salts change, the writer has started a new generation (after a
completed checkpoint). The cursor restarts at frame 0, and the page overlay is cleared
because the main file now holds everything the old generation contained. Section 3
explains why no unread frame can be lost at that moment.

## 2. Page images and database states

`Pages` resolves a page number to its image in one state: look the page up in the WAL
layers (newest first) and read that frame, otherwise read the main file. The state
*before* a transaction is `[overlay]`; the state *after* it is `[overlay, commit]`,
where the overlay maps every page written in this log generation to its latest
committed frame. After a transaction is applied its pages are merged into the overlay.
Only offsets are kept in memory, never page images.

## 3. The pin: why checkpoints cannot overtake tailite

The danger in reading someone else's WAL is the checkpoint. It copies frames into the
main file, and once the log is fully copied the next writer resets it, overwriting old
frames. Either can destroy what tailite still needs: the new frames of transactions it
has not read, or the old page images of pages that were never in the log (the "before"
side lives in the main file until a checkpoint overwrites it).

SQLite already protects readers from exactly this. A checkpoint never copies a frame
newer than the snapshot of an open read transaction, and the log is not reset while a
reader holds a snapshot that uses it. So tailite holds a read transaction (the *pin*)
at a point it has already processed:

1. Open a new read transaction. Its snapshot is at most what is in the log right now.
2. Read the log and apply every committed transaction found, which is at least
   everything the new snapshot can see.
3. Release the previous pin.

Invariant: at every moment some pin is held at or before the last processed commit.
Therefore the main file never contains a page newer than what tailite has processed,
and the log never resets while it holds unprocessed frames. Two connections alternate
as pins, so no connection is opened per poll.

The cost to the writer is that checkpoints can only advance to where tailite has read,
at most one poll interval behind. This is the same technique Litestream uses.

The test `concurrent_writer_with_checkpoint_pressure` runs a writer thread that
checkpoints aggressively (PASSIVE, RESTART and TRUNCATE) while the tail polls. With the
pin disabled, every run we tried failed: frames vanished mid-read, before-images came
from already-overwritten pages, and inserts were reported twice. With the pin, it passes.

## 4. From pages to tables: the page index

A transaction is a set of page images. To turn them into rows tailite must know which
table each page belongs to, before and after. SQLite pages carry no back-pointers, so
tailite keeps three maps, built once at attach by walking every table's b-tree:

- `owner`: page -> root page of the table it belongs to
- `parent`: page -> parent page
- `overflow`: overflow page -> (table, row key, page holding the cell)

They are maintained incrementally for each commit:

1. **Paths.** From every changed page that belongs to a table, walk `parent` up to the
   root. The tree's shape can only have changed along these paths.
2. **Re-walk.** Walk the same paths top-down through the *new* images. A rewritten
   interior page re-assigns owner and parent to all of its children. An unchanged page
   still has its old children, so only its on-path children are followed.
3. **Orphans.** Every page that was on a path, or was a child of a rewritten interior
   page, and was not met again during the re-walk has left the tree, together with
   whatever hangs below it in its old image. SQLite does not write a page it frees
   (`sqlite3PagerDontWrite`), so orphans are usually *not* part of the transaction.
   Their old images are the only record of the rows a big DELETE removed, and tailite
   reads them.

Cost: O(changed pages x tree depth) plus the pages actually freed. A table is never
scanned. Schema changes (the schema cookie in the header moved) instead rebuild the maps
from the new state, which is O(database) but rare.

The maps are a cache of a pure function of the database state. `TAILITE_VERIFY=1`
rebuilds them from scratch after every commit and asserts equality; the randomized
tests run with it on.

## 5. From pages to rows

Rows are collected from the old and new image of every changed page and orphan, on
whichever side the page belongs to a tracked table:

- rowid tables: cells of table-leaf pages, keyed by rowid;
- WITHOUT ROWID tables: cells of every page of their index b-tree (interior pages hold
  entries too), keyed by the primary key, with the record's PK-first column order mapped
  back to declaration order.

A cell that is byte-identical at the same offset on both images of a page is an
untouched row and is not decoded. Then, per table and key: present only before is a
delete, only after is an insert, both with different values is an update. A row that
merely moved between pages (a split or merge) appears on both sides with equal values
and produces nothing.

One case slips past page-level reasoning: SQLite overwrites a same-size payload in
place, so updating a large blob can rewrite only an overflow page and leave the leaf
byte-identical. The `overflow` map catches it: a changed overflow page leads back to its
row, which is decoded on both sides.

Record decoding applies what SQLite applies on read: the rowid alias column (`INTEGER
PRIMARY KEY`) is filled from the rowid, integers stored in REAL-affinity columns become
reals, and records shorter than the table (written before `ALTER TABLE ADD COLUMN`) are
padded with the column defaults. VIRTUAL generated columns are not stored and are not
reported.

## 6. What a transaction's changes can and cannot say

tailite sees the net effect of each committed transaction, which is what a replica or an
index needs. It cannot recover what the log does not record: statement order inside a
transaction (changes are ordered by table, then key), or a row that was inserted and
deleted within the same transaction.
