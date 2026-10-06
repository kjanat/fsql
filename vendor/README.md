# Vendored crates

## sqlparser

Upstream: [datafusion-sqlparser-rs]\
Commit: [14cbf755b3f479a27f338c050217431279eb1f2e]\
Upstream version: 0.63.0\
Local version: 0.63.0+fsql\
License: Apache-2.0 (see [`sqlparser/LICENSE.TXT`])

### Local patches

`src/dialect/mod.rs`

- `Dialect::supports_byte_unit_suffixes` (default `false`)
- `Dialect::supports_octal_prefix` (default `false`)

`src/tokenizer.rs` and `src/tokenizer/numeric_literals.rs`

- Octal literals with `0o` or `0O` tokenize as decimal numbers when the dialect opts in. `0o7_55` becomes `Number("493")` when numeric underscores are also enabled. Separators must sit between octal digits; invalid digits, trailing identifiers, and values above `u32::MAX` are rejected.
- Byte-unit suffixes `b k m g t p kib mib gib tib pib kb mb gb tb pb` (case insensitive) scale the literal in place, so `1g` becomes `Number("1073741824")` and `1.5mib` becomes `Number("1572864")`. Decimal and exponent forms use exact base-ten arithmetic, round half up to whole bytes, and reject raw values above `u64::MAX`. No floating-point conversion or extra dependency is used.
- Both extensions preserve spans over the original source text, report errors at the literal's start, and leave quoted strings, identifiers, and comments untouched. They remain disabled by default and can be enabled independently.
- Local helpers and regression tests live together in `numeric_literals.rs`, keeping upstream tokenizer edits limited to the integration points.

The local crate uses Rust edition 2024. Redundant `ref` binding modifiers were removed where the new edition rejects them. Unpatched upstream files retain their original formatting and contents. `.dprint.jsonc` excludes `vendor/**`, and the vendor-local `.rustfmt.toml` uses upstream's Rust 2021 style independently of the project config. Existing upstream license notices, including `NOTICE.TXT`, are preserved.

### Omitted upstream files

This copy excludes upstream repository administration, CI, release tooling, and release history. Upstream-only `.clusterfuzzlite/`, `.gitattributes`, `codecov.yml`, and the tracked root `Cargo.lock` are also omitted:

| Path                             | Purpose                                                 |
| -------------------------------- | ------------------------------------------------------- |
| `.github/`                       | GitHub workflows, actions, and Dependabot configuration |
| `dev/release/`                   | Apache release packaging and verification tools         |
| `changelog/`, `CHANGELOG.md`     | Upstream release history                                |
| `.asf.yaml`                      | Apache repository administration                        |
| `.tool-versions`, `rustfmt.toml` | Upstream development tool configuration                 |
| `AGENTS.md`                      | Upstream contribution and PR instructions               |
| `SECURITY.md`                    | Upstream repository vulnerability-reporting policy      |

Source code, Cargo manifests, licensing files, tests and their SQL fixtures, examples, parser documentation, fuzz targets, and benchmarks are retained. The upstream `.gitignore` is retained to keep local build artifacts and Cargo lockfiles out of the vendored tree.

### Re-vendoring

1. Check out the upstream commit being vendored in a temporary clone and delete the clone's own `.git` directory.
2. Remove every path listed under **Omitted upstream files** from that clone. Preserve licenses, notices, and symlinks, including `derive/LICENSE.TXT`.
3. `diff -ru vendor/sqlparser <clone>` and re-apply the patches listed above.
4. Replace the directory, then update the commit and version in this file.
5. `cd vendor/sqlparser && cargo test --lib tokenizer`
6. `cargo test --workspace`

[datafusion-sqlparser-rs]: https://github.com/apache/datafusion-sqlparser-rs
[`sqlparser/LICENSE.TXT`]: https://github.com/apache/datafusion-sqlparser-rs/blob/14cbf755b3f479a27f338c050217431279eb1f2e/LICENSE.TXT
[14cbf755b3f479a27f338c050217431279eb1f2e]: https://github.com/apache/datafusion-sqlparser-rs/commit/14cbf755b3f479a27f338c050217431279eb1f2e
