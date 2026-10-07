# Runnable examples

With `fsql` installed, run these commands from the repository root:

```sh
fsql -C crates -F examples/inventory.fsql
fsql -C crates -F examples/by-extension.fsql
```

For an uninstalled checkout, use `cargo run --release -p fsql-cli --` in place of `fsql`.

All reporting queries accept a scan root through `-C`:

| Query                     | What it answers                                                                            |
| ------------------------- | ------------------------------------------------------------------------------------------ |
| [`largest-files`]         | Ten largest files, permissions, exact/readable bytes, and whole-tree file/directory counts |
| [`inventory`]             | Twenty largest files with exact and readable sizes                                         |
| [`by-extension`]          | Counts, total bytes, average size, and largest file per extension                          |
| [`largest-executables`]   | Largest regular files with execute permission bits                                         |
| [`world-writable-recent`] | Regular files modified within seven days with the other-write bit set                      |
| [`duplicate-sizes`]       | Nonempty files sharing a size, with the number of matches                                  |
| [`directory-usage`]       | Largest directories by the apparent bytes of all descendant regular files                  |

```sh
fsql -C ~/projects --from-file crates/fsql/examples/queries/largest-files.fsql
fsql -C ~/projects --from-file examples/largest-executables.fsql
fsql -C ~/projects --from-file examples/world-writable-recent.fsql
fsql -C ~/projects --from-file examples/duplicate-sizes.fsql
fsql -C ~/projects --from-file examples/directory-usage.fsql
```

`largest-files` repeats the whole-tree totals beside each result. Directories include the scan root; symlinks count as neither regular files nor directories. An empty tree still produces one totals row with empty file columns. It performs two traversals: the size query keeps only ten candidates, and the totals query reads entry kinds without requesting metadata. Concurrent filesystem changes can make the two passes observe different states.

The recursive directory report counts each file once for every ancestor within the scan root; it omits directories with no descendant regular files. These are apparent bytes, not allocated disk usage: hard links count per entry, sparse-file holes count toward size, and directory metadata is excluded. It materializes intermediate relations and may need larger execution budgets on deep/large trees. Equal sizes do not imply equal contents, and permission-bit queries do not fully model ACLs or effective access.

For an insert → rename → inspect → delete sequence in a disposable filesystem:

```sh
cargo sandbox:fsql --apply -F "${PWD}/examples/sandbox-mutations.fsql"
```

This Linux example requires Bubblewrap. It prints `renamed.txt` with 15 bytes, then a remaining file count of zero. Each mutation is journaled inside the sandbox. Files and journals disappear when the process exits. Without `--apply`, the CLI previews changes; later statements therefore cannot see earlier changes.

## Rust library

```sh
cargo run -p fsql --example query -- crates
cargo run -p fsql --example stream -- crates
cargo run -p fsql --example mutate_and_undo
```

- [`query`] prepares a query, collects results, checks the completion report, and formats a table.
- [`stream`] sets a work budget and stops after five rows using the callback's `ControlFlow`. Traversal order is unspecified.
- [`mutate_and_undo`] creates its own temporary files, previews a deletion, checks the apply outcome, and verifies that undo restores the contents. It never modifies a user-supplied directory.

SQL lives in the library examples' [`queries`] directory. Rust uses `include_str!` to embed these files at compile time, so the executables also run outside the checkout. The CLI's `--from-file` reads a file at runtime; `-F` is its short form, and `-F -` reads stdin. The existing positional file argument also works.

The first two accept an optional directory argument, defaulting to the current directory. The mutation example removes its fixture and journals on completion. All three also work through the sandbox Cargo configuration; pass an absolute host directory when reading existing files there.

[`queries`]: ../crates/fsql/examples/queries/
[`query`]: ../crates/fsql/examples/query.rs
[`stream`]: ../crates/fsql/examples/stream.rs
[`mutate_and_undo`]: ../crates/fsql/examples/mutate_and_undo.rs

[`largest-files`]: ../crates/fsql/examples/queries/largest-files.fsql
[`inventory`]: inventory.fsql
[`by-extension`]: by-extension.fsql
[`largest-executables`]: largest-executables.fsql
[`world-writable-recent`]: world-writable-recent.fsql
[`duplicate-sizes`]: duplicate-sizes.fsql
[`directory-usage`]: directory-usage.fsql
