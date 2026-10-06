# Upstream source

Copied from [DerekStride/tree-sitter-sql](https://github.com/DerekStride/tree-sitter-sql)
at commit `97614d051eebfd3bc5d97c0bdb5a1638719ca811` (package version 0.3.11).
The upstream MIT license and copyright notice are retained in `LICENSE`.

Copied inputs: `grammar.js`, `grammar/`, `src/scanner.c`, `queries/`, and selected
`test/corpus/` cases. Packaging, documentation, examples, and fsql-specific tests
are local. The generated parser is produced with tree-sitter CLI 0.27.0, ABI 15.

## Local adaptations

- Name and scanner symbols changed from `sql` to `fsql`.
- fsql rules are overlaid in `grammar.js`; the copied grammar modules stay intact.
- Binary/decimal byte suffixes, GLOB/MATCH, XOR and interval units are recognized.
- Entry points are restricted to SELECT, INSERT, UPDATE, DELETE and VALUES.
- SELECT without FROM accepts clauses; DELETE accepts relation aliases;
  UPDATE accepts ORDER BY/LIMIT; INTERSECT/EXCEPT accept ALL or DISTINCT.
- Empty statements and an optional final semicolon are allowed.
- Highlight queries recognize numeric literals using tree-sitter's regex syntax.
- Queries omit unreachable DDL/procedural nodes. The retained corpus covers
  expressions, literals, comments, casts, CTEs, joins, subqueries, grouping,
  windows and mutations. DDL, COPY, SELECT INTO, and nonstandard parenthesized
  WITH headers are omitted; DELETE trees gain the relation wrapper.
- The package detects `.fsql` files as `source.fsql`; it does not take over `.sql`.

This is intentionally a permissive SQL grammar. Expression and query syntax
inherited from upstream may parse successfully even though fsql cannot execute it
(for example, window functions). DDL and procedural statements are excluded.
The Rust planner and evaluator remain the authority on supported queries and mutations.

## Updating the copy

Fetch a pinned upstream archive and compare the copied inputs, preserving the
local overlays, scanner symbol rename, queries, and tests. Update this commit
reference, regenerate the parser, and run the corpus and example checks. Do not
replace fsql-specific additions with an unreviewed upstream snapshot.
