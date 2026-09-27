//! How fast does tailite turn commits into row events, and what does attaching cost?
//!
//!     cargo run --release --example bench -- [rows] [rows-per-transaction]
//!
//! Time spent inside `Tail::poll` is measured separately from time spent writing, so
//! the decode rate is tailite's own cost, not SQLite's.

use rusqlite::Connection;
use std::time::{Duration, Instant};
use tailite::Tail;

fn main() {
    let args: Vec<usize> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let rows = args.first().copied().unwrap_or(200_000);
    let batch = args.get(1).copied().unwrap_or(100);
    let dir = std::env::temp_dir().join(format!("tailite-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bench.db");

    let w = Connection::open(&path).unwrap();
    w.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         CREATE TABLE events(id INTEGER PRIMARY KEY, kind TEXT, payload BLOB, ts REAL);
         CREATE INDEX events_kind ON events(kind);",
    )
    .unwrap();
    // a pre-existing table so attaching has something to index
    w.execute_batch(&format!(
        "WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i<{rows})
         INSERT INTO events(kind, payload, ts) SELECT 'k' || (i % 50), randomblob(64 + i % 200), i * 0.5 FROM s;
         PRAGMA wal_checkpoint(TRUNCATE);"
    ))
    .unwrap();
    let size = std::fs::metadata(&path).unwrap().len();

    let t = Instant::now();
    let mut tail = Tail::open(&path).unwrap();
    println!("attach: indexed {:.1} MB, {rows} rows in {:.0?}", size as f64 / 1e6, t.elapsed());

    let t = Instant::now();
    let snap = tail.snapshot().unwrap().len();
    println!("snapshot: {snap} rows in {:.0?} ({:.0} rows/s)", t.elapsed(), snap as f64 / t.elapsed().as_secs_f64());

    let phases: [(&str, String); 3] = [
        ("insert", "INSERT INTO events(kind, payload, ts) VALUES ('new', randomblob(120), 1.0)".into()),
        ("update", "UPDATE events SET ts = ts + 1 WHERE id = abs(random()) % $ROWS + 1".into()),
        ("delete", "DELETE FROM events WHERE id = abs(random()) % $ROWS + 1".into()),
    ];
    for (name, stmt) in phases {
        let stmt = stmt.replace("$ROWS", &rows.to_string());
        let (mut writing, mut polling, mut events, mut txs) = (Duration::ZERO, Duration::ZERO, 0usize, 0usize);
        for _ in 0..(rows / 10 / batch).max(1) {
            let t = Instant::now();
            w.execute_batch("BEGIN").unwrap();
            for _ in 0..batch {
                w.execute_batch(&stmt).unwrap();
            }
            w.execute_batch("COMMIT").unwrap();
            writing += t.elapsed();
            let t = Instant::now();
            for tx in tail.poll().unwrap() {
                events += tx.changes.len();
                txs += 1;
            }
            polling += t.elapsed();
        }
        println!(
            "{name:>6}: {events} events in {txs} transactions; tailite {:.0?} ({:.0} events/s), SQLite writing {:.0?}",
            polling,
            events as f64 / polling.as_secs_f64(),
            writing
        );
    }
    drop(tail);
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}
