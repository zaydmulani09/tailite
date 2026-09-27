//! Turns "these pages changed" into "these rows changed".
//!
//! A committed transaction is a set of new page images. To name the rows behind them
//! we need to know which table each page belongs to, before and after. SQLite pages
//! carry no back-pointer to their tree, so the tracker keeps page -> table-root and
//! page -> parent maps, built once by walking every table b-tree and then maintained
//! incrementally per transaction:
//!
//! 1. From every changed page, walk up the parent map to its root. Those paths are the
//!    only places the tree's shape can have changed.
//! 2. Walk the same paths top-down through the new images, re-assigning owner and
//!    parent to every child met. Unchanged children off the paths keep their subtrees.
//! 3. Anything that was on a path or under a changed interior page and was not met
//!    again has left the tree: an orphan. SQLite does not rewrite freed pages, so
//!    orphans are usually *not* in the transaction; their old images still hold the
//!    rows that were deleted, and we read those (plus whatever hangs below them).
//!
//! Rows are then collected from the old and new images of changed pages and orphans
//! and diffed by key (rowid, or primary key for WITHOUT ROWID) within each table. Cost is O(changed pages x tree depth) plus
//! the pages that were actually freed, never a table scan.
//!
//! One more index maps overflow pages to the row that owns them: SQLite overwrites a
//! same-size payload in place, so a blob update can change only an overflow page and
//! leave the leaf byte-identical.

use crate::format::{self, Cell, Pages, LEAF_TABLE};
use crate::schema::{self, Schema, Table};
use crate::{Change, Op, Result, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Row identity within a table: the rowid, or for WITHOUT ROWID tables the primary key
/// values (compared through their debug rendering, which is exact for stored values).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Key {
    Rowid(i64),
    Pk(String),
}

/// Where an overflow page's payload lives: (table root, row key, page holding the cell).
type OverflowOwner = (u32, Key, u32);

pub(crate) struct Tracker {
    pub schema: Schema,
    // ponytail: three hash entries per b-tree page; a 10 GB database costs ~150 MB here.
    // Dense arrays keyed by page number would cut that if it ever matters.
    owner: HashMap<u32, u32>,
    parent: HashMap<u32, u32>,
    overflow: HashMap<u32, OverflowOwner>,
}

/// One state of the database as seen by the row collector.
struct Side<'a> {
    pages: &'a Pages<'a>,
    schema: &'a Schema,
    owner: &'a dyn Fn(u32) -> Option<u32>,
}

impl Side<'_> {
    fn table(&self, pgno: u32) -> Option<&Table> {
        (self.owner)(pgno).and_then(|root| self.schema.by_root(root))
    }
}

struct Row {
    values: Vec<Value>,
    chain: Vec<u32>,
    leaf: u32,
}

/// table name -> key -> row
type Rows = HashMap<String, BTreeMap<Key, Row>>;

impl Tracker {
    /// Walk every rowid table (and the schema table itself) from its root.
    pub fn build(pages: &Pages) -> Result<Tracker> {
        let schema = schema::load(pages)?;
        let mut t = Tracker { owner: HashMap::new(), parent: HashMap::new(), overflow: HashMap::new(), schema };
        let roots: Vec<u32> = std::iter::once(1).chain(t.schema.tables.iter().map(|t| t.root)).collect();
        for root in roots {
            let table = t.schema.by_root(root).cloned();
            format::scan(pages, root, &mut |pgno, page| {
                t.owner.insert(pgno, root);
                if format::is_interior(page, pgno) {
                    for c in format::children(page, pgno)? {
                        t.parent.insert(c, pgno);
                    }
                }
                for cell in format::cells(page, pgno, pages.geo)? {
                    if cell.overflow != 0 {
                        let key = match &table {
                            Some(tb) => decode(pages, tb, &cell)?.0,
                            None => Key::Rowid(cell.rowid), // sqlite_schema
                        };
                        for o in format::payload(pages, &cell)?.1 {
                            t.overflow.insert(o, (root, key.clone(), pgno));
                        }
                    }
                }
                Ok(())
            })?;
        }
        Ok(t)
    }

    /// Apply one committed transaction. `old` is the state before it, `new` the state
    /// after it, `changed` the pages it wrote.
    pub fn apply(&mut self, old: &Pages, new: &Pages, changed: &HashSet<u32>) -> Result<Vec<Change>> {
        if changed.contains(&1) && schema::cookie(&new.page(1)?) != self.schema.cookie {
            return self.rebuild(old, new, changed);
        }

        // 1. paths from every changed tracked page up to its root
        let mut path = HashSet::new();
        let mut roots = HashSet::new();
        for &p in changed.iter().filter(|p| self.owner.contains_key(p)) {
            let mut q = p;
            while path.insert(q) {
                match self.parent.get(&q) {
                    Some(&up) => q = up,
                    None if self.owner.get(&q) == Some(&q) => {
                        roots.insert(q);
                    }
                    // a page with an owner but no way up: the maps lost track. Heal.
                    None => return self.rebuild(old, new, changed),
                }
            }
        }

        // 2. re-walk those paths top-down through the new images
        let mut assign: HashMap<u32, u32> = roots.iter().map(|&r| (r, r)).collect();
        let mut new_parent: HashMap<u32, u32> = HashMap::new();
        let mut stack: Vec<u32> = roots.iter().copied().collect();
        while let Some(q) = stack.pop() {
            let t = assign[&q];
            let img = new.page(q)?;
            if !format::is_interior(&img, q) {
                continue;
            }
            let rewritten = changed.contains(&q);
            for c in format::children(&img, q)? {
                let follow = changed.contains(&c) || path.contains(&c);
                // an unchanged page still has its old children: only re-walk the paths
                if !rewritten && !follow {
                    continue;
                }
                new_parent.insert(c, q);
                if assign.insert(c, t).is_none() && follow {
                    stack.push(c);
                }
            }
        }

        // 3. orphans: path pages and old children of changed interior pages that were
        //    not met again, closed over their old subtrees
        let mut todo: Vec<u32> = path.iter().copied().collect();
        for &p in changed.iter().filter(|p| self.owner.contains_key(p)) {
            let img = old.page(p)?;
            if format::is_interior(&img, p) {
                todo.extend(format::children(&img, p)?);
            }
        }
        let mut orphans = HashSet::new();
        while let Some(p) = todo.pop() {
            if assign.contains_key(&p) || !orphans.insert(p) {
                continue;
            }
            let img = old.page(p)?;
            if format::is_interior(&img, p) {
                todo.extend(format::children(&img, p)?);
            }
        }

        let owner_old = |p: u32| self.owner.get(&p).copied();
        let owner_new = |p: u32| match assign.get(&p) {
            Some(&t) => Some(t),
            None if orphans.contains(&p) || changed.contains(&p) => None,
            None => self.owner.get(&p).copied(),
        };
        let before = Side { pages: old, schema: &self.schema, owner: &owner_old };
        let after = Side { pages: new, schema: &self.schema, owner: &owner_new };
        let scope: HashSet<u32> = changed.union(&orphans).copied().collect();
        let (old_rows, new_rows) = self.collect(&before, &after, &scope)?;

        // 4. maintain the indexes
        for p in &orphans {
            self.owner.remove(p);
            self.parent.remove(p);
        }
        self.owner.extend(assign);
        self.parent.extend(new_parent);
        for rows in old_rows.values() {
            for o in rows.values().flat_map(|r| &r.chain) {
                self.overflow.remove(o);
            }
        }
        for (name, rows) in &new_rows {
            let root = self.schema.tables.iter().find(|t| &t.name == name).map_or(0, |t| t.root);
            for (key, row) in rows {
                for &o in &row.chain {
                    self.overflow.insert(o, (root, key.clone(), row.leaf));
                }
            }
        }
        if std::env::var_os("TAILITE_VERIFY").is_some() {
            self.verify(new);
        }
        Ok(diff(&self.schema, &self.schema, old_rows, new_rows))
    }

    /// Every row of every tracked table in this state, as inserts.
    pub fn snapshot(&self, pages: &Pages) -> Result<Vec<Change>> {
        let owner = |p: u32| self.owner.get(&p).copied();
        let side = Side { pages, schema: &self.schema, owner: &owner };
        let mut rows = Rows::new();
        for &p in self.owner.keys() {
            leaf_rows(&side, p, &pages.page(p)?, None, &HashSet::new(), &mut rows)?;
        }
        Ok(diff(&self.schema, &self.schema, Rows::new(), rows))
    }

    /// Debug aid (`TAILITE_VERIFY=1`): compare the incrementally maintained indexes with
    /// a full rebuild after every transaction. Slow; for tests and bug reports.
    fn verify(&self, pages: &Pages) {
        let fresh = Tracker::build(pages).expect("rebuild for verification");
        for (name, a, b) in [("owner", &self.owner, &fresh.owner), ("parent", &self.parent, &fresh.parent)] {
            let mut drift: Vec<_> =
                a.keys().chain(b.keys()).filter(|p| a.get(p) != b.get(p)).map(|p| (*p, a.get(p), b.get(p))).collect();
            drift.sort();
            drift.dedup();
            assert!(drift.is_empty(), "{name} map drift (page, have, want): {drift:?}");
        }
        assert!(self.overflow == fresh.overflow, "overflow map drift");
    }

    /// Schema changed: rebuild every index from the new state and diff the pages whose
    /// owner or content moved. Rare (DDL, VACUUM), and O(database) by design.
    pub fn rebuild(&mut self, old: &Pages, new: &Pages, changed: &HashSet<u32>) -> Result<Vec<Change>> {
        let fresh = Tracker::build(new)?;
        let mut scope = changed.clone();
        for (p, t) in &self.owner {
            if fresh.owner.get(p) != Some(t) {
                scope.insert(*p);
            }
        }
        let owner_old = |p: u32| self.owner.get(&p).copied();
        let owner_new = |p: u32| fresh.owner.get(&p).copied();
        let before = Side { pages: old, schema: &self.schema, owner: &owner_old };
        let after = Side { pages: new, schema: &fresh.schema, owner: &owner_new };
        let (old_rows, new_rows) = self.collect(&before, &after, &scope)?;
        let mut changes = diff(&self.schema, &fresh.schema, old_rows, new_rows);
        // rows of dropped tables are not reported as deletes
        changes.retain(|c| c.op != Op::Delete || fresh.schema.tables.iter().any(|t| t.name == c.table));
        *self = fresh;
        Ok(changes)
    }

    /// Rows on the old and new images of `scope` (a side skips pages it does not own),
    /// plus rows whose overflow pages were rewritten in place under an unchanged leaf.
    fn collect(&self, before: &Side, after: &Side, scope: &HashSet<u32>) -> Result<(Rows, Rows)> {
        let mut old_rows = Rows::new();
        let mut new_rows = Rows::new();
        for &p in scope {
            let (a, b) = (before.pages.page(p)?, after.pages.page(p)?);
            // A cell that is byte-identical on both images of a page of the same table is
            // an untouched row: skip decoding it. Cells with an overflow chain are always
            // decoded, since SQLite can rewrite the chain in place under the same cell.
            let same = |p| before.table(p).is_some_and(|t| after.table(p).is_some_and(|u| t.name == u.name));
            let mut unchanged = HashSet::new();
            if a != b && same(p) {
                let geo = before.pages.geo;
                let old: HashMap<usize, (i64, &[u8])> = format::cells(&a, p, geo)?
                    .into_iter()
                    .filter(|c| c.overflow == 0)
                    .map(|c| (c.off, (c.rowid, c.local)))
                    .collect();
                for c in format::cells(&b, p, geo)? {
                    if c.overflow == 0 && old.get(&c.off) == Some(&(c.rowid, c.local)) {
                        unchanged.insert(c.off);
                    }
                }
            }
            leaf_rows(before, p, &a, None, &unchanged, &mut old_rows)?;
            leaf_rows(after, p, &b, None, &unchanged, &mut new_rows)?;
        }
        for p in scope {
            if let Some((_, key, leaf)) = self.overflow.get(p) {
                if !scope.contains(leaf) {
                    let none = HashSet::new();
                    leaf_rows(before, *leaf, &before.pages.page(*leaf)?, Some(key), &none, &mut old_rows)?;
                    leaf_rows(after, *leaf, &after.pages.page(*leaf)?, Some(key), &none, &mut new_rows)?;
                }
            }
        }
        Ok((old_rows, new_rows))
    }
}

fn decode(pages: &Pages, table: &Table, cell: &Cell) -> Result<(Key, Vec<Value>, Vec<u32>)> {
    let (data, chain) = format::payload(pages, cell)?;
    let rec = format::record(&data, pages.geo.encoding)?;
    let key = match table.without_rowid {
        true => Key::Pk(format!("{:?}", &rec[..table.pk.len().min(rec.len())])),
        false => Key::Rowid(cell.rowid),
    };
    Ok((key, table.row(cell.rowid, rec), chain))
}

/// Decode the rows stored on `pgno` if, on this side, it belongs to a tracked table:
/// table leaves for rowid tables, every page of a WITHOUT ROWID table's index b-tree.
fn leaf_rows(
    side: &Side,
    pgno: u32,
    img: &[u8],
    only: Option<&Key>,
    skip: &HashSet<usize>,
    out: &mut Rows,
) -> Result<()> {
    let Some(table) = side.table(pgno) else {
        return Ok(());
    };
    if (format::page_type(img, pgno) == LEAF_TABLE) == table.without_rowid {
        return Ok(());
    }
    let rows = out.entry(table.name.clone()).or_default();
    for cell in format::cells(img, pgno, side.pages.geo)? {
        if only.is_some_and(|k| *k != Key::Rowid(cell.rowid)) && !table.without_rowid || skip.contains(&cell.off) {
            continue;
        }
        let (key, values, chain) = decode(side.pages, table, &cell)?;
        if only.is_some_and(|k| *k != key) {
            continue;
        }
        rows.insert(key, Row { values, chain, leaf: pgno });
    }
    Ok(())
}

fn diff(old_schema: &Schema, new_schema: &Schema, mut old_rows: Rows, mut new_rows: Rows) -> Vec<Change> {
    let find = |s: &'_ Schema, name: &str| -> Option<Table> { s.tables.iter().find(|t| t.name == name).cloned() };
    let mut names: Vec<String> = old_rows.keys().chain(new_rows.keys()).cloned().collect();
    names.sort();
    names.dedup();
    let mut out = vec![];
    for name in names {
        let before = old_rows.remove(&name).unwrap_or_default();
        let mut after = new_rows.remove(&name).unwrap_or_default();
        let columns =
            find(new_schema, &name).or_else(|| find(old_schema, &name)).map(|t| t.columns).unwrap_or_default();
        let mut ops: BTreeMap<Key, Change> = BTreeMap::new();
        let rowid = |k: &Key| match k {
            Key::Rowid(r) => Some(*r),
            Key::Pk(_) => None,
        };
        for (key, b) in before {
            let change = match after.remove(&key) {
                Some(a) if a.values == b.values => continue,
                Some(a) => (Op::Update, Some(b.values), Some(a.values)),
                None => (Op::Delete, Some(b.values), None),
            };
            let c = Change {
                table: name.clone(),
                op: change.0,
                rowid: rowid(&key),
                columns: columns.clone(),
                before: change.1,
                after: change.2,
            };
            ops.insert(key, c);
        }
        for (key, a) in after {
            let c = Change {
                table: name.clone(),
                op: Op::Insert,
                rowid: rowid(&key),
                columns: columns.clone(),
                before: None,
                after: Some(a.values),
            };
            ops.insert(key, c);
        }
        out.extend(ops.into_values());
    }
    out
}
