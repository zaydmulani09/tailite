//! Following a live database.
//!
//! The hazard in reading someone else's WAL is the checkpoint: it copies frames into
//! the main file and may then restart the log, overwriting frames we have not read and
//! destroying the "before" images we need. SQLite never checkpoints past the snapshot
//! of an open reader, so tailite keeps a read transaction open at a point it has already
//! processed (the pin). Each poll opens a new pin *before* reading the log and drops the
//! old one only after everything up to the new pin has been processed, so at every
//! moment:
//!
//! * the main file holds no page newer than what we have processed, and
//! * the log is not restarted while it holds frames we have not processed.
//!
//! This is the same trick Litestream uses. The cost to the writer is that a checkpoint
//! can only advance to where tailite has read, which with a short poll interval is
//! at most one interval behind.

use crate::format::{Geometry, Pages};
use crate::tracker::Tracker;
use crate::wal::Cursor;
use crate::{Change, Error, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};

/// A committed transaction and the rows it changed.
#[derive(Clone, Debug, PartialEq)]
pub struct Transaction {
    /// Position in the stream, counting from 1 at [`Tail::open`].
    pub seq: u64,
    /// Changed rows, ordered by table name then rowid (or key). SQLite does not record
    /// statement order inside a transaction, so neither can a log reader.
    pub changes: Vec<Change>,
}

/// A read transaction held open to stop checkpoints overtaking us.
struct Pin(Connection);

impl Pin {
    fn new(path: &Path) -> Result<Pin> {
        let open = |flags| Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX);
        // read-only when possible; read-only WAL access needs an existing -shm, so fall back
        let conn = open(OpenFlags::SQLITE_OPEN_READ_ONLY).or_else(|_| open(OpenFlags::SQLITE_OPEN_READ_WRITE))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Pin::begin(conn)
    }

    fn begin(conn: Connection) -> Result<Pin> {
        // a deferred transaction only takes its snapshot on the first read
        conn.execute_batch("BEGIN")?;
        conn.query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))?;
        Ok(Pin(conn))
    }

    fn end(self) -> Result<Connection> {
        self.0.execute_batch("COMMIT")?;
        Ok(self.0)
    }
}

/// The main file, the log, and the frames that make up the current state.
pub(crate) struct Files {
    pub db: File,
    /// false for rollback-journal databases: no log to read, the file is the state
    pub wal_mode: bool,
    pub wal: Option<File>,
    pub wal_path: PathBuf,
    pub cursor: Cursor,
    /// page -> latest committed frame offset in the current log generation
    pub overlay: HashMap<u32, u64>,
    pub geo: Geometry,
}

impl Files {
    /// Open a database and absorb every transaction already committed to its log.
    pub fn open(path: &Path) -> Result<Files> {
        let db = File::open(path)?;
        let mut head = [0u8; 100];
        let n = crate::format::read_at(&db, &mut head, 0)?;
        if n != 0 && (n < 100 || &head[..16] != b"SQLite format 3\0") {
            return Err(Error::NotWal(format!("{} is not a SQLite database", path.display())));
        }
        // a WAL database whose first transaction was never checkpointed has an empty main file
        let wal_mode = n == 0 || (head[18] == 2 && head[19] == 2);
        let mut wal_path = path.as_os_str().to_owned();
        wal_path.push("-wal");
        let wal_path = PathBuf::from(wal_path);
        let mut files = Files {
            db,
            wal_mode,
            wal: None,
            wal_path,
            cursor: Cursor::default(),
            overlay: HashMap::new(),
            geo: Geometry { page_size: 0, usable: 0, encoding: 1 },
        };
        let from_wal = files.read_log()?;
        let page_size = match (from_wal.0, u16::from_be_bytes([head[16], head[17]])) {
            (Some(ps), _) => ps,
            (None, 1) => 65536,
            (None, ps) if n == 100 => ps as usize,
            _ => return Err(Error::NotWal(format!("{} is empty", path.display()))),
        };
        for c in from_wal.1 {
            files.overlay.extend(c.pages);
        }
        // reserved bytes and text encoding come from page 1 as the log sees it
        files.geo = Geometry { page_size, usable: page_size, encoding: 1 };
        let page1 = files.pages(None).page(1)?;
        files.geo.usable = page_size - page1[20] as usize;
        files.geo.encoding = crate::format::be32(&page1, 56)?.max(1);
        Ok(files)
    }

    /// Committed transactions appended since the last call. Clears the overlay if the
    /// log restarted (the main file then holds everything before the restart).
    fn read_log(&mut self) -> Result<(Option<usize>, Vec<crate::wal::Commit>)> {
        if self.wal.is_none() && self.wal_mode {
            self.wal = File::open(&self.wal_path).ok();
        }
        let Some(wal) = &self.wal else { return Ok((None, vec![])) };
        let read = self.cursor.read(wal)?;
        if read.restarted {
            self.overlay.clear();
        }
        Ok((read.page_size, read.commits))
    }

    pub fn page_count(&self) -> Result<u32> {
        crate::format::be32(&self.pages(None).page(1)?, 28)
    }

    pub fn pages<'a>(&'a self, top: Option<&'a HashMap<u32, u64>>) -> Pages<'a> {
        Pages { db: &self.db, wal: self.wal.as_ref(), layers: [Some(&self.overlay), top], geo: self.geo }
    }
}

/// Follows a live SQLite database and reports row changes per committed transaction.
pub struct Tail {
    files: Files,
    tracker: Tracker,
    pin: Option<Pin>,
    spare: Option<Connection>,
    path: PathBuf,
    seq: u64,
}

impl Tail {
    /// Attach to a database in WAL mode. Changes committed before this call are the
    /// baseline and are not reported. Reads the whole database once to index it.
    pub fn open(path: impl AsRef<Path>) -> Result<Tail> {
        let path = path.as_ref().to_path_buf();
        let pin = Pin::new(&path)?;
        let files = Files::open(&path)?;
        if !files.wal_mode {
            return Err(Error::NotWal(format!(
                "{} is not in WAL mode; run `PRAGMA journal_mode=WAL` on it once (the setting is persistent)",
                path.display()
            )));
        }
        let tracker = Tracker::build(&files.pages(None))?;
        Ok(Tail { files, tracker, pin: Some(pin), spare: None, path, seq: 0 })
    }

    /// Every row as of the last processed transaction, as inserts. Called right after
    /// [`Tail::open`], this is the initial load that the following polls continue
    /// without gap or overlap: the state it reads is pinned, so nothing slips between.
    pub fn snapshot(&self) -> Result<Vec<Change>> {
        self.tracker.snapshot(&self.files.pages(None))
    }

    /// Process every transaction committed since the last poll. Never blocks on the
    /// writer; returns an empty vec when nothing new has been committed.
    ///
    /// An error means the log or the database could not be read or decoded. The
    /// position may then be partway through a batch, so drop this `Tail` and open a new
    /// one (with [`Tail::snapshot`] to resynchronize) rather than polling it again.
    pub fn poll(&mut self) -> Result<Vec<Transaction>> {
        // pin first, then read: everything the new pin can see is in the log by now
        let next = match self.spare.take() {
            Some(conn) => Pin::begin(conn)?,
            None => Pin::new(&self.path)?,
        };
        let (_, commits) = self.files.read_log()?;
        let mut out = vec![];
        for commit in commits {
            let changed: HashSet<u32> = commit.pages.keys().copied().collect();
            let changes = {
                let old = self.files.pages(None);
                let new = self.files.pages(Some(&commit.pages));
                self.tracker.apply(&old, &new, &changed)?
            };
            self.files.overlay.extend(commit.pages);
            self.seq += 1;
            if !changes.is_empty() {
                out.push(Transaction { seq: self.seq, changes });
            }
        }
        if let Some(old) = self.pin.replace(next) {
            self.spare = Some(old.end()?);
        }
        Ok(out)
    }
}
