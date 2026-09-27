//! The table catalogue: `sqlite_schema` rows plus just enough of a CREATE TABLE
//! parser to name columns, find the rowid alias and fill ALTER TABLE ADD COLUMN defaults.

use crate::format::{self, Pages};
use crate::{Result, Value};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Table {
    pub name: String,
    pub root: u32,
    pub columns: Vec<String>,
    /// Value for a column missing from an older, shorter record (ALTER TABLE ADD COLUMN).
    pub defaults: Vec<Value>,
    /// Column that is an alias for the rowid (stored as NULL in the record).
    pub rowid_alias: Option<usize>,
    /// REAL-affinity columns: SQLite stores integral values there as integers on disk
    /// and converts them back when read.
    pub real: Vec<bool>,
    pub without_rowid: bool,
    /// WITHOUT ROWID only: primary key columns, in key order. The record stores
    /// these first, then the remaining columns in declared order.
    pub pk: Vec<usize>,
}

impl Table {
    /// Turn a raw record into a full row in declared column order.
    pub fn row(&self, rowid: i64, mut values: Vec<Value>) -> Vec<Value> {
        if self.without_rowid {
            let order = self.pk.iter().copied().chain((0..self.columns.len()).filter(|i| !self.pk.contains(i)));
            let mut out = self.defaults.clone();
            for (v, i) in values.into_iter().zip(order) {
                out[i] = v;
            }
            values = out;
        } else {
            values.truncate(self.columns.len());
            let have = values.len();
            values.extend(self.defaults[have..].iter().cloned());
        }
        if let Some(i) = self.rowid_alias {
            values[i] = Value::Integer(rowid);
        }
        for (v, &real) in values.iter_mut().zip(&self.real) {
            if let (Value::Integer(n), true) = (&*v, real) {
                *v = Value::Real(*n as f64);
            }
        }
        values
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Schema {
    /// Schema cookie (db header offset 40); bumped by SQLite on every DDL.
    pub cookie: u32,
    /// Tables that hold data (virtual tables have no root page and are skipped).
    pub tables: Vec<Table>,
}

impl Schema {
    pub fn by_root(&self, root: u32) -> Option<&Table> {
        self.tables.iter().find(|t| t.root == root)
    }
}

pub(crate) fn cookie(page1: &[u8]) -> u32 {
    format::be32(page1, 40).unwrap_or(0)
}

pub(crate) fn load(pages: &Pages) -> Result<Schema> {
    let page1 = pages.page(1)?;
    let mut schema = Schema { cookie: cookie(&page1), ..Default::default() };
    let mut rows = Vec::new();
    format::scan(pages, 1, &mut |pgno, page| {
        if format::page_type(page, pgno) == format::LEAF_TABLE {
            for cell in format::cells(page, pgno, pages.geo)? {
                let (data, _) = format::payload(pages, &cell)?;
                rows.push(format::record(&data, pages.geo.encoding)?);
            }
        }
        Ok(())
    })?;
    for r in rows {
        let text = |i: usize| match r.get(i) {
            Some(Value::Text(s)) => s.clone(),
            _ => String::new(),
        };
        let root = match r.get(3) {
            Some(Value::Integer(n)) => *n as u32,
            _ => 0,
        };
        if text(0) != "table" || root == 0 {
            continue;
        }
        let mut t = parse_create_table(&text(4));
        t.name = text(1);
        t.root = root;
        schema.tables.push(t);
    }
    Ok(schema)
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String, bool), // text, was quoted
    Str(String),
    Punct(char),
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let c: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    // read a quoted run closed by `close`, where a doubled `close` is an escape
    let quoted = |i: &mut usize, close: char| {
        let mut s = String::new();
        *i += 1;
        while *i < c.len() {
            if c[*i] == close {
                if close != ']' && c.get(*i + 1) == Some(&close) {
                    s.push(close);
                    *i += 2;
                    continue;
                }
                *i += 1;
                break;
            }
            s.push(c[*i]);
            *i += 1;
        }
        s
    };
    while i < c.len() {
        match c[i] {
            ch if ch.is_whitespace() => i += 1,
            '-' if c.get(i + 1) == Some(&'-') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '/' if c.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            '\'' => out.push(Tok::Str(quoted(&mut i, '\''))),
            '"' => out.push(Tok::Word(quoted(&mut i, '"'), true)),
            '`' => out.push(Tok::Word(quoted(&mut i, '`'), true)),
            '[' => out.push(Tok::Word(quoted(&mut i, ']'), true)),
            ch @ ('(' | ')' | ',' | ';') => {
                out.push(Tok::Punct(ch));
                i += 1;
            }
            _ => {
                let start = i;
                while i < c.len() && !c[i].is_whitespace() && !"()',;\"`[".contains(c[i]) {
                    i += 1;
                }
                out.push(Tok::Word(c[start..i].iter().collect(), false));
            }
        }
    }
    out
}

fn kw(t: Option<&Tok>, k: &str) -> bool {
    matches!(t, Some(Tok::Word(w, false)) if w.eq_ignore_ascii_case(k))
}

const COLUMN_CONSTRAINTS: &[&str] =
    &["CONSTRAINT", "PRIMARY", "NOT", "NULL", "UNIQUE", "CHECK", "DEFAULT", "COLLATE", "REFERENCES", "GENERATED", "AS"];
const TABLE_CONSTRAINTS: &[&str] = &["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];

fn literal(toks: &[Tok]) -> Value {
    match toks {
        [Tok::Str(s), ..] => Value::Text(s.clone()),
        [Tok::Word(x, false), Tok::Str(h), ..] if x.eq_ignore_ascii_case("x") => {
            Value::Blob((0..h.len() / 2).filter_map(|i| u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()).collect())
        }
        [Tok::Word(w, false), ..] => {
            if w.eq_ignore_ascii_case("true") {
                Value::Integer(1)
            } else if w.eq_ignore_ascii_case("false") {
                Value::Integer(0)
            } else if let Ok(n) = w.parse::<i64>() {
                Value::Integer(n)
            } else if let Ok(f) = w.parse::<f64>() {
                Value::Real(f)
            } else {
                Value::Null // ponytail: expression defaults read as NULL; ADD COLUMN only allows constants anyway
            }
        }
        _ => Value::Null,
    }
}

/// Parse the parts of a CREATE TABLE statement that affect how records decode.
pub(crate) fn parse_create_table(sql: &str) -> Table {
    let toks = tokenize(sql);
    let mut t = Table { name: String::new(), root: 0, columns: vec![], defaults: vec![], rowid_alias: None, real: vec![], without_rowid: false, pk: vec![] };
    let Some(open) = toks.iter().position(|x| *x == Tok::Punct('(')) else { return t };
    // split the parenthesised body into top-level comma-separated definitions
    let (mut defs, mut cur, mut depth, mut end) = (vec![], vec![], 0, toks.len());
    for (i, tok) in toks.iter().enumerate().skip(open + 1) {
        match tok {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') if depth == 0 => {
                end = i;
                break;
            }
            Tok::Punct(')') => depth -= 1,
            Tok::Punct(',') if depth == 0 => {
                defs.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(tok.clone());
    }
    defs.push(cur);
    t.without_rowid = toks[end..].windows(2).any(|w| kw(w.first(), "WITHOUT") && kw(w.get(1), "ROWID"));

    let mut types = vec![];
    let mut table_pk: Vec<String> = vec![];
    let mut column_pk = None;
    for def in defs.iter().filter(|d| !d.is_empty()) {
        if TABLE_CONSTRAINTS.iter().any(|k| kw(def.first(), k)) {
            if let Some(p) = def.iter().position(|x| kw(Some(x), "PRIMARY")) {
                table_pk = def[p..]
                    .iter()
                    .skip_while(|x| **x != Tok::Punct('('))
                    .skip(1)
                    .take_while(|x| **x != Tok::Punct(')'))
                    .filter_map(|x| match x {
                        Tok::Word(w, _) if !["ASC", "DESC", "COLLATE"].iter().any(|k| w.eq_ignore_ascii_case(k)) => Some(w.clone()),
                        _ => None,
                    })
                    .collect();
            }
            continue;
        }
        let Tok::Word(name, _) = &def[0] else { continue };
        let cons = def.iter().skip(1).position(|x| COLUMN_CONSTRAINTS.iter().any(|k| kw(Some(x), k))).map_or(def.len(), |p| p + 1);
        let ty: Vec<String> = def[1..cons]
            .iter()
            .filter_map(|x| match x {
                Tok::Word(w, _) => Some(w.to_ascii_uppercase()),
                _ => None,
            })
            .collect();
        let rest = &def[cons..];
        // `[GENERATED ALWAYS] AS (expr) [VIRTUAL|STORED]`: VIRTUAL columns are computed on
        // read and never stored, so they are not part of the record (or of our rows)
        let mut depth = 0;
        let top: Vec<&Tok> = rest
            .iter()
            .filter(|x| {
                match x {
                    Tok::Punct('(') => depth += 1,
                    Tok::Punct(')') => depth -= 1,
                    _ => return depth == 0,
                }
                false
            })
            .collect();
        if top.iter().any(|x| kw(Some(x), "AS")) && !top.iter().any(|x| kw(Some(x), "STORED")) {
            continue;
        }
        let pk =rest.iter().position(|x| kw(Some(x), "PRIMARY"));
        let desc = pk.is_some_and(|p| kw(rest.get(p + 2), "DESC"));
        if pk.is_some() {
            column_pk = Some(t.columns.len());
        }
        if pk.is_some() && !desc && ty.join(" ") == "INTEGER" {
            t.rowid_alias = Some(t.columns.len());
        }
        let default = rest
            .iter()
            .position(|x| kw(Some(x), "DEFAULT"))
            .map(|p| &rest[p + 1..])
            .map(|v| match v {
                [Tok::Punct('('), inner @ ..] => literal(inner),
                [Tok::Word(sign, false), Tok::Word(n, false), ..] if sign == "-" || sign == "+" => literal(&[Tok::Word(format!("{sign}{n}"), false)]),
                _ => literal(v),
            })
            .unwrap_or(Value::Null);
        // affinity rules, in SQLite's order: INT wins, then text and blob, then REAL
        let ty_s = ty.join(" ");
        let has = |k: &str| ty_s.contains(k);
        t.real.push(!has("INT") && !has("CHAR") && !has("CLOB") && !has("TEXT") && !has("BLOB") && (has("REAL") || has("FLOA") || has("DOUB")));
        types.push(ty_s);
        t.columns.push(name.clone());
        t.defaults.push(default);
    }
    if t.rowid_alias.is_none() && table_pk.len() == 1 {
        if let Some(i) = t.columns.iter().position(|c| c.eq_ignore_ascii_case(&table_pk[0])) {
            if types[i] == "INTEGER" {
                t.rowid_alias = Some(i);
            }
        }
    }
    if t.without_rowid {
        t.rowid_alias = None;
        t.pk = match column_pk {
            Some(i) => vec![i],
            None => table_pk.iter().filter_map(|k| t.columns.iter().position(|c| c.eq_ignore_ascii_case(k))).collect(),
        };
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_and_alias() {
        let t = parse_create_table(
            r#"CREATE TABLE "my t" (
                id INTEGER PRIMARY KEY, -- the rowid
                [first name] VARCHAR(20) NOT NULL DEFAULT 'x',
                `n` DECIMAL(10, 2) DEFAULT -1.5,
                b BLOB DEFAULT x'00ff', flag DEFAULT (TRUE),
                CONSTRAINT u UNIQUE (n, b)
            )"#,
        );
        assert_eq!(t.columns, ["id", "first name", "n", "b", "flag"]);
        assert_eq!(t.rowid_alias, Some(0));
        assert_eq!(t.defaults[1], Value::Text("x".into()));
        assert_eq!(t.defaults[2], Value::Real(-1.5));
        assert_eq!(t.defaults[3], Value::Blob(vec![0, 255]));
        assert_eq!(t.defaults[4], Value::Integer(1));
        assert!(!t.without_rowid);
        assert_eq!(parse_create_table("CREATE TABLE t(a REAL, b FLOATING POINT, c DOUBLE PRECISION, d POINT INT, e)").real, [true, false, true, false, false]);
    }

    #[test]
    fn alias_rules() {
        assert_eq!(parse_create_table("CREATE TABLE t(a INT PRIMARY KEY)").rowid_alias, None);
        assert_eq!(parse_create_table("CREATE TABLE t(a INTEGER PRIMARY KEY DESC)").rowid_alias, None);
        assert_eq!(parse_create_table("CREATE TABLE t(x, a INTEGER, PRIMARY KEY(a DESC))").rowid_alias, Some(1));
        assert_eq!(parse_create_table("CREATE TABLE t(a INTEGER, b, PRIMARY KEY(a, b))").rowid_alias, None);
        let w = parse_create_table("CREATE TABLE t(a INTEGER PRIMARY KEY, b) WITHOUT ROWID, STRICT");
        assert!(w.without_rowid);
        assert_eq!(w.rowid_alias, None);
        assert_eq!(w.pk, [0]);
        let w = parse_create_table("CREATE TABLE t(a, b, c, PRIMARY KEY(c, a)) WITHOUT ROWID");
        assert_eq!(w.pk, [2, 0]);
        // stored as c, a, b
        assert_eq!(w.row(0, vec![Value::Integer(3), Value::Integer(1), Value::Integer(2)]), [Value::Integer(1), Value::Integer(2), Value::Integer(3)]);
    }
}
