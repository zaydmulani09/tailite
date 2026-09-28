//! Write-ahead log reader. Yields only fully committed transactions: a run of frames
//! whose salts match the header and whose cumulative checksums verify, ending in a
//! commit frame. This is the same rule SQLite's own recovery uses, so a transaction
//! that is still being written (or was rolled back) is never surfaced.

use crate::format::{be32, read_at};
use crate::Result;
use std::collections::HashMap;
use std::fs::File;

const HEADER: usize = 32;
const FRAME_HEADER: usize = 24;

/// Pages written by one committed transaction: page number -> offset of the page image
/// in the WAL file. When a transaction writes a page twice, the last frame wins.
pub(crate) struct Commit {
    pub pages: HashMap<u32, u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Header {
    big_endian: bool,
    page_size: usize,
    salt: [u32; 2],
    cksum: [u32; 2],
}

/// Position in the log: which generation (salts) and how far into it we have consumed.
#[derive(Default)]
pub(crate) struct Cursor {
    header: Option<Header>,
    next_frame: u64,
    cksum: [u32; 2],
}

/// Result of one read of the log.
pub(crate) struct Read {
    /// The log was reset since the last read (a checkpoint completed and a writer
    /// started a new generation). Every frame of the old generation had already been
    /// consumed; the main database file now holds that state.
    pub restarted: bool,
    pub page_size: Option<usize>,
    pub commits: Vec<Commit>,
}

fn checksum(big_endian: bool, data: &[u8], mut s: [u32; 2]) -> [u32; 2] {
    for w in data.as_chunks::<8>().0 {
        let word = |i: usize| {
            let b = [w[i], w[i + 1], w[i + 2], w[i + 3]];
            if big_endian {
                u32::from_be_bytes(b)
            } else {
                u32::from_le_bytes(b)
            }
        };
        s[0] = s[0].wrapping_add(word(0)).wrapping_add(s[1]);
        s[1] = s[1].wrapping_add(word(4)).wrapping_add(s[0]);
    }
    s
}

fn header(wal: &File) -> Result<Option<Header>> {
    let mut b = [0u8; HEADER];
    if read_at(wal, &mut b, 0)? < HEADER {
        return Ok(None);
    }
    let magic = be32(&b, 0)?;
    if magic & !1 != 0x377f0682 {
        return Ok(None);
    }
    let h = Header {
        big_endian: magic & 1 == 1,
        page_size: be32(&b, 8)? as usize,
        salt: [be32(&b, 16)?, be32(&b, 20)?],
        cksum: [be32(&b, 24)?, be32(&b, 28)?],
    };
    // an invalid header means SQLite treats the log as empty
    let ok = checksum(h.big_endian, &b[..24], [0, 0]) == h.cksum
        && h.page_size.is_power_of_two()
        && (512..=65536).contains(&h.page_size);
    Ok(ok.then_some(h))
}

impl Cursor {
    pub fn read(&mut self, wal: &File) -> Result<Read> {
        let Some(h) = header(wal)? else {
            return Ok(Read { restarted: false, page_size: None, commits: vec![] });
        };
        let mut restarted = false;
        if self.header.map(|c| c.salt) != Some(h.salt) {
            restarted = self.header.is_some();
            *self = Cursor { header: Some(h), next_frame: 0, cksum: h.cksum };
        }
        let frame = (FRAME_HEADER + h.page_size) as u64;
        let mut buf = vec![0u8; FRAME_HEADER + h.page_size];
        let (mut i, mut s) = (self.next_frame, self.cksum);
        let mut pending = HashMap::new();
        let mut commits = vec![];
        loop {
            let off = HEADER as u64 + i * frame;
            if read_at(wal, &mut buf, off)? < buf.len() {
                break;
            }
            let fh = &buf[..FRAME_HEADER];
            if [be32(fh, 8)?, be32(fh, 12)?] != h.salt {
                break;
            }
            s = checksum(h.big_endian, &buf[..8], s);
            s = checksum(h.big_endian, &buf[FRAME_HEADER..], s);
            if s != [be32(fh, 16)?, be32(fh, 20)?] {
                break;
            }
            pending.insert(be32(fh, 0)?, off + FRAME_HEADER as u64);
            i += 1;
            if be32(fh, 4)? != 0 {
                // commit frame: everything up to here is durable and visible
                commits.push(Commit { pages: std::mem::take(&mut pending) });
                self.next_frame = i;
                self.cksum = s;
            }
        }
        Ok(Read { restarted, page_size: Some(h.page_size), commits })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_matches_sqlite_definition() {
        // s0 += x0 + s1; s1 += x1 + s0, over big-endian words
        let data = [0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4];
        assert_eq!(checksum(true, &data, [0, 0]), [1 + 3 + 3, 3 + 4 + 7]);
    }
}
