# tailite

**Row-level change data capture for any SQLite database, from outside the process.**

tailite reads a database's write-ahead log, decodes the b-tree pages of every committed
transaction, and reports which rows were inserted, updated and deleted, with full
before and after values. The application writing the database needs no triggers, no
hooks, no extension, no schema change and no restart. It does not even need to know
tailite exists.

```console
$ tailite watch app.db
tailite: following app.db
tx 1  users INSERT rowid=2  id=2 name='grace' plan='pro'
tx 2  users UPDATE rowid=1  plan: 'free' → 'pro'
tx 3  users DELETE rowid=2  id=2 name='grace' plan='pro'
```

That output came from a Python process writing with its own `sqlite3` module while
`tailite watch` ran in another process.

## Why

SQLite is the most deployed database in the world, and it has no way to tell another
process what changed.

- `sqlite3_update_hook` and the preupdate hook work only inside the process that makes
  the change.
- The session extension also has to be enabled by the writer, per connection.
- Trigger-based CDC ([sqlite-cdc](https://github.com/kevinconway/sqlite-cdc) and similar)
  adds triggers and a log table to the schema and slows every write. You cannot do that
  to a database owned by an application you do not control.
- Litestream and LiteFS ship *pages*, not rows. They replicate a database; they cannot
  tell you that `users#42` changed its `email`.
- Turso's CDC is built into its own SQLite rewrite, not stock SQLite.
- WAL parsers such as [rustbish](https://github.com/p1tsi/rustbish) are forensic: they dump
  records from a file at rest. They do not follow a live database or produce before/after
  row images.

So people poll: `SELECT * ... WHERE updated_at > ?` (needs a column, misses deletes) or
diff whole tables on a timer. tailite replaces the polling with the log SQLite already
writes.

Things this makes possible:

- **See what your app writes.** Run `tailite watch` next to any app, ORM or framework and
  watch its database change row by row while you click around.
- **Replicate or index a SQLite database.** `--snapshot` emits every existing row, then
  the stream continues with no gap and no overlap. Pipe the JSON into a search index, a
  cache invalidator, a warehouse, or another database.
- **Audit a database you do not own:** desktop apps, browsers, home automation, edge
  devices. [`examples/audit.rs`](examples/audit.rs) writes a durable change log without
  touching the watched file.
- **Assert on writes in tests:** `Tail::open`, run the code under test, `poll`, and check
  exactly which rows changed.
- **Diff two database files by row.** `tailite diff old.db new.db` decodes only the pages
  that differ, so the cost follows the size of the change, not the size of the tables.

## Install

```sh
cargo install tailite
```

Prebuilt binaries for Linux, macOS (arm64 and x86_64) and Windows are attached to each
[GitHub release](https://github.com/zaydmulani09/tailite/releases). SQLite is compiled in;
there is nothing else to install.

## Use

### CLI

```text
tailite watch <db> [--json] [--snapshot] [--table <name>]... [--interval <ms>]
tailite diff <old.db> <new.db> [--json] [--table <name>]...
```

`watch` requires the database to be in WAL mode (`PRAGMA journal_mode=WAL;`, which is
persistent and what most applications already use). `diff` works on any database file.

`--json` prints one object per changed row (JSON Lines):

```json
{"tx":2,"table":"users","op":"update","rowid":1,"before":{"id":1,"name":"ada","plan":"free"},"after":{"id":1,"name":"ada","plan":"pro"}}
```

Integers and reals are JSON numbers, text is a string, NULL is `null`, a blob is
`{"$blob":"<hex>"}`. `rowid` is `null` for WITHOUT ROWID tables, whose rows are
identified by their primary key columns.

### Library

```toml
[dependencies]
tailite = "0.1"
```

```rust
let mut tail = tailite::Tail::open("app.db")?;

// optional: every existing row, as inserts, consistent with what poll() continues from
for change in tail.snapshot()? { /* initial load */ }

loop {
    for tx in tail.poll()? {
        for c in &tx.changes {
            // c.table, c.op (Insert/Update/Delete), c.rowid,
            // c.columns, c.before, c.after: Option<Vec<Value>>
            println!("{} {:?} {:?} -> {:?}", c.table, c.op, c.before, c.after);
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
}
```

`tailite::diff(old, new)` returns the same `Change` values for two files.

## How it works

The short version, with the long one in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md):

1. **Committed transactions only.** Frames are accepted exactly when SQLite's own recovery
   would accept them: matching salts, an unbroken checksum chain, ending in a commit
   frame. Uncommitted and rolled-back work is never reported.
2. **A pin keeps checkpoints behind the reader.** tailite holds a read transaction at a
   point it has already processed, and rotates it forward each poll: new pin first, then
   read, then release the old pin. SQLite never checkpoints past an open reader's snapshot
   and never resets the log under one, so neither the unread frames nor the "before"
   images in the main file can be overwritten while tailite needs them.
3. **A page index maps pages to tables.** Built once at attach, then maintained per
   commit by walking only the paths from changed pages to their roots. Pages that left
   the tree (SQLite frees pages without rewriting them) are found as orphans and their
   old images supply the rows a DELETE removed.
4. **Rows are diffed by key** from the old and new images: rowid for ordinary tables,
   primary key for WITHOUT ROWID tables. Byte-identical cells are skipped. An overflow
   index catches SQLite's in-place overwrite of same-size blobs, which changes no leaf.

## Correctness

The test suite drives real SQLite and checks tailite against SQLite itself:

- **Differential, randomized.** Thousands of random transactions (inserts, updates,
  range deletes, `DELETE FROM` truncation, same-size blob overwrites, `INSERT OR REPLACE`,
  CREATE/DROP TABLE, VACUUM, PASSIVE/RESTART/TRUNCATE checkpoints) over rowid, AUTOINCREMENT
  and WITHOUT ROWID tables, with 4096-, 1024- and 512-byte pages and with `auto_vacuum=FULL`.
  Every event is replayed into a mirror, every update and delete must carry the exact prior
  row, and after every poll the mirror must equal `SELECT *` of every table.
- **Index self-check.** In those runs `TAILITE_VERIFY=1` rebuilds the page index from
  scratch after every commit and asserts that the incremental one matches.
- **Real concurrency.** A writer thread commits and checkpoints aggressively while the tail
  polls from another thread. With the pin disabled this test failed on every run we tried
  (vanished frames, wrong before-images, duplicate inserts); with it, it passes.
- Also covered: open and rolled-back transactions stay invisible, DDL in the stream,
  ALTER TABLE ADD COLUMN defaults, generated columns, REAL affinity, UTF-16 databases,
  snapshot + stream consistency, file diff, and the CLI.

CI runs all of it on Linux, macOS and Windows.

## Performance

`cargo run --release --example bench` on a Windows 11 laptop, warm OS cache, 200,000-row
table (40 MB) with an index, 100 rows per transaction. "SQLite writing" is the time the
writer spent executing and committing the same transactions, for scale.

| phase | tailite | SQLite writing |
|-------|---------|----------------|
| attach (index 40 MB) | ~45 ms | |
| snapshot, 200k rows | 0.37 s (~540k rows/s) | |
| 20k appended rows | 81 ms (~245k events/s) | 183 ms |
| 20k random updates | 0.66 s (~30k events/s) | 0.89 s |
| 20k random deletes | 1.7 s (~11k events/s) | 2.6 s |

Random updates and deletes touch a different leaf for almost every row, so the cost is
dominated by reading two images of each touched page. In every phase tailite kept up
with the writer. CI runs the same benchmark with 100,000 rows on every push; on GitHub's
Linux, macOS and Windows runners it measured 430k-710k appended rows/s, 44k-88k random
updates/s and 32k-70k random deletes/s. Numbers vary with hardware; run it on yours.

## Limitations

- `watch` needs WAL mode. `diff` works in any mode.
- Changes are the **net effect** of each transaction, ordered by table then key. The log
  does not record statement order, and a row inserted and deleted in the same transaction
  leaves no trace.
- Changes committed while tailite is not running are not reported. There is no resumable
  position yet; use `--snapshot` to resynchronize after a restart.
- Attaching reads the whole database once to build the page index. The index costs about
  three hash-map entries per b-tree page (roughly 150 MB for a 10 GB database).
- Schema changes (and VACUUM) rebuild the index, O(database). `DROP TABLE` reports no
  deletes, a rename reports nothing, and VACUUM may renumber the rowids of tables without
  an `INTEGER PRIMARY KEY`, which shows up as deletes and inserts.
- VIRTUAL generated columns are not reported. Virtual tables (FTS5, R*Tree) are not
  followed themselves; their shadow tables are ordinary tables and are.
- `ALTER TABLE ADD COLUMN` defaults that are expressions rather than literals decode as NULL
  for rows written before the ALTER.
- Encrypted databases (SQLCipher, SEE) are not supported.
- While tailite runs, checkpoints advance only as far as it has read. With the default
  100 ms poll interval that is at most one interval behind.

## Roadmap

- Resumable positions for at-least-once delivery across restarts.
- Sinks: `--exec` per transaction, webhooks, NATS/Kafka.
- Python and Node bindings over the same core.
- Rollback-journal databases, by snapshotting pages at each change of the file change counter.
- A dense page index to cut memory on very large databases.

## License

MIT
