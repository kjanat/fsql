# fsql

[![Crates.io](https://img.shields.io/crates/v/fsql?logo=rust&labelColor=B7410E&color=black)][crates] [![License: MIT](https://img.shields.io/crates/l/fsql?color=blue)](LICENSE)

`fsql` runs SQL against the filesystem. `SELECT` reads directory entries, mounts, extended attributes and POSIX ACLs. `DELETE`, `UPDATE` and `INSERT` change files, and a journal records each applied statement so it can be undone.

```toml
[dependencies]
fsql = "0.1"
```

## Query

```rust
use std::ops::ControlFlow;

use fsql::walk::WalkOptions;
use fsql::{Engine, ErrorPolicy};

fn main() -> fsql::Result<()> {
    let mut engine = Engine::new(".", WalkOptions::default());
    engine.execution.error_policy = ErrorPolicy::BestEffort;
    let query = engine.prepare_query("select path from files where ext = 'rs'")?;
    let completion = query.stream(&mut |columns, row| {
        println!("{columns:?}: {row:?}");
        Ok(ControlFlow::Continue(()))
    })?;
    assert!(completion.is_complete(), "{:?}", completion.diagnostics);
    Ok(())
}
```

`Engine::prepare_query` validates names, function arities and grouping before any scan. `PreparedQuery::stream` passes rows to a callback, and the callback can return `ControlFlow::Break(())` to stop. `PreparedQuery::collect` returns the whole result set. Both report a `Completion` that lists skipped entries under `ErrorPolicy::BestEffort`.

`ExecutionOptions` holds the row, byte and work budgets, the timeout and a clonable `CancellationToken`.

## Mutate

`Engine::resolve_mutation` walks the tree, evaluates the predicate and returns a `ResolvedMutation` that lists every matching entry with its device, inode and ctime. `ResolvedMutation::apply(journal_base)` checks each entry's identity again before changing it and writes a journal. `apply_without_journal` skips the journal, and its result cannot be undone. `fsql::mutate::undo` and `fsql::mutate::recover` reverse a journaled statement or finish an interrupted one.

The [fsql repository](https://github.com/kjanat/fsql#readme) documents the tables, columns, functions and mutation rules. The [examples](https://github.com/kjanat/fsql/tree/master/crates/fsql/examples) include complete programs that collect results, stream rows, and apply and undo a mutation. The command-line tool is [`fsql-cli`](https://crates.io/crates/fsql-cli).

## License

MIT. See [`LICENSE`](LICENSE).

[crates]: https://crates.io/crates/fsql
