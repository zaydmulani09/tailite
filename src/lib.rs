//! Row-level change data capture for SQLite, from outside the writing process.
//!
//! `tailite` reads a database's write-ahead log directly, decodes every committed
//! transaction's b-tree pages, and turns page images into row events with full
//! before/after values. The application writing the database needs no triggers,
//! no hooks, no extension and no schema change.
//!
//! ```no_run
//! let mut tail = tailite::Tail::open("app.db")?;
//! loop {
//!     for tx in tail.poll()? {
//!         for change in &tx.changes {
//!             println!("{} {:?} {:?}", change.table, change.op, change.after);
//!         }
//!     }
//!     std::thread::sleep(std::time::Duration::from_millis(100));
//! }
//! # Ok::<(), tailite::Error>(())
//! ```
//!
//! [`Tail::snapshot`] returns every existing row as inserts, consistent with what the
//! following polls continue from, for an initial load. [`diff`] compares two database
//! files without a live writer.
//!
//! See `docs/ARCHITECTURE.md` in the repository for how pages become rows and why
//! checkpoints cannot overtake a running tail.

mod format;
mod schema;
mod tail;
mod tracker;
mod wal;

use std::fmt;
use std::path::Path;

pub use tail::{Tail, Transaction};

/// Row-level difference between two database files (either journal mode; a WAL
/// database's committed log is included). Unlike a full-table comparison, only pages
/// whose bytes differ are decoded, so the cost follows the size of the change.
/// Neither file should be written while the diff runs.
pub fn diff(old: impl AsRef<Path>, new: impl AsRef<Path>) -> Result<Vec<Change>> {
    let (a, b) = (tail::Files::open(old.as_ref())?, tail::Files::open(new.as_ref())?);
    let (pa, pb) = (a.pages(None), b.pages(None));
    let n = a.page_count()?.max(b.page_count()?);
    let mut changed = std::collections::HashSet::new();
    for p in 1..=n {
        if a.geo.page_size != b.geo.page_size || pa.page(p)? != pb.page(p)? {
            changed.insert(p);
        }
    }
    tracker::Tracker::build(&pa)?.rebuild(&pa, &pb, &changed)
}

/// A single SQLite value as stored in a record.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// Renders as an SQL literal: `NULL`, `42`, `1.5`, `'it''s'`, `X'00ff'`.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Real(x) => write!(f, "{x:?}"),
            Value::Text(s) => write!(f, "'{}'", s.replace('\'', "''")),
            Value::Blob(b) => write!(f, "X'{}'", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Insert,
    Update,
    Delete,
}

/// One row changed by a committed transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub table: String,
    pub op: Op,
    /// The row's rowid; `None` for WITHOUT ROWID tables, whose rows are identified by
    /// their primary key columns.
    pub rowid: Option<i64>,
    /// Column names, in declaration order. `before`/`after` line up with these.
    pub columns: Vec<String>,
    /// Row image before the transaction (`None` for inserts).
    pub before: Option<Vec<Value>>,
    /// Row image after the transaction (`None` for deletes).
    pub after: Option<Vec<Value>>,
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    /// The file is not a SQLite database in WAL mode.
    NotWal(String),
    /// On-disk structure did not decode. Indicates a corrupt file or a tailite bug.
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn corrupt(msg: impl Into<String>) -> Error {
    Error::Corrupt(msg.into())
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Sqlite(e) => write!(f, "sqlite: {e}"),
            Error::NotWal(m) => write!(f, "{m}"),
            Error::Corrupt(m) => write!(f, "undecodable database structure: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sqlite(e)
    }
}
