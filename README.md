# fsql

SQL over the filesystem. Query with `SELECT`, change with `DELETE`, `UPDATE`
and `INSERT`. Mutations preview by default, run only with `--apply`, and every
applied statement is journaled so it can be undone.

```sh
fsql -e "select path, human(size) from files where size > 1g order by size desc limit 20"
fsql -C ~/projects -e "select ext, count(*), sum(size) from files group by ext order by 3 desc"
fsql -e "delete from files where ext = 'tmp' and mtime < now() - interval '30' day"
fsql --apply -e "delete from files where ext = 'tmp' and mtime < now() - interval '30' day"
fsql undo 18d32e8f3c07b302-129aef
```

## Input

```sh
fsql -e SQL            one or more statements from the argument, repeatable
fsql FILE              statements from a file
fsql -                 statements from stdin
fsql < script.fsql     same
```

`-C DIR` sets the directory the `files` table starts from. The default is the
current directory. `files('/some/dir')` sets it per query.

## Tables

`files` has one row per directory entry below the root, the root included.
Symbolic links are rows and are never followed.

| column | type      | source   |
| ------ | --------- | -------- |
| path   | text      | walk     |
| name   | text      | walk     |
| parent | text      | walk     |
| ext    | text      | walk     |
| depth  | int       | walk     |
| hidden | bool      | walk     |
| kind   | text      | dirent   |
| size   | int       | statx    |
| blocks | int       | statx    |
| mode   | int       | statx    |
| perms  | text      | statx    |
| setuid | bool      | statx    |
| setgid | bool      | statx    |
| sticky | bool      | statx    |
| uid    | int       | statx    |
| gid    | int       | statx    |
| user   | text      | passwd   |
| group  | text      | group    |
| nlink  | int       | statx    |
| inode  | int       | statx    |
| dev    | int       | statx    |
| atime  | timestamp | statx    |
| mtime  | timestamp | statx    |
| ctime  | timestamp | statx    |
| btime  | timestamp | statx    |
| target | text      | readlink |
| broken | bool      | statx    |

`kind` is one of `file`, `dir`, `symlink`, `fifo`, `socket`, `block`, `char`.
Columns are read lazily, so a query touching only `path` and `kind` never
calls `statx`.

`mounts` lists mounted filesystems: `mountpoint`, `fstype`, `source`,
`options`, `readonly`, `dev`, `mnt_id`, `topology`, `transport`, `media`,
`case_sensitive`, `remote`.

`xattrs` has one row per extended attribute below the root: `path`, `name`,
`value`, `size`.

`acls` has one row per POSIX ACL entry below the root: `path`, `kind`
(`access` or `default`), `tag`, `qualifier`, `perms`.

## Literals and operators

```sql
size > 1g            binary units: k m g t p, kib mib gib tib pib
size > 4gb           decimal units: kb mb gb tb pb
mode & 0o111         octal
mtime > now() - interval '7' day
mtime > '2026-07-01'
path glob '**/*.tmp'
name regexp '^\.'
name like 'a_c%'     name ilike 'A%'
```

Functions: `now`, `lower`, `upper`, `length`, `trim`, `substr`, `replace`,
`starts_with`, `ends_with`, `contains`, `coalesce`, `ifnull`, `nullif`, `abs`,
`basename`, `dirname`, `extension`, `human`, `oct`, `typeof`, `cast`,
`extract`. Aggregates: `count`, `sum`, `avg`, `min`, `max`, `group_concat`.

Comparing values of different types is an error rather than a silent
mismatch, so `size > 'big'` fails instead of matching nothing.
Numeric equality is shared by comparisons, grouping, distinctness and set
operations: `1` and `1.0` represent the same key. Integer/float comparisons
preserve large integer precision.

Names, function arities and grouping are validated before scanning. Unsupported
function modifiers, including window functions, are rejected explicitly.

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

Inner, left, right, full and cross joins with `ON` or `USING`. Scalar,
`IN`, `EXISTS`, `ANY` and `ALL` subqueries, correlated or not. Plain and
recursive `WITH`. `UNION`, `INTERSECT`, `EXCEPT`, each with `ALL`. Derived
tables and `VALUES`. Column references may be qualified with a table alias.

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

Assignable columns: `path`, `name`, `parent`, `mode`, `uid`, `gid`, `user`,
`group`, `atime`, `mtime`, `target`.

`INSERT` takes `path`, `kind`, `mode`, `target`, `uid`, `gid`, `user`,
`group`, `atime`, `mtime`, plus `content` (bytes to write into a new file)
and `source` (an existing path to copy: bytes for files, target for symlinks,
kind and mode when not given).

A mutation runs in two phases. Resolve walks the tree, evaluates the
predicate, and freezes every matching row with its device, inode and ctime.
Apply reopens each parent directory component by component without following
symlinks and checks the frozen identity. Preflight detects stale targets before
the first change; each target is checked again during apply. Metadata updates
use a pinned object descriptor. Mutation paths must be absolute and cannot
contain `..`.

Statements are not filesystem transactions. A failure during apply can follow
successful or partial changes, which are reported separately. Rename and unlink
operate on directory entries, so concurrent namespace changes cannot be made
atomic with identity checks. Update undo validates the recorded post-change
identity and refuses replacement objects. Directory identity checks use device
and inode because changing children also changes the directory's ctime.

A statement is refused during planning when:

- `DELETE` or `UPDATE` has no `WHERE` clause
- the `WHERE` clause is always true, such as `1 = 1`

Apply is refused when more rows match than `--cap` allows (default 10000).
Mutation resolution requires a complete scan, including its subqueries.

`DELETE` removes exactly the rows that matched. A directory is removed only if
it is empty by then, so `where name = 'build'` fails on a populated directory
while `where path glob '/x/build*'` removes the tree.

## Journal and undo

Each applied statement writes a journal under `$XDG_DATA_HOME/fsql/journal`
(override with `--journal-dir` or `FSQL_JOURNAL_DIR`). Deleted files are
renamed into the journal, or copied with mode, owner, times and extended
attributes when the journal is on another filesystem. Deleted directories and
links are recorded, updates keep a before-image, inserts record what was
created.

Journals persist an intent before each operation and synchronize completion
records after the filesystem change. A partially created file is recorded for
undo even when a later attribute update fails. Special files that cannot be
recreated are refused when journaling is enabled.

Interrupted operations retain their intent files and appear as
`[RECOVERY REQUIRED]` in the journal list. Automatic undo refuses these journals:
the recorded intent and current filesystem must be reconciled before manual
recovery. Completed undo steps are checkpointed so they are not replayed.
Legacy update records without object identity are also refused by automatic
undo. This is conservative recovery fencing, not automatic crash rollback.

```sh
fsql journal          list journals
fsql undo ID          reverse one journal
```

`--no-journal` applies without any of this and cannot be undone.

## Output

`-f table` (default), `csv`, `tsv`, `json` (one object per line), `lines`.

Simple queries stream with `json` and `lines`. Sorting, grouping, distinctness,
CTEs and joins may need intermediate storage. Ordered queries with a limit
retain only the best `limit + offset` rows when distinctness is not requested.
Joins materialize requested columns; `USING` equijoins index their matching keys.

Filesystem errors stop queries by default. `--best-effort` permits partial
SELECT results, prints diagnostics and exits with status 1 when entries were
skipped; it does not relax mutation resolution. A streaming query can emit rows
before a later failure, so consumers must check its exit status.

Execution defaults to budgets of 1,000,000 cumulative retained row allocations,
256 MiB of estimated retained value allocations, and 10,000,000 work units.
Use `--max-rows`, `--max-bytes`, `--max-work` and `--timeout SECONDS` to configure
them. Allocation budgets include intermediate results and are conservative
cumulative estimates, not process RSS limits. Cancellation and timeouts are
checked between operations; they cannot interrupt a blocked filesystem syscall.

## Library

`Engine` provides opaque prepared queries and resolved mutations with execution
policy captured at preparation time. The mutation cap also applies to library
callers. Low-level modules remain available for callers managing their own
plans and policies.

```rust
use fsql::{Engine, ErrorPolicy};
use fsql::walk::WalkOptions;
use std::ops::ControlFlow;

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

`PreparedQuery::collect` returns a result set and the same completion report.
`ExecutionOptions` also supplies a clonable cancellation token. Streaming
callbacks can return `ControlFlow::Break(())` to stop consuming rows.

`Engine::resolve_mutation` produces an inspectable `ResolvedMutation`; its
consuming `apply(journal_base)` method creates the journal. Disabling recovery
requires the explicit `apply_without_journal` method. Apply outcomes include
completed entries, failures, partial changes and recovery-required journals.

## Build

```sh
cargo build --release
cargo test --workspace
```

[`vendor/sqlparser`] is a patched copy of [`sqlparser-rs`]; see `vendor/README.md`.
[`tree-fucker`] is a git dependency on `github.com/kjanat/tree-fucker`, pinned in
`Cargo.lock`. Its one-shot `Scan` performs the `files` traversal. It lists each
directory directly, never follows a symbolic link, and applies the mount-crossing
policy at every domain boundary, so `-x` stays on the root's filesystem while a
plain walk crosses into mounts beneath the root. Rows stream as each directory's
listing completes. Traversal operations run under tree-fucker's process-wide
resource governor, which the CLI allows one full worker of foreground time.
fsql reads metadata lazily with its own `statx` and `readlinkat` calls. These
follow-up reads use the listed directory's descriptor while an anchor is
available, and an absolute path after the bounded anchor allowance is exhausted.
They are separate from the traversal governor. The same library backs the
`mounts` probe. Query cancellation also cancels active scans; resource-limit and
quarantine events remain fatal even in best-effort mode.

[`vendor/sqlparser`]: ./vendor/sqlparser/
[`sqlparser-rs`]: https://github.com/apache/datafusion-sqlparser-rs
[`tree-fucker`]: https://github.com/kjanat/tree-fucker

<!-- rumdl-disable-file line-length -->
