//! Keep a durable audit trail of every row change in a database you do not control.
//!
//!     cargo run --release --example audit -- app.db audit.db
//!
//! Each change becomes a row in `audit.db`: when it was seen, which transaction, table,
//! operation, rowid, and the before/after images as SQL literals. The watched database
//! is never written to.

use rusqlite::{params, Connection};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tailite::{Tail, Value};

fn render(columns: &[String], row: &Option<Vec<Value>>) -> Option<String> {
    let row = row.as_ref()?;
    Some(columns.iter().zip(row).map(|(c, v)| format!("{c}={v}")).collect::<Vec<_>>().join(", "))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [watched, audit] = args.as_slice() else {
        eprintln!("usage: audit <watched.db> <audit.db>");
        std::process::exit(2);
    };
    let out = Connection::open(audit)?;
    out.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit(
            seen_at REAL, tx INTEGER, tbl TEXT, op TEXT, row_id INTEGER, before TEXT, after TEXT)",
    )?;
    let mut tail = Tail::open(watched)?;
    eprintln!("auditing {watched} into {audit}");
    loop {
        let txs = tail.poll()?;
        if !txs.is_empty() {
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
            // one audit transaction per poll keeps the audit log cheap to write
            let batch = out.unchecked_transaction()?;
            for tx in &txs {
                for c in &tx.changes {
                    batch.execute(
                        "INSERT INTO audit VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            now,
                            tx.seq as i64,
                            c.table,
                            format!("{:?}", c.op).to_lowercase(),
                            c.rowid,
                            render(&c.columns, &c.before),
                            render(&c.columns, &c.after)
                        ],
                    )?;
                }
            }
            batch.commit()?;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
