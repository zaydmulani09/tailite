//! On-disk SQLite structures: varints, b-tree pages, cells, overflow chains, records.
//! Reference: <https://www.sqlite.org/fileformat2.html>

use crate::{corrupt, Result, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io;

pub(crate) const INTERIOR_INDEX: u8 = 0x02;
pub(crate) const INTERIOR_TABLE: u8 = 0x05;
pub(crate) const LEAF_INDEX: u8 = 0x0a;
pub(crate) const LEAF_TABLE: u8 = 0x0d;

/// Positioned read that works on unix and windows without moving a shared cursor.
pub(crate) fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        #[cfg(unix)]
        let k = std::os::unix::fs::FileExt::read_at(f, &mut buf[n..], off + n as u64)?;
        #[cfg(windows)]
        let k = std::os::windows::fs::FileExt::seek_read(f, &mut buf[n..], off + n as u64)?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

pub(crate) fn be16(b: &[u8], at: usize) -> Result<usize> {
    let s = b.get(at..at + 2).ok_or_else(|| corrupt("short read (u16)"))?;
    Ok(u16::from_be_bytes([s[0], s[1]]) as usize)
}

pub(crate) fn be32(b: &[u8], at: usize) -> Result<u32> {
    let s = b.get(at..at + 4).ok_or_else(|| corrupt("short read (u32)"))?;
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// SQLite varint: 1-9 bytes, big-endian, 7 bits per byte, the 9th byte contributes all 8.
pub(crate) fn varint(b: &[u8], at: usize) -> Result<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..9 {
        let byte = *b.get(at + i).ok_or_else(|| corrupt("truncated varint"))?;
        if i == 8 {
            return Ok(((v << 8) | byte as u64, 9));
        }
        v = (v << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Ok((v, i + 1));
        }
    }
    unreachable!()
}

/// Geometry every page decode needs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Geometry {
    pub page_size: usize,
    /// page_size minus the per-page reserved bytes (db header offset 20)
    pub usable: usize,
    /// text encoding: 1 = utf-8, 2 = utf-16le, 3 = utf-16be
    pub encoding: u32,
}

/// One consistent database state: the main file with WAL frames layered on top.
/// A layer maps page number -> byte offset of that page's image inside the WAL file;
/// later layers win.
#[derive(Clone, Copy)]
pub(crate) struct Pages<'a> {
    pub db: &'a File,
    pub wal: Option<&'a File>,
    pub layers: [Option<&'a HashMap<u32, u64>>; 2],
    pub geo: Geometry,
}

impl Pages<'_> {
    pub fn page(&self, pgno: u32) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.geo.page_size];
        let wal_off = self.layers.iter().rev().flatten().find_map(|l| l.get(&pgno));
        match (wal_off, self.wal) {
            (Some(&off), Some(wal)) => {
                if read_at(wal, &mut buf, off)? != buf.len() {
                    return Err(corrupt("WAL frame vanished"));
                }
            }
            // a page past the end of the main file has never been written: zeros
            _ => {
                read_at(self.db, &mut buf, (pgno as u64 - 1) * self.geo.page_size as u64)?;
            }
        }
        Ok(buf)
    }
}

/// Page 1 carries the 100-byte database header before its b-tree header.
fn hdr(pgno: u32) -> usize {
    if pgno == 1 {
        100
    } else {
        0
    }
}

pub(crate) fn page_type(page: &[u8], pgno: u32) -> u8 {
    page.get(hdr(pgno)).copied().unwrap_or(0)
}

pub(crate) fn is_interior(page: &[u8], pgno: u32) -> bool {
    matches!(page_type(page, pgno), INTERIOR_TABLE | INTERIOR_INDEX)
}

/// Child page numbers of an interior page (table or index), right-most child last.
pub(crate) fn children(page: &[u8], pgno: u32) -> Result<Vec<u32>> {
    let h = hdr(pgno);
    let n = be16(page, h + 3)?;
    let mut out = Vec::with_capacity(n + 1);
    for i in 0..n {
        let off = be16(page, h + 12 + 2 * i)?;
        out.push(be32(page, off)?);
    }
    out.push(be32(page, h + 8)?);
    Ok(out)
}

/// Visit every page of the b-tree rooted at `root`, parents before children.
pub(crate) fn scan(pages: &Pages, root: u32, f: &mut dyn FnMut(u32, &[u8]) -> Result<()>) -> Result<()> {
    let mut stack = vec![(root, 0)];
    while let Some((pgno, depth)) = stack.pop() {
        if depth > 64 {
            return Err(corrupt("b-tree too deep (cycle?)"));
        }
        let page = pages.page(pgno)?;
        f(pgno, &page)?;
        if is_interior(&page, pgno) {
            stack.extend(children(&page, pgno)?.into_iter().map(|c| (c, depth + 1)));
        }
    }
    Ok(())
}

/// A cell with its payload still split between the page and an overflow chain.
/// `rowid` is only meaningful on table leaves.
pub(crate) struct Cell<'a> {
    pub rowid: i64,
    /// offset of the payload within the page
    pub off: usize,
    pub size: usize,
    pub local: &'a [u8],
    pub overflow: u32,
}

/// How many payload bytes of a cell live on the b-tree page itself.
fn local_size(g: Geometry, size: usize, table_leaf: bool) -> usize {
    let u = g.usable;
    let x = if table_leaf { u - 35 } else { (u - 12) * 64 / 255 - 23 };
    if size <= x {
        return size;
    }
    let m = (u - 12) * 32 / 255 - 23;
    let k = m + (size - m) % (u - 4);
    if k <= x {
        k
    } else {
        m
    }
}

/// Payload-carrying cells: table leaves (keyed by rowid) and both kinds of index page,
/// whose cells are whole records. Interior table pages carry no payload.
pub(crate) fn cells(page: &[u8], pgno: u32, g: Geometry) -> Result<Vec<Cell<'_>>> {
    let h = hdr(pgno);
    let ty = page_type(page, pgno);
    let (ptrs, skip) = match ty {
        LEAF_TABLE | LEAF_INDEX => (h + 8, 0),
        INTERIOR_INDEX => (h + 12, 4), // cell starts with the left child pointer
        _ => return Ok(vec![]),
    };
    let n = be16(page, h + 3)?;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let mut off = be16(page, ptrs + 2 * i)? + skip;
        let (size, a) = varint(page, off)?;
        off += a;
        let mut rowid = 0;
        if ty == LEAF_TABLE {
            let (r, b) = varint(page, off)?;
            off += b;
            rowid = r;
        }
        let size = size as usize;
        let local = local_size(g, size, ty == LEAF_TABLE);
        let body = page.get(off..off + local).ok_or_else(|| corrupt("cell overruns page"))?;
        let overflow = if local < size { be32(page, off + local)? } else { 0 };
        out.push(Cell { rowid: rowid as i64, off, size, local: body, overflow });
    }
    Ok(out)
}

/// Reassemble a full payload. Also returns the overflow pages it walked, which the
/// tracker indexes so that in-place overflow rewrites can be mapped back to their row.
pub(crate) fn payload(src: &Pages, cell: &Cell) -> Result<(Vec<u8>, Vec<u32>)> {
    let mut data = Vec::with_capacity(cell.size);
    data.extend_from_slice(cell.local);
    let mut chain = Vec::new();
    let mut next = cell.overflow;
    let per = src.geo.usable - 4;
    while data.len() < cell.size {
        if next == 0 || chain.len() > cell.size / per + 1 {
            return Err(corrupt("overflow chain ends early or loops"));
        }
        chain.push(next);
        let p = src.page(next)?;
        let take = per.min(cell.size - data.len());
        data.extend_from_slice(p.get(4..4 + take).ok_or_else(|| corrupt("short overflow page"))?);
        next = be32(&p, 0)?;
    }
    Ok((data, chain))
}

fn int_be(b: &[u8]) -> i64 {
    // sign-extend a 1..8 byte big-endian two's complement integer
    let mut v: i64 = if b[0] & 0x80 != 0 { -1 } else { 0 };
    for &x in b {
        v = (v << 8) | x as i64;
    }
    v
}

/// Decode a record (header of serial types, then bodies) into values.
pub(crate) fn record(b: &[u8], encoding: u32) -> Result<Vec<Value>> {
    let (hlen, mut p) = varint(b, 0)?;
    let hlen = hlen as usize;
    let mut body = hlen;
    let mut out = Vec::new();
    while p < hlen {
        let (t, n) = varint(b, p)?;
        p += n;
        let len = match t {
            0 | 8 | 9 => 0,
            1..=4 => t as usize,
            5 => 6,
            6 | 7 => 8,
            10 | 11 => return Err(corrupt("reserved serial type")),
            _ => (t as usize - 12) / 2,
        };
        let v = b.get(body..body + len).ok_or_else(|| corrupt("record body overrun"))?;
        body += len;
        out.push(match t {
            0 => Value::Null,
            8 => Value::Integer(0),
            9 => Value::Integer(1),
            1..=6 => Value::Integer(int_be(v)),
            7 => Value::Real(f64::from_bits(u64::from_be_bytes(v.try_into().unwrap()))),
            t if t % 2 == 0 => Value::Blob(v.to_vec()),
            _ => Value::Text(text(v, encoding)),
        });
    }
    Ok(out)
}

fn text(v: &[u8], encoding: u32) -> String {
    let units = |f: fn([u8; 2]) -> u16| -> Vec<u16> { v.chunks_exact(2).map(|c| f([c[0], c[1]])).collect() };
    match encoding {
        2 => String::from_utf16_lossy(&units(u16::from_le_bytes)),
        3 => String::from_utf16_lossy(&units(u16::from_be_bytes)),
        _ => String::from_utf8_lossy(v).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints() {
        assert_eq!(varint(&[0x7f], 0).unwrap(), (0x7f, 1));
        assert_eq!(varint(&[0x81, 0x00], 0).unwrap(), (0x80, 2));
        assert_eq!(varint(&[0xff; 9], 0).unwrap(), (u64::MAX, 9));
        assert!(varint(&[0x81], 0).is_err());
    }

    #[test]
    fn records() {
        // header: len 5, types: int8 (1), text len 2 (17), null (0), one (9)
        let rec = [5, 1, 17, 0, 9, 0xfe, b'h', b'i'];
        assert_eq!(
            record(&rec, 1).unwrap(),
            vec![Value::Integer(-2), Value::Text("hi".into()), Value::Null, Value::Integer(1)]
        );
        assert!(record(&[3, 1, 1, 7], 1).is_err());
    }

    #[test]
    fn local_payload_split() {
        let g = Geometry { page_size: 4096, usable: 4096, encoding: 1 };
        assert_eq!(local_size(g, 4061, true), 4061);
        // spills: M = 489, K = 489 + (5000-489) % 4092 = 908
        assert_eq!(local_size(g, 5000, true), 908);
    }
}
