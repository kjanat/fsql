# fsql-cli

[![Crates.io](https://img.shields.io/crates/v/fsql-cli?logo=rust&labelColor=B7410E&color=black)][crates] [![License: MIT](https://img.shields.io/crates/l/fsql-cli?color=blue)](LICENSE)

`fsql-cli` installs `fsql`, a command that queries and changes the filesystem with SQL.

```sh
cargo install fsql-cli
```

## Query

```sh
fsql -e "select path, human(size) from files where size > 1g order by size desc limit 20"
fsql -C ~/projects -e "select ext, count(*), sum(size) from files group by ext order by 3 desc"
fsql -f json -e "select path, mtime from files where ext = 'log'"
```

`-C DIR` sets the directory the `files` table starts from. The default is the current directory. `-f` selects `table`, `csv`, `tsv`, `json` or `lines`.

`fsql FILE`, `fsql -F FILE` and `fsql -` read statements from a file or from stdin. `-e` can be passed more than once.

## Change files

```sh
fsql -e "delete from files where ext = 'tmp' and mtime < now() - interval '30' day"
fsql --apply -e "delete from files where ext = 'tmp' and mtime < now() - interval '30' day"
```

Without `--apply`, `DELETE`, `UPDATE` and `INSERT` print the entries they would change and change nothing. With `--apply`, fsql refuses a statement that touches more than `--cap` entries (10000 by default).

Every applied statement is journaled under `$XDG_DATA_HOME/fsql/journal`:

```sh
fsql journal    # list statements that can be undone
fsql undo ID    # reverse one
fsql recover ID # finish an interrupted apply or undo
```

`--no-journal` applies without a journal, and fsql cannot undo the result.

## Limits

`--max-rows`, `--max-bytes`, `--max-work` and `--timeout SECONDS` bound a query. `--best-effort` returns the readable rows of a `SELECT` when some entries cannot be read, and exits with status 1 when it skipped entries.

The [fsql repository](https://github.com/kjanat/fsql#readme) documents the tables, columns, functions and the full mutation rules. The engine is the [`fsql`](https://crates.io/crates/fsql) crate.

## License

MIT. See [`LICENSE`](LICENSE).

[crates]: https://crates.io/crates/fsql-cli
