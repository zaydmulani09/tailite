use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;
use tailite::{Change, Op, Value};

const USAGE: &str = "\
tailite: row-level change data capture for SQLite, read from the WAL

USAGE:
    tailite watch <db> [--json] [--table <name>]... [--interval <ms>]
    tailite diff <old.db> <new.db> [--json] [--table <name>]...

watch  Follow a live database (WAL mode) and print every committed row change
       with before/after values. The writing application needs no changes.
diff   Print the row changes that turn one database file into another.

OPTIONS:
    --json             One JSON object per changed row (JSON Lines)
    --table <name>     Only report this table (repeatable)
    --interval <ms>    Poll interval for watch [default: 100]
";

struct Opts {
    files: Vec<String>,
    json: bool,
    tables: Vec<String>,
    interval: u64,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { files: vec![], json: false, tables: vec![], interval: 100 };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--json" => o.json = true,
            "--table" => o.tables.push(val("--table")?),
            "--interval" => o.interval = val("--interval")?.parse().map_err(|_| "--interval takes milliseconds")?,
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => o.files.push(a.clone()),
        }
    }
    Ok(o)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("watch") => parse(&args[1..]).and_then(watch),
        Some("diff") => parse(&args[1..]).and_then(diff),
        Some("--version" | "-V") => {
            println!("tailite {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        _ => Err(format!("expected a command\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tailite: {e}");
            ExitCode::FAILURE
        }
    }
}

fn watch(o: Opts) -> Result<(), String> {
    let [db] = o.files.as_slice() else { return Err("watch takes exactly one database".into()) };
    let mut tail = tailite::Tail::open(db).map_err(|e| e.to_string())?;
    for t in tail.skipped_tables() {
        eprintln!("tailite: note: {t} is a WITHOUT ROWID table and is not followed");
    }
    eprintln!("tailite: following {db}");
    let mut out = io::stdout().lock();
    loop {
        for tx in tail.poll().map_err(|e| e.to_string())? {
            for c in tx.changes.iter().filter(|c| o.tables.is_empty() || o.tables.contains(&c.table)) {
                if print(&mut out, &o, Some(tx.seq), c).is_err() {
                    return Ok(()); // stdout closed (e.g. piped into head)
                }
            }
        }
        std::thread::sleep(Duration::from_millis(o.interval));
    }
}

fn diff(o: Opts) -> Result<(), String> {
    let [a, b] = o.files.as_slice() else { return Err("diff takes two databases".into()) };
    let mut out = io::stdout().lock();
    for c in tailite::diff(a, b).map_err(|e| e.to_string())? {
        if (o.tables.is_empty() || o.tables.contains(&c.table)) && print(&mut out, &o, None, &c).is_err() {
            break;
        }
    }
    Ok(())
}

fn print(out: &mut impl Write, o: &Opts, tx: Option<u64>, c: &Change) -> io::Result<()> {
    if o.json {
        writeln!(out, "{}", json(tx, c))?;
    } else {
        writeln!(out, "{}", human(tx, c))?;
    }
    out.flush()
}

fn human(tx: Option<u64>, c: &Change) -> String {
    let short = |v: &Value| {
        let s = v.to_string();
        if s.chars().count() > 60 {
            format!("{}… ({} bytes)", s.chars().take(57).collect::<String>(), s.len())
        } else {
            s
        }
    };
    let row = |vals: &[Value]| c.columns.iter().zip(vals).map(|(k, v)| format!("{k}={}", short(v))).collect::<Vec<_>>().join(" ");
    let head = format!("{}{} {} rowid={}", tx.map(|t| format!("tx {t}  ")).unwrap_or_default(), c.table, op(c.op).to_uppercase(), c.rowid);
    match (c.op, &c.before, &c.after) {
        (Op::Update, Some(b), Some(a)) => {
            let changed: Vec<String> = c
                .columns
                .iter()
                .zip(b.iter().zip(a))
                .filter(|(_, (x, y))| x != y)
                .map(|(k, (x, y))| format!("{k}: {} → {}", short(x), short(y)))
                .collect();
            format!("{head}  {}", changed.join(", "))
        }
        (_, _, Some(a)) => format!("{head}  {}", row(a)),
        (_, Some(b), None) => format!("{head}  {}", row(b)),
        _ => head,
    }
}

fn op(op: Op) -> &'static str {
    match op {
        Op::Insert => "insert",
        Op::Update => "update",
        Op::Delete => "delete",
    }
}

fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json_value(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Integer(i) => i.to_string(),
        Value::Real(f) if f.is_finite() => format!("{f:?}"),
        Value::Real(f) => json_str(&f.to_string()),
        Value::Text(s) => json_str(s),
        Value::Blob(b) => format!("{{\"$blob\":\"{}\"}}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
    }
}

fn json(tx: Option<u64>, c: &Change) -> String {
    let obj = |vals: &Option<Vec<Value>>| match vals {
        None => "null".to_string(),
        Some(v) => format!(
            "{{{}}}",
            c.columns.iter().zip(v).map(|(k, v)| format!("{}:{}", json_str(k), json_value(v))).collect::<Vec<_>>().join(",")
        ),
    };
    format!(
        "{{{}\"table\":{},\"op\":\"{}\",\"rowid\":{},\"before\":{},\"after\":{}}}",
        tx.map(|t| format!("\"tx\":{t},")).unwrap_or_default(),
        json_str(&c.table),
        op(c.op),
        c.rowid,
        obj(&c.before),
        obj(&c.after)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escapes_and_types() {
        let c = Change {
            table: "t\"q".into(),
            op: Op::Update,
            rowid: 7,
            columns: vec!["a".into(), "b".into(), "c".into()],
            before: Some(vec![Value::Text("x\ny".into()), Value::Real(1.0), Value::Blob(vec![0, 255])]),
            after: Some(vec![Value::Null, Value::Real(f64::INFINITY), Value::Integer(-3)]),
        };
        assert_eq!(
            json(Some(2), &c),
            r#"{"tx":2,"table":"t\"q","op":"update","rowid":7,"before":{"a":"x\ny","b":1.0,"c":{"$blob":"00ff"}},"after":{"a":null,"b":"inf","c":-3}}"#
        );
    }
}
