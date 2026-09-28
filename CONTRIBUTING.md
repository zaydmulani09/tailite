# Contributing

Bug reports with a reproduction are the most valuable contribution. If tailite reported
something SQLite disagrees with, please include the SQL that produced the database (or the
database itself) and the tailite version.

## Working on the code

```sh
cargo test --release          # the randomized workloads are slow in debug builds
cargo clippy --all-targets -- -D warnings
cargo fmt
```

`docs/ARCHITECTURE.md` explains the pipeline and the invariants each module keeps.

Two tools help when chasing a wrong event:

- `TAILITE_VERIFY=1` rebuilds the page index from scratch after every commit and panics
  with the pages that drifted. The randomized tests set it.
- The differential tests in `tests/stream.rs` print the first table, key and row that
  differ from SQLite. `random_workload` takes a seed; add a failing seed as a regression.

Every change to decoding or to the tracker should come with a test that drives real SQLite
into the case and compares against `SELECT`.
