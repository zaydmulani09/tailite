//! Differential tests: drive a real SQLite writer, follow it with `Tail`, replay the
//! events into a mirror, and compare the mirror with what SQLite itself returns.
//! Every update and delete must also carry the exact prior row image.

use rusqlite::{types::ValueRef, Connection};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use tailite::{Change, Op, Tail, Value};

type Mirror = HashMap<String, BTreeMap<i64, Vec<Value>>>;

fn temp_db(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tailite-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("t.db")
}

fn writer(path: &Path, pragmas: &str) -> Connection {
    let c = Connection::open(path).unwrap();
    c.execute_batch(pragmas).unwrap();
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF;").unwrap();
    c
}

fn value(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Integer(i),
        ValueRef::Real(f) => Value::Real(f),
        ValueRef::Text(t) => Value::Text(String::from_utf8_lossy(t).into()),
        ValueRef::Blob(b) => Value::Blob(b.to_vec()),
    }
}

/// Every rowid table, as SQLite sees it.
fn truth(c: &Connection) -> Mirror {
    let mut names = c.prepare("SELECT name, sql FROM sqlite_schema WHERE type='table' AND rootpage>0").unwrap();
    let names: Vec<String> = names
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .filter(|(_, sql)| !sql.as_deref().unwrap_or("").to_uppercase().contains("WITHOUT ROWID"))
        .map(|(n, _)| n)
        .collect();
    let mut m = Mirror::new();
    for name in names {
        let mut q = c.prepare(&format!("SELECT _rowid_, * FROM \"{name}\"")).unwrap();
        let n = q.column_count();
        let rows = q
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, (1..n).map(|i| value(r.get_ref(i).unwrap())).collect())))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        m.insert(name, rows);
    }
    m
}

fn replay(m: &mut Mirror, changes: &[Change]) {
    for c in changes {
        let t = m.entry(c.table.clone()).or_default();
        match c.op {
            Op::Insert => {
                assert!(t.insert(c.rowid, c.after.clone().unwrap()).is_none(), "insert of existing row {c:?}");
            }
            Op::Update => {
                let prev = t.insert(c.rowid, c.after.clone().unwrap());
                assert_eq!(prev.as_ref(), c.before.as_ref(), "update before-image mismatch on {}#{}", c.table, c.rowid);
            }
            Op::Delete => {
                let prev = t.remove(&c.rowid);
                assert_eq!(prev.as_ref(), c.before.as_ref(), "delete before-image mismatch on {}#{}", c.table, c.rowid);
            }
        }
    }
}

fn poll_into(tail: &mut Tail, m: &mut Mirror) -> usize {
    let txs = tail.poll().unwrap();
    for tx in &txs {
        replay(m, &tx.changes);
    }
    txs.iter().map(|t| t.changes.len()).sum()
}

fn check(c: &Connection, m: &mut Mirror) {
    let t = truth(c);
    // dropped tables produce no delete events by design; forget them
    m.retain(|k, _| t.contains_key(k));
    m.retain(|_, rows| !rows.is_empty());
    let t: Mirror = t.into_iter().filter(|(_, r)| !r.is_empty()).collect();
    if *m != t {
        for (name, rows) in &t {
            let mine = m.get(name).cloned().unwrap_or_default();
            if &mine != rows {
                let missing: Vec<_> = rows.keys().filter(|k| !mine.contains_key(k)).take(5).collect();
                let extra: Vec<_> = mine.keys().filter(|k| !rows.contains_key(k)).take(5).collect();
                let differ: Vec<_> = rows.iter().filter(|(k, v)| mine.get(k).is_some_and(|x| x != *v)).map(|(k, _)| k).take(5).collect();
                let show = |v: &Vec<Value>| -> String {
                    v.iter().map(|x| match x { Value::Blob(b) => format!("blob[{}:{:02x?}..]", b.len(), &b[..b.len().min(4)]), o => format!("{o:?}") }).collect::<Vec<_>>().join(", ")
                };
                let detail = differ.first().map(|k| format!("
 mine: {}
 want: {}", show(&mine[k]), show(&rows[k]))).unwrap_or_default();
                panic!("table {name}: {} rows vs {} expected; missing {missing:?} extra {extra:?} differ {differ:?}{detail}", mine.len(), rows.len());
            }
        }
        panic!("mirror has tables SQLite does not: {:?}", m.keys().filter(|k| !t.contains_key(*k)).collect::<Vec<_>>());
    }
}

#[test]
fn basic_insert_update_delete() {
    let path = temp_db("basic");
    let w = writer(&path, "");
    w.execute_batch("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, score REAL, avatar BLOB)").unwrap();
    let mut tail = Tail::open(&path).unwrap();
    assert!(tail.poll().unwrap().is_empty());

    w.execute_batch("INSERT INTO users(name, score) VALUES ('ada', 1.5), ('grace', 2.0)").unwrap();
    let txs = tail.poll().unwrap();
    assert_eq!(txs.len(), 1);
    let c = &txs[0].changes;
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].op, Op::Insert);
    assert_eq!(c[0].columns, ["id", "name", "score", "avatar"]);
    assert_eq!(c[0].after, Some(vec![Value::Integer(1), Value::Text("ada".into()), Value::Real(1.5), Value::Null]));

    w.execute_batch("UPDATE users SET name = 'Ada' WHERE id = 1; DELETE FROM users WHERE id = 2;").unwrap();
    let txs = tail.poll().unwrap();
    assert_eq!(txs.len(), 2, "two autocommit statements are two transactions");
    assert_eq!(txs[0].changes[0].op, Op::Update);
    assert_eq!(txs[0].changes[0].before.as_ref().unwrap()[1], Value::Text("ada".into()));
    assert_eq!(txs[0].changes[0].after.as_ref().unwrap()[1], Value::Text("Ada".into()));
    assert_eq!(txs[1].changes[0].op, Op::Delete);
    assert_eq!(txs[1].changes[0].rowid, 2);
}

#[test]
fn rolled_back_and_uncommitted_work_is_invisible() {
    let path = temp_db("rollback");
    let w = writer(&path, "PRAGMA cache_size=1;"); // tiny cache: dirty pages spill into the log mid-transaction
    w.execute_batch("CREATE TABLE t(x)").unwrap();
    let mut tail = Tail::open(&path).unwrap();
    w.execute_batch("BEGIN; INSERT INTO t SELECT randomblob(900) FROM generate_series(1, 300);").unwrap_or_else(|_| {
        // generate_series may be absent; fall back to a recursive CTE
        w.execute_batch("WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i<300) INSERT INTO t SELECT randomblob(900) FROM s;").unwrap();
    });
    assert!(tail.poll().unwrap().is_empty(), "open transaction must not be visible");
    w.execute_batch("ROLLBACK").unwrap();
    assert!(tail.poll().unwrap().is_empty(), "rolled back transaction must not be visible");
    w.execute_batch("INSERT INTO t VALUES (1)").unwrap();
    let txs = tail.poll().unwrap();
    assert_eq!(txs.len(), 1);
    assert_eq!(txs[0].changes.len(), 1);
}

/// Tiny xorshift so the workload is reproducible without a rand dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn random_workload(name: &str, pragmas: &str, seed: u64, steps: usize) {
    let path = temp_db(name);
    let w = writer(&path, pragmas);
    w.execute_batch(
        "PRAGMA wal_autocheckpoint=40;
         CREATE TABLE t1(id INTEGER PRIMARY KEY, a TEXT, b BLOB, c REAL);
         CREATE TABLE t2(x, y);
         CREATE TABLE t3(k TEXT PRIMARY KEY, v);
         CREATE TABLE t4(id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT);
         CREATE INDEX t1_a ON t1(a);
         INSERT INTO t1(a, b, c) VALUES ('seed', zeroblob(10), 0.5);",
    )
    .unwrap();
    let mut rng = Rng(seed);
    let mut tail = Tail::open(&path).unwrap();
    let mut mirror = truth(&w);
    let mut extra_tables = 0;
    let mut events = 0;
    for step in 0..steps {
        let sql = match rng.below(100) {
            0..=24 => format!(
                "INSERT INTO t1(a, b, c) VALUES ('{}', randomblob({}), {})",
                rng.below(1000),
                [0, 10, 300, 3000, 20000][rng.below(5) as usize],
                rng.below(1000) as f64 / 7.0
            ),
            25..=34 => format!("UPDATE t1 SET a = a || 'x', c = c + 1 WHERE id % 7 = {}", rng.below(7)),
            // same-size payload: SQLite overwrites overflow pages in place
            35..=41 => format!("UPDATE t1 SET b = randomblob(length(b)) WHERE id = (SELECT id FROM t1 ORDER BY id LIMIT 1 OFFSET {})", rng.below(50)),
            42..=49 => format!("DELETE FROM t1 WHERE id % {} = 0", 3 + rng.below(10)),
            50..=59 => format!(
                "WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i<{}) INSERT INTO t2 SELECT i, hex(randomblob(i % 40)) FROM s",
                1 + rng.below(400)
            ),
            60..=65 => "DELETE FROM t2 WHERE rowid IN (SELECT rowid FROM t2 ORDER BY random() LIMIT 200)".into(),
            66..=67 => "DELETE FROM t2".into(),
            68..=75 => format!("INSERT OR REPLACE INTO t3 VALUES ('k{}', randomblob({}))", rng.below(300), rng.below(2000)),
            76..=79 => format!("DELETE FROM t3 WHERE k > 'k{}'", rng.below(300)),
            80..=84 => format!("INSERT INTO t4(note) VALUES ('{}')", rng.below(1 << 30)),
            85..=86 => "BEGIN; UPDATE t1 SET c = -c; DELETE FROM t4 WHERE id % 2 = 0; INSERT INTO t2 VALUES (1, 2); COMMIT;".into(),
            87 => "VACUUM".into(),
            88..=89 => ["PRAGMA wal_checkpoint(PASSIVE)", "PRAGMA wal_checkpoint(RESTART)", "PRAGMA wal_checkpoint(TRUNCATE)"]
                [rng.below(3) as usize]
                .into(),
            90..=92 => {
                extra_tables += 1;
                format!("CREATE TABLE x{extra_tables}(a, b INTEGER PRIMARY KEY); INSERT INTO x{extra_tables}(a) VALUES (1), (2), (randomblob(5000));")
            }
            93..=94 if extra_tables > 0 => format!("DROP TABLE IF EXISTS x{}", 1 + rng.below(extra_tables)),
            95 => "UPDATE t2 SET y = y || y WHERE rowid % 5 = 0".into(),
            _ => "INSERT INTO t1(a, b) SELECT a, b FROM t1 ORDER BY random() LIMIT 30".into(),
        };
        // checkpoints report busy through a result row rather than an error
        if sql.starts_with("PRAGMA") {
            w.query_row(&sql, [], |_| Ok(())).unwrap();
        } else {
            w.execute_batch(&sql).unwrap_or_else(|e| panic!("step {step}: {sql}: {e}"));
        }
        if std::env::var("TRACE").is_ok() { eprintln!("step {step}: {sql}"); }
        if rng.below(3) == 0 {
            events += poll_into(&mut tail, &mut mirror);
            check(&w, &mut mirror);
        }
    }
    events += poll_into(&mut tail, &mut mirror);
    check(&w, &mut mirror);
    assert!(events > steps, "workload should produce plenty of events, got {events}");
}

#[test]
fn random_workload_default_pages() {
    for seed in 1..=4 {
        random_workload(&format!("rand-{seed}"), "", seed * 0x9e3779b97f4a7c15, 250);
    }
}

#[test]
fn random_workload_small_pages() {
    // 512-byte pages: deep trees, long overflow chains, constant splits and merges
    for seed in 1..=3 {
        random_workload(&format!("small-{seed}"), "PRAGMA page_size=512;", seed * 0x2545f4914f6cdd1d, 250);
    }
}

#[test]
fn random_workload_auto_vacuum() {
    // auto_vacuum relocates pages at every commit to keep the file compact
    for seed in 1..=3 {
        random_workload(&format!("av-{seed}"), "PRAGMA page_size=1024; PRAGMA auto_vacuum=FULL;", seed * 0xd1b54a32d192ed03, 250);
    }
}
