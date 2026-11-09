# fsql

> SQL over the filesystem.

[![fsql on crates.io](https://img.shields.io/crates/v/fsql?label=fsql&logo=rust&labelColor=B7410E&color=black)][crates-fsql] [![fsql-sqlparser on crates.io](https://img.shields.io/crates/v/fsql-sqlparser?label=fsql-sqlparser&logo=rust&labelColor=B7410E&color=black)][crates-fsql-sqlparser] [![License: MIT + Apache-2.0](https://img.shields.io/badge/license-MIT%20%2B%20Apache--2.0-blue)](#license)

<!--[![NPM](https://img.shields.io/npm/v/fsql?logo=npm&labelColor=CB3837&color=black)](https://npm.im/fsql)-->

Query with `SELECT`, change with `DELETE`, `UPDATE` and `INSERT`.

Mutations preview by default and run only with `--apply`. fsql journals every applied statement, and `fsql undo` reverses it.

Find the ten largest files with permissions, exact and readable sizes, and whole-tree totals:

```sh
fsql -C ~/projects --from-file crates/fsql/examples/queries/largest-files.fsql
```

<details open><summary><tt>largest-files.fsql</tt></summary>

```sql
-- Keep only the ten largest files; count the whole tree in a separate cheap scan.
-- Directory totals include the scan root. Symlinks are neither files nor dirs.
WITH largest AS (
    SELECT path, size, perms
    FROM files
    WHERE kind = 'file'
    ORDER BY size DESC, path
    LIMIT 10
), totals AS (
    SELECT
        coalesce(sum(CAST(kind = 'file' AS INT)), 0) AS total_files,
        coalesce(sum(CAST(kind = 'dir' AS INT)), 0) AS total_dirs
    FROM files
)
SELECT
    largest.path,
    human(largest.size) AS readable_size,
    largest.size,
    largest.perms,
    totals.total_files,
    totals.total_dirs
FROM totals LEFT JOIN largest ON true
ORDER BY largest.size DESC, largest.path;
```

</details>

The [query file] sorts on the numeric `size` and calls `human(size)` only for display. More runnable reports cover [executables, permissions, duplicate sizes, and directory usage].

```sh
fsql -e "select path, human(size) from files where size > 1g order by size desc limit 20"
fsql -C ~/projects -e "select ext, count(*), sum(size) from files group by ext order by 3 desc"
fsql -e "delete from files where ext = 'tmp' and mtime < now() - interval '30' day"
fsql --apply -e "delete from files where ext = 'tmp' and mtime < now() - interval '30' day"
fsql undo 18d32e8f3c07b302-129aef
```

## Install

```sh
cargo install fsql
```

## Input

```sh
fsql -e SQL           # one or more statements from the argument, repeatable
fsql --from-file FILE # statements loaded from a file
fsql -F FILE          # short form of --from-file
fsql --from-file -    # statements loaded from stdin
fsql FILE             # statements from a file
fsql -                # statements from stdin
fsql <script.fsql     # same
```

`--from-file` and the positional `FILE` are alternatives; use one per invocation. `-f` selects the output format, for example `-f json`.

`-C DIR` sets the directory the `files` table starts from. The default is the current directory. `files('/some/dir')` sets it per query.

For editor highlighting of `.fsql` query files, use the bundled [Zed extension] or [tree-sitter grammar].

[Zed extension]: editors/zed/README.md
[tree-sitter grammar]: tree-sitter-fsql/README.md

## Tables

`files` has one row per directory entry below the root, the root included. Symbolic links appear as rows, and fsql never follows them.

| column        | type      | source   |
| ------------- | --------- | -------- |
| path          | text      | walk     |
| relative_path | text      | walk     |
| name          | text      | walk     |
| parent        | text      | walk     |
| ext           | text      | walk     |
| depth         | int       | walk     |
| hidden        | bool      | walk     |
| kind          | text      | dirent   |
| size          | int       | statx    |
| blocks        | int       | statx    |
| mode          | int       | statx    |
| perms         | text      | statx    |
| setuid        | bool      | statx    |
| setgid        | bool      | statx    |
| sticky        | bool      | statx    |
| uid           | int       | statx    |
| gid           | int       | statx    |
| user          | text      | passwd   |
| group         | text      | group    |
| nlink         | int       | statx    |
| inode         | int       | statx    |
| dev           | int       | statx    |
| atime         | timestamp | statx    |
| mtime         | timestamp | statx    |
| ctime         | timestamp | statx    |
| btime         | timestamp | statx    |
| target        | text      | readlink |
| broken        | bool      | statx    |

`kind` is one of `file`, `dir`, `symlink`, `fifo`, `socket`, `block`, `char`.

`relative_path` is relative to the root given to `files(...)`, or to the default query root. It uses `/` separators and is empty for the root row. It keeps that meaning when a path predicate narrows the scan. Like `path`, it preserves non-UTF-8 filenames as byte values.

`GLOB ANY (ARRAY[...])` matches at least one pattern, with the same brace expansion and separator semantics as `GLOB`:

```sql
SELECT relative_path
FROM files('.')
WHERE kind IN ('file', 'symlink')
  AND NOT (relative_path GLOB ANY (ARRAY[
    '**/{.git,node_modules,target}{,/**}',
    '{build,dist}{,/**}'
  ]));
```

`GLOB SOME` is an alias for `GLOB ANY`; `GLOB ALL` requires every pattern to match. Each accepts an array or a one-column subquery, including correlated subqueries. For example, `name GLOB ANY (SELECT pattern FROM patterns)` uses a query-defined pattern collection.

Constant string arrays for `ANY`/`SOME` compile into a cached glob set without rebuilding pattern strings for each row. fsql evaluates dynamic array elements for each row. Empty arrays and empty subqueries give false for `ANY` and true for `ALL`, even for a NULL subject. Otherwise NULL comparisons propagate unless a match decides `ANY` or a nonmatch decides `ALL`.

The public `WalkOptions` struct holds only traversal settings. Pruning is an internal scan policy. `Entry::relative_path()` returns the original source-relative path without rebuilding it on every call. `SELECT *` appends `relative_path` after the other columns, and their positions stay the same.

For a single `files` source, the optimizer can prune traversal for mandatory `NOT (relative_path GLOB ...)` and `NOT (relative_path GLOB ANY (...))` filters. It recognizes provable subtree patterns ending in `{,/**}` or `/**` and leaves ambiguous wildcard or escape cases to ordinary row filtering. Pruning respects left-to-right `AND` evaluation. An exclusion never skips an earlier expression that might fail. Only proven infallible predecessors, such as `depth > 0`, can precede a prunable exclusion. The optimizer leaves metadata predicates and arithmetic unoptimized. fsql still evaluates the row of the excluded directory itself. Other patterns, filename filters, and exclusions inside `OR` do not trigger pruning. Pruning is a conservative optimization of the SQL and does not read Git's index or ignore files.

fsql reads columns lazily. A query that touches only `path` and `kind` never calls `statx`.

`mounts` lists mounted filesystems: `mountpoint`, `fstype`, `source`, `options`, `readonly`, `dev`, `mnt_id`, `topology`, `transport`, `media`, `case_sensitive`, `remote`.

`xattrs` has one row per extended attribute below the root: `path`, `name`, `value`, `size`.

`acls` has one row per POSIX ACL entry below the root: `path`, `kind`, `tag`, `qualifier`, `perms`. `kind` is `access` or `default`.

## Literals and operators

```sql
size > 1g            -- binary units: k m g t p, kib mib gib tib pib
size > 4gb           -- decimal units: kb mb gb tb pb
mode & 0o111         -- octal
mtime > now() - interval '7' day
mtime > '2026-07-01'
path glob '**/*.tmp'
name regexp '^\.'
name like 'a_c%'     name ilike 'A%'
```

Functions: `now`, `lower`, `upper`, `length`, `trim`, `substr`, `replace`, `starts_with`, `ends_with`, `contains`, `coalesce`, `ifnull`, `nullif`, `abs`, `basename`, `dirname`, `extension`, `human`, `oct`, `typeof`, `cast`, `extract`.\
Aggregates: `count`, `sum`, `avg`, `min`, `max`, `group_concat`.

Comparing values of different types is an error, so `size > 'big'` fails.\
Comparisons, grouping, distinctness and set operations treat `1` and `1.0` as equal.\
Integer and float comparisons preserve large integer precision.

fsql validates names, function arities and grouping before scanning.\
fsql rejects unsupported function modifiers, including window functions.

## Joins, subqueries, CTEs, set operations

```sql
select f.path, m.fstype from files f join mounts m on f.dev = m.dev
select d.name, count(f.path) from files d left join files f on f.parent = d.path group by d.name
select name from files where size = (select max(size) from files)
select name from files p where exists (select 1 from files c where c.parent = p.path and c.ext = 'rs')
with big as (select path, size from files where size > 1g) select * from big order by size desc
with recursive up(path, n) as (select path, 0 from files where name = 'x' union all select dirname(path), n + 1 from up where path <> '/') select * from up
select ext from files except select 'rs'
select * from (values (1, 'a'), (2, 'b')) v
```

fsql supports inner, left, right, full and cross joins with `ON` or `USING`.\
Subqueries may be scalar, `IN`, `EXISTS`, `ANY` or `ALL`, correlated or not.\
fsql supports plain and recursive `WITH`, and `UNION`, `INTERSECT` and `EXCEPT`, each with `ALL`.\
Derived tables and `VALUES` work.

A column reference may carry a table alias as its qualifier.

## Mutations

```sql
delete from files where ext = 'tmp'
update files set mode = 0o644 where ext = 'sh' and mode & 0o111 = 0
update files set name = 'old_' || name where mtime < '2020-01-01'
update files set parent = '/archive' where ext = 'log'
insert into files (path, kind) values ('/tmp/scratch', 'dir')
insert into files (path, kind, target) values ('/tmp/link', 'symlink', 'scratch')
insert into files (path, content) values ('/tmp/note', 'hello')
insert into files (path, source) select path || '.bak', path from files where ext = 'conf'
```

Assignable columns: `path`, `name`, `parent`, `mode`, `uid`, `gid`, `user`, `group`, `atime`, `mtime`, `target`.

`INSERT` takes `path`, `kind`, `mode`, `target`, `uid`, `gid`, `user`, `group`, `atime`, `mtime`, `content` and `source`. `content` holds the bytes to write into a new file. `source` names an existing path to copy. fsql copies the bytes of a source file and the target of a source symlink, and takes the source's kind and mode when the statement does not give them.

A mutation runs in two phases. Resolve walks the tree, evaluates the predicate, and freezes every matching row with its device, inode and ctime. Apply reopens each parent directory component by component without following symlinks and checks the frozen identity. Preflight detects stale targets before the first change, and apply checks each target again. Metadata updates use a pinned object descriptor. Mutation paths must be absolute and cannot contain `..`.

Statements are not filesystem transactions. A failure during apply can follow successful or partial changes, and fsql reports each kind separately. Rename and unlink act on directory entries, and identity checks cannot make them atomic against concurrent namespace changes. Update undo validates the recorded post-change identity and refuses replacement objects. Directory identity checks use device and inode because changing children also changes the directory's ctime.

fsql refuses a statement during planning when:

- `DELETE` or `UPDATE` has no `WHERE` clause
- the `WHERE` clause is always true, such as `1 = 1`

fsql refuses to apply a statement when more rows match than `--cap` allows. The default cap is 10000. Mutation resolution requires a complete scan, including its subqueries.

`DELETE` removes exactly the rows that matched. It removes a directory only if the directory is empty by then. `where name = 'build'` fails on a populated directory, while `where path glob '/x/build*'` removes the tree.

## Journal and undo

Each applied statement writes a journal under `$XDG_DATA_HOME/fsql/journal`. `--journal-dir` or `FSQL_JOURNAL_DIR` overrides that location. fsql prepares inserts privately, including contents and attributes, before they become visible. A delete keeps the original file, empty directory or symlink in a private `.fsql-ID-N` staging directory on the affected filesystem. The staged object keeps its original inode and metadata even when the journal is on another filesystem. Staging stays outside directories the statement removes. Keep these directories until undo completes. fsql reserves their `.fsql-<hex>-<hex>-<decimal>` names and excludes them from scans and mutation destinations. Creating them requires write access to a surviving parent. Metadata-only updates keep their staging in the journal directory.

Every visible step has a durable replay record and an inverse. fsql publishes completion checkpoints atomically after synchronizing filesystem changes. Interrupted operations appear as `[RECOVERY REQUIRED]` in the journal list:

```sh
fsql journal    # list active journals
fsql recover ID # finish prepared entries, or resume an interrupted undo
fsql undo ID    # reverse applied steps, including an interrupted apply
```

Recovery discards incomplete private preparations and finishes entries whose preparation was durably recorded. It does not rerun the SQL or apply rows that had not reached preparation. Undo can reverse a partially applied entry even when its remaining forward step cannot succeed. You can interrupt and retry both commands, and each completed step has a checkpoint. A finished undo keeps its completion records, so a repeated recovery is harmless. `fsql journal` does not list these records. The library exposes the same behavior through `fsql::mutate::{recover, undo}`.

Recovery checks object identity and recorded metadata. It preserves conflicting objects and reports their paths. Resolve the reported conflict, for example by moving an unrelated destination aside, then retry. A journal lock prevents simultaneous apply, recover and undo on that journal. Other processes may still edit the filesystem; these operations do not provide SQL transaction isolation or make an entire statement atomic. Filesystems must support durable synchronization and the required same-filesystem renames.

fsql can still read legacy journals and undo completed legacy operations. Interrupted legacy intents lack the staging evidence that safe replay needs and still require manual reconciliation. Update records without object identity are not eligible for automatic undo. fsql refuses to delete special files when journaling is enabled.

`--no-journal` applies without a journal, and fsql cannot undo the result.

## Output

`-f` accepts `table`, `csv`, `tsv`, `json` and `lines`. `table` is the default, and `json` writes one object per line.

Simple queries stream with `json` and `lines`. Sorting, grouping, distinctness, CTEs and joins may need intermediate storage. An ordered query with a limit keeps only the best `limit + offset` rows when it does not ask for distinct rows. Joins materialize requested columns; `USING` equijoins index their matching keys.

Filesystem errors stop queries by default. `--best-effort` permits partial SELECT results, prints diagnostics and exits with status 1 when entries were skipped; it does not relax mutation resolution. A streaming query can emit rows before a later failure, so consumers must check its exit status.

Execution defaults to budgets of 1,000,000 cumulative retained row allocations, 256 MiB of estimated retained value allocations, and 10,000,000 work units. Use `--max-rows`, `--max-bytes`, `--max-work` and `--timeout SECONDS` to configure them. Allocation budgets include intermediate results. They are conservative estimates of cumulative allocations and do not limit process RSS. fsql checks cancellation and timeouts between operations and cannot interrupt a blocked filesystem syscall.

## Library

`Engine` returns opaque prepared queries and resolved mutations. Each one captures the execution policy at preparation time. The mutation cap also applies to library callers. Low-level modules remain available for callers that manage their own plans and policies.

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

`PreparedQuery::collect` returns a result set and the same completion report. `ExecutionOptions` also supplies a clonable cancellation token. Streaming callbacks can return `ControlFlow::Break(())` to stop consuming rows.

`Engine::resolve_mutation` produces an inspectable `ResolvedMutation`; its consuming `apply(journal_base)` method creates the journal. Disabling recovery requires the explicit `apply_without_journal` method. Apply outcomes include completed entries, failures, partial changes and recovery-required journals.

The [runnable examples] include query files and complete Rust programs that collect results, stream rows, and apply and undo a journaled mutation.

## Build

```sh
cargo build --release
cargo test --workspace
```

The [benchmark guide](benchmarks/README.md) covers repeatable CLI comparisons against `find`, `fd`, and `ls`, Criterion engine benchmarks, and comparisons between versions and queries. [CONTRIBUTING.md](CONTRIBUTING.md) lists the development commands.

[`vendor/sqlparser`] is a patched copy of [`sqlparser-rs`], described in `vendor/README.md`. fsql uses [`tree-fucker`] 0.1.0 from crates.io for the one-shot `Scan` that performs the `files` traversal. The scan lists each directory directly, never follows a symbolic link, and applies the mount-crossing policy at every domain boundary. `-x` therefore stays on the root's filesystem, while a plain walk crosses into mounts beneath the root. Rows stream as each directory's listing completes. tree-fucker's process-wide resource governor limits traversal operations, and the CLI configures that governor with one full worker of foreground time. fsql reads metadata lazily with its own `statx` and `readlinkat` calls. These reads use the listed directory's descriptor while an anchor is available, and an absolute path once the bounded anchor allowance runs out. The traversal governor does not limit them. The same library backs the `mounts` probe. Cancelling a query also cancels active scans. Resource-limit and quarantine events stay fatal in best-effort mode.

## License

| Part                                  | License                                                                                                   |
| ------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `fsql`, the Zed extension             | MIT, [`LICENSE`](LICENSE) and [`editors/zed/LICENSE`](editors/zed/LICENSE)                                |
| `fsql-sqlparser` (`vendor/sqlparser`) | Apache-2.0, [`LICENSE.TXT`](vendor/sqlparser/LICENSE.TXT) and [`NOTICE.TXT`](vendor/sqlparser/NOTICE.TXT) |
| `tree-sitter-fsql`                    | MIT, copyright Derek Stride, [`tree-sitter-fsql/LICENSE`](tree-sitter-fsql/LICENSE)                       |

[`vendor/sqlparser`]: ./vendor/sqlparser/
[`sqlparser-rs`]: https://github.com/apache/datafusion-sqlparser-rs
[`tree-fucker`]: https://crates.io/crates/tree-fucker

[query file]: crates/fsql/examples/queries/largest-files.fsql
[executables, permissions, duplicate sizes, and directory usage]: examples/README.md
[runnable examples]: examples/README.md

[crates-fsql]: https://crates.io/crates/fsql
[crates-fsql-sqlparser]: https://crates.io/crates/fsql-sqlparser
