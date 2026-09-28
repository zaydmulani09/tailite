//! The binary, end to end: `diff` output in both formats and argument errors.

use rusqlite::Connection;
use std::process::Command;

fn tailite(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tailite")).args(args).output().unwrap();
    (out.status.success(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
}

#[test]
fn diff_human_and_json() {
    let dir = std::env::temp_dir().join(format!("tailite-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (a, b) = (dir.join("a.db"), dir.join("b.db"));
    let c = Connection::open(&a).unwrap();
    c.execute_batch("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, bio BLOB); INSERT INTO users VALUES (1, 'ada', NULL), (2, 'bob', NULL);").unwrap();
    c.execute(&format!("VACUUM INTO '{}'", b.display()), []).unwrap();
    let d = Connection::open(&b).unwrap();
    d.execute_batch("UPDATE users SET name = 'Ada \"L\"' WHERE id = 1; DELETE FROM users WHERE id = 2; INSERT INTO users VALUES (3, 'cy', x'00ff');").unwrap();
    drop((c, d));
    let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());

    let (ok, out, _) = tailite(&["diff", a, b]);
    assert!(ok);
    assert_eq!(
        out.lines().collect::<Vec<_>>(),
        [
            "users UPDATE rowid=1  name: 'ada' → 'Ada \"L\"'",
            "users DELETE rowid=2  id=2 name='bob' bio=NULL",
            "users INSERT rowid=3  id=3 name='cy' bio=X'00ff'",
        ]
    );

    let (ok, out, _) = tailite(&["diff", a, b, "--json", "--table", "users"]);
    assert!(ok);
    let first = out.lines().next().unwrap();
    assert_eq!(
        first,
        r#"{"table":"users","op":"update","rowid":1,"before":{"id":1,"name":"ada","bio":null},"after":{"id":1,"name":"Ada \"L\"","bio":null}}"#
    );
    let (_, out, _) = tailite(&["diff", a, b, "--table", "nope"]);
    assert!(out.is_empty());
}

#[test]
fn usage_errors() {
    let (ok, _, err) = tailite(&["frobnicate"]);
    assert!(!ok && err.contains("USAGE"));
    let (ok, _, err) = tailite(&["watch", "a.db", "--bogus"]);
    assert!(!ok && err.contains("unknown option --bogus"), "{err}");
    let (ok, out, _) = tailite(&["--version"]);
    assert!(ok && out.starts_with("tailite "));
}
