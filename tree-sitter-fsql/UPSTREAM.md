# Upstream source

This directory copies [Derek Stride (@DerekStride)]'s [`tree-sitter-sql`] at commit `97614d051eebfd3bc5d97c0bdb5a1638719ca811`, package version 0.3.11. [`LICENSE`] keeps the upstream MIT license and copyright notice.

The copied inputs are `grammar.js`, `grammar/`, `src/scanner.c`, `queries/`, and selected `test/corpus/` cases. Packaging, documentation, examples, and fsql-specific tests are local additions. tree-sitter CLI 0.27.0 generates the parser with ABI 15.

## Local adaptations

- Name and scanner symbols changed from `sql` to `fsql`.
- `grammar.js` overlays the fsql rules. The copied modules have JSDoc types for strict JavaScript checking. The copy drops identical duplicate keywords, and helper signatures document the optional arguments the helpers already took.
- Callbacks take `$` only when they use grammar symbols and take no arguments otherwise. The helpers are simpler and the regexes use equivalent, simpler syntax. These edits leave the generated parser unchanged.
- The grammar recognizes binary and decimal byte suffixes, GLOB, MATCH, XOR and interval units.
- Entry points are SELECT, INSERT, UPDATE, DELETE and VALUES.
- SELECT without FROM accepts clauses. DELETE accepts relation aliases. UPDATE accepts ORDER BY and LIMIT. INTERSECT and EXCEPT accept ALL or DISTINCT.
- The grammar allows empty statements and an optional final semicolon.
- Highlight queries recognize numeric literals with tree-sitter's regex syntax.
- The queries omit unreachable DDL and procedural nodes. The retained corpus covers expressions, literals, comments, casts, CTEs, joins, subqueries, grouping, windows and mutations. It omits DDL, COPY, SELECT INTO, and nonstandard parenthesized WITH headers, and DELETE trees gain the relation wrapper.
- The package detects `.fsql` files as `source.fsql` and ignores `.sql` files.

The grammar is deliberately permissive. Expression and query syntax from upstream can parse even when fsql cannot execute it. Window functions are one example. The grammar excludes DDL and procedural statements. The Rust planner and evaluator decide which queries and mutations fsql supports.

## Updating the copy

Fetch a pinned upstream archive and compare the copied inputs. Keep the local overlays, scanner symbol rename, queries, and tests. Update this commit reference, regenerate the parser, and run the corpus and example checks. Review every upstream change before it replaces an fsql-specific addition.

[Derek Stride (@DerekStride)]: https://github.com/DerekStride
[`tree-sitter-sql`]: https://github.com/DerekStride/tree-sitter-sql/tree/97614d051eebfd3bc5d97c0bdb5a1638719ca811
[`LICENSE`]: https://github.com/kjanat/fsql/blob/5b4cb65c7b51b5a8432e6304339a0eda49ec9c13/tree-sitter-fsql/LICENSE
