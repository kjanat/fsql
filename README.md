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

```
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

```
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

## Joins, subqueries, CTEs, set operations

```
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

```
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
symlinks, checks the frozen identity, and only then issues the syscall. If any
frozen row no longer matches, the whole statement stops before touching
anything.

A statement is refused at plan time when:

- `DELETE` or `UPDATE` has no `WHERE` clause
- the `WHERE` clause is always true, such as `1 = 1`
- more rows match than `--cap` allows (default 10000)

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

```
fsql journal          list journals
fsql undo ID          reverse one journal
```

`--no-journal` applies without any of this and cannot be undone.

## Output

`-f table` (default), `csv`, `tsv`, `json` (one object per line), `lines`.

## Build

```
cargo build --release
cargo test --workspace
```

`vendor/sqlparser` is a patched copy of `sqlparser-rs`; see `vendor/README.md`.
`tree-fucker` is a git dependency on `github.com/kjanat/tree-fucker`, pinned in `Cargo.lock`.
