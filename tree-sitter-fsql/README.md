# tree-sitter-fsql

A permissive tree-sitter grammar for `.fsql` query files, adapted from [Derek Stride (@DerekStride)]'s [`tree-sitter-sql`]. See [UPSTREAM.md] for the pinned source and local changes.

The grammar includes SELECT, INSERT, UPDATE, DELETE, joins, subqueries, CTEs, VALUES, and set operations, with fsql byte sizes (`1g`, `4GB`, `1.5MiB`, `1_024k`), octal permissions (`0o755`), and pattern operators (`GLOB`, `REGEXP`, `MATCH`, `LIKE`, `ILIKE`). Comments and multiple semicolon-separated statements are supported, with an optional final semicolon.

This is an editor parser, not an execution validator. It intentionally retains SQL syntax that the fsql engine rejects. Parsing a mutation does not execute it or prove that it is safe.

## Editor integration

- Language name: `fsql`
- Scope: `source.fsql`
- File extension: `.fsql`
- C entry point: `tree_sitter_fsql`
- Generated parser: `src/parser.c` (ABI 15)
- External scanner: `src/scanner.c`
- Queries: `queries/highlights.scm` and `queries/indents.scm`

Compile both C sources with `src` on the include path. The generated parser and headers are checked in, so consumers do not need Node.js or tree-sitter CLI. Editor extensions can point at this repository's `tree-sitter-fsql` directory. The Rust execution parser remains unchanged.

## Development

Use Bun 1.4.2 and run these commands from the repository root. The root workspace manifest and `bun.lock` install tree-sitter CLI 0.27.0:

```sh
bun install --frozen-lockfile
bun run --cwd tree-sitter-fsql generate
bun run --cwd tree-sitter-fsql typecheck
bun run --cwd tree-sitter-fsql test
bun run --cwd tree-sitter-fsql check
```

`bun run --cwd tree-sitter-fsql typecheck` typechecks the JavaScript grammar, helpers, and check script. `bun run --cwd tree-sitter-fsql test` runs the retained SQL corpus, fsql regression cases, error recovery, and highlighting assertions. `bun run --cwd tree-sitter-fsql check` parses the example files, compiles the highlight and indent queries, and compares incremental edits with fresh parses. The examples are query files only; these checks never execute filesystem mutations. To run a file through fsql separately, use `fsql tree-sitter-fsql/examples/browse.fsql` from the repository root.

[Derek Stride (@DerekStride)]: https://github.com/DerekStride
[`tree-sitter-sql`]: https://github.com/DerekStride/tree-sitter-sql/tree/97614d051eebfd3bc5d97c0bdb5a1638719ca811
[UPSTREAM.md]: UPSTREAM.md
