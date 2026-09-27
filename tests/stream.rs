//! Differential tests: drive a real SQLite writer, follow it with `Tail`, replay the
//! events into a mirror, and compare the mirror with what SQLite itself returns.
//! Every update and delete must also carry the exact prior row image.

use rusqlite::{types::ValueRef, Connection};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use tailite::{Change, Op, Tail, Value};

/// table -> row key (rowid, or the first column for WITHOUT ROWID tables) -> row
type Mirror = HashMap<String, BTreeMap<String, Vec<Value>>>;

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
    let names: Vec<(String, bool)> = names
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .map(|(n, sql)| (n, sql.unwrap_or_default().to_uppercase().contains("WITHOUT ROWID")))
        .collect();
    let mut m = Mirror::new();
    for (name, without_rowid) in names {
        let mut q =
            c.prepare(&format!("SELECT {}* FROM \"{name}\"", if without_rowid { "" } else { "_rowid_, " })).unwrap();
        let n = q.column_count();
        let rows = q
            .query_map([], |r| {
                let vals: Vec<Value> = (0..n).map(|i| value(r.get_ref(i).unwrap())).collect();
                Ok(match without_rowid {
                    true => (format!("{:?}", vals[0]), vals),
                    false => (format!("{:?}", vals[0]), vals[1..].to_vec()),
                })
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        m.insert(name, rows);
    }
    m
}

fn key(c: &Change) -> String {
    match c.rowid {
        Some(r) => format!("{:?}", Value::Integer(r)),
        None => format!("{:?}", c.after.as_ref().or(c.before.as_ref()).unwrap()[0]),
    }
}

fn replay(m: &mut Mirror, changes: &[Change]) {
    for c in changes {
        let k = key(c);
        let t = m.entry(c.table.clone()).or_default();
        match c.op {
            Op::Insert => {
                assert!(t.insert(k, c.after.clone().unwrap()).is_none(), "insert of existing row {c:?}");
            }
            Op::Update => {
                let prev = t.insert(k, c.after.clone().unwrap());
                assert_eq!(
                    prev.as_ref(),
                    c.before.as_ref(),
                    "update before-image mismatch on {}#{:?}",
                    c.table,
                    c.rowid
                );
            }
            Op::Delete => {
                let prev = t.remove(&k);
                assert_eq!(
                    prev.as_ref(),
                    c.before.as_ref(),
                    "delete before-image mismatch on {}#{:?}",
                    c.table,
                    c.rowid
                );
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
                let missing: Vec<_> = rows.keys().filter(|k| !mine.contains_key(*k)).take(5).collect();
                let extra: Vec<_> = mine.keys().filter(|k| !rows.contains_key(*k)).take(5).collect();
                let differ: Vec<_> = rows
                    .iter()
                    .filter(|(k, v)| mine.get(*k).is_some_and(|x| x != *v))
                    .map(|(k, _)| k)
                    .take(5)
                    .collect();
                let show = |v: &Vec<Value>| -> String {
                    v.iter()
                        .map(|x| match x {
                            Value::Blob(b) => format!("blob[{}:{:02x?}..]", b.len(), &b[..b.len().min(4)]),
                            o => format!("{o:?}"),
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let detail = differ
                    .first()
                    .map(|k| {
                        format!(
                            "
 mine: {}
 want: {}",
                            show(&mine[*k]),
                            show(&rows[*k])
                        )
                    })
                    .unwrap_or_default();
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
    assert_eq!(txs[1].changes[0].rowid, Some(2));
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
    // cross-check the incremental page indexes against a full rebuild after every commit
    std::env::set_var("TAILITE_VERIFY", "1");
    let path = temp_db(name);
    let w = writer(&path, pragmas);
    w.execute_batch(
        "PRAGMA wal_autocheckpoint=40;
         CREATE TABLE t1(id INTEGER PRIMARY KEY, a TEXT, b BLOB, c REAL);
         CREATE TABLE t2(x, y);
         CREATE TABLE t3(k TEXT PRIMARY KEY, v);
         CREATE TABLE t4(id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT);
         CREATE INDEX t1_a ON t1(a);
         CREATE TABLE w1(k TEXT PRIMARY KEY, v, n INTEGER) WITHOUT ROWID;
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
            96..=97 => format!(
                "INSERT OR REPLACE INTO w1 VALUES ('w{}', randomblob({}), {})",
                rng.below(400),
                [5, 200, 3000][rng.below(3) as usize],
                rng.below(9)
            ),
            98 => format!("DELETE FROM w1 WHERE n = {}; UPDATE w1 SET v = randomblob(length(v)) WHERE n = {};", rng.below(9), rng.below(9)),
            _ => "INSERT INTO t1(a, b) SELECT a, b FROM t1 ORDER BY random() LIMIT 30".into(),
        };
        // checkpoints report busy through a result row rather than an error
        if sql.starts_with("PRAGMA") {
            w.query_row(&sql, [], |_| Ok(())).unwrap();
        } else {
            w.execute_batch(&sql).unwrap_or_else(|e| panic!("step {step}: {sql}: {e}"));
        }
        if std::env::var("TRACE").is_ok() {
            eprintln!("step {step}: {sql}");
        }
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
        random_workload(
            &format!("av-{seed}"),
            "PRAGMA page_size=1024; PRAGMA auto_vacuum=FULL;",
            seed * 0xd1b54a32d192ed03,
            250,
        );
    }
}

#[test]
fn snapshot_then_stream_is_complete() {
    let path = temp_db("snapshot");
    let w = writer(&path, "");
    w.execute_batch(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v);
         WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i<2000) INSERT INTO t(v) SELECT randomblob(i % 700) FROM s;",
    )
    .unwrap();
    let mut tail = Tail::open(&path).unwrap();
    let mut mirror = Mirror::new();
    replay(&mut mirror, &tail.snapshot().unwrap());
    check(&w, &mut mirror);
    w.execute_batch("DELETE FROM t WHERE id % 3 = 0; UPDATE t SET v = 'x' WHERE id % 5 = 0;").unwrap();
    poll_into(&mut tail, &mut mirror);
    check(&w, &mut mirror);
}

#[test]
fn schema_changes_are_followed() {
    let path = temp_db("ddl");
    let w = writer(&path, "");
    w.execute_batch("CREATE TABLE a(x); INSERT INTO a VALUES (1), (2);").unwrap();
    let mut tail = Tail::open(&path).unwrap();
    let mut mirror = truth(&w);

    // ADD COLUMN: old records are shorter; the default fills the gap in before-images
    w.execute_batch("ALTER TABLE a ADD COLUMN y TEXT DEFAULT 'none'; UPDATE a SET x = 10 WHERE x = 1;").unwrap();
    let txs = tail.poll().unwrap();
    let upd = txs.iter().flat_map(|t| &t.changes).find(|c| c.op == Op::Update).unwrap();
    assert_eq!(upd.before.as_ref().unwrap(), &[Value::Integer(1), Value::Text("none".into())]);
    assert_eq!(upd.after.as_ref().unwrap(), &[Value::Integer(10), Value::Text("none".into())]);

    // create + fill in one transaction, then rename, then drop
    w.execute_batch("BEGIN; CREATE TABLE b(k INTEGER PRIMARY KEY, v); INSERT INTO b(v) VALUES ('p'), ('q'); COMMIT;")
        .unwrap();
    let txs = tail.poll().unwrap();
    let ins: Vec<_> = txs.iter().flat_map(|t| &t.changes).filter(|c| c.table == "b").collect();
    assert_eq!(ins.len(), 2);
    w.execute_batch("ALTER TABLE b RENAME TO c; INSERT INTO c(v) VALUES ('r');").unwrap();
    let txs = tail.poll().unwrap();
    let c: Vec<_> = txs.iter().flat_map(|t| &t.changes).collect();
    assert_eq!(c.len(), 1, "rename moves no rows: {c:?}");
    assert_eq!((c[0].table.as_str(), c[0].rowid), ("c", Some(3)));
    w.execute_batch("DROP TABLE c; INSERT INTO a(x) VALUES (3);").unwrap();
    let txs = tail.poll().unwrap();
    let c: Vec<_> = txs.iter().flat_map(|t| &t.changes).collect();
    assert_eq!(c.len(), 1, "a drop reports no deletes: {c:?}");
    // keep the mirror honest for the tables that still exist
    mirror.clear();
    replay(&mut mirror, &tail.snapshot().unwrap());
    check(&w, &mut mirror);
}

#[test]
fn generated_columns_and_affinity() {
    let path = temp_db("gen");
    let w = writer(&path, "");
    w.execute_batch(
        "CREATE TABLE g(a INTEGER, b AS (a * 2) VIRTUAL, c REAL GENERATED ALWAYS AS (a * 3) STORED, d TEXT CHECK (CAST(d AS TEXT) = d), e REAL)",
    )
    .unwrap();
    let mut tail = Tail::open(&path).unwrap();
    w.execute_batch("INSERT INTO g(a, d, e) VALUES (7, 'x', 2)").unwrap();
    let c = &tail.poll().unwrap()[0].changes[0];
    assert_eq!(c.columns, ["a", "c", "d", "e"], "virtual columns are not stored and not reported");
    assert_eq!(
        c.after.as_ref().unwrap(),
        &[Value::Integer(7), Value::Real(21.0), Value::Text("x".into()), Value::Real(2.0)]
    );
}

#[test]
fn utf16_database() {
    let path = temp_db("utf16");
    let w = writer(&path, "PRAGMA encoding='UTF-16le';");
    w.execute_batch("CREATE TABLE t(s TEXT)").unwrap();
    let mut tail = Tail::open(&path).unwrap();
    w.execute_batch("INSERT INTO t VALUES ('héllo wörld ✓')").unwrap();
    let c = &tail.poll().unwrap()[0].changes[0];
    assert_eq!(c.after.as_ref().unwrap(), &[Value::Text("héllo wörld ✓".into())]);
}

#[test]
fn diff_two_files() {
    let path = temp_db("diff");
    let w = writer(&path, "");
    w.execute_batch(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, v);
         WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i<3000) INSERT INTO t(v) SELECT randomblob(i % 900) FROM s;",
    )
    .unwrap();
    let copy = path.with_file_name("copy.db");
    w.execute(&format!("VACUUM INTO '{}'", copy.display()), []).unwrap();
    let mut mirror = truth(&w);
    w.execute_batch("DELETE FROM t WHERE id BETWEEN 100 AND 1900; UPDATE t SET v = 42 WHERE id % 97 = 0; INSERT INTO t(v) VALUES (1);").unwrap();
    // diff reads the committed log too, so no checkpoint is needed
    let changes = tailite::diff(&copy, &path).unwrap();
    replay(&mut mirror, &changes);
    check(&w, &mut mirror);
    assert!(tailite::diff(&path, &path).unwrap().is_empty());
}

#[test]
fn refuses_rollback_journal_databases() {
    let path = temp_db("journal");
    let c = Connection::open(&path).unwrap();
    c.execute_batch("CREATE TABLE t(x)").unwrap();
    let err = Tail::open(&path).err().expect("must refuse").to_string();
    assert!(err.contains("WAL"), "{err}");
}

#[test]
fn without_rowid_composite_key() {
    let path = temp_db("wr");
    let w = writer(&path, "PRAGMA page_size=1024;");
    w.execute_batch("CREATE TABLE edges(src TEXT, weight REAL, dst TEXT, PRIMARY KEY(dst, src)) WITHOUT ROWID")
        .unwrap();
    let mut tail = Tail::open(&path).unwrap();
    w.execute_batch(
        "WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM s WHERE i<500)
         INSERT INTO edges SELECT 'n' || (i % 37), i, 'n' || i || hex(randomblob(i % 50)) FROM s",
    )
    .unwrap();
    let txs = tail.poll().unwrap();
    assert_eq!(txs[0].changes.len(), 500, "rows in interior index pages count too");
    assert!(txs[0].changes.iter().all(|c| c.rowid.is_none() && c.columns == ["src", "weight", "dst"]));
    w.execute_batch("UPDATE edges SET weight = -weight WHERE src = 'n3'; DELETE FROM edges WHERE src = 'n4';").unwrap();
    let txs = tail.poll().unwrap();
    let upd = &txs[0].changes;
    assert!(
        !upd.is_empty()
            && upd.iter().all(|c| c.op == Op::Update && c.after.as_ref().unwrap()[0] == Value::Text("n3".into()))
    );
    assert!(matches!(upd[0].after.as_ref().unwrap()[1], Value::Real(x) if x < 0.0));
    assert!(txs[1].changes.iter().all(|c| c.op == Op::Delete));
}
