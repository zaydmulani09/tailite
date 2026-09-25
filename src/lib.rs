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
//!             println!("{} {:?} rowid={}", change.table, change.op, change.rowid);
//!         }
//!     }
//!     std::thread::sleep(std::time::Duration::from_millis(100));
//! }
//! # Ok::<(), tailite::Error>(())
//! ```


mod format;
mod schema;
mod wal;

use std::fmt;

/// A single SQLite value as stored in a record.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
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
    pub rowid: i64,
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
