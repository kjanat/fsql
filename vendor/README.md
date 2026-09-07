# Vendored crates

## sqlparser

Upstream: https://github.com/apache/datafusion-sqlparser-rs
Commit: 2f3b5b824c24304720f1e8e504f54b9910e63bad
Version: 0.62.0
License: Apache-2.0 (see `sqlparser/LICENSE.TXT`)

### Local patches

`src/dialect/mod.rs`

- `Dialect::supports_byte_unit_suffixes` (default `false`)
- `Dialect::supports_octal_prefix` (default `false`)

`src/tokenizer.rs`

- Octal literal `0o755` tokenizes as `Number("493")` when the dialect opts in.
- Byte-unit suffixes `b k m g t p kib mib gib tib pib kb mb gb tb pb` scale the
  literal in place, so `1g` tokenizes as `Number("1073741824")` and `1.5mib` as
  `Number("1572864")`. Spans still cover the original source text.
- Helpers `peek_byte_unit_suffix` and `scale_by_byte_unit`.

Everything else is byte-identical to upstream. `.gitattributes` marks
`vendor/**` as `-text` and `.dprint.jsonc` excludes it, so neither git nor the
formatter rewrites upstream files.

### Re-vendoring

1. Shallow-clone upstream next to the current copy and delete the clone's own
   `.git` directory.
2. `diff -ru vendor/sqlparser <clone>` and re-apply the patches listed above.
3. Replace the directory, then update the commit and version in this file.
4. `cd vendor/sqlparser && cargo test --lib tokenizer`
5. `cargo test --workspace`
