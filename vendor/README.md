# Vendored crates

## sqlparser

Upstream: [datafusion-sqlparser-rs]\
Commit: [14cbf755b3f479a27f338c050217431279eb1f2e]\
Upstream version: 0.63.0\
Package: `fsql-sqlparser`, versioned with the workspace\
License: Apache-2.0, in [`sqlparser/LICENSE.TXT`]

### Local patches

`derive/Cargo.toml` sets `publish = false`. The root workspace follows that path dependency, and the published `fsql-sqlparser` crate depends on crates.io `sqlparser_derive` 0.6.0.

`Cargo.toml`, `src/lib.rs` and `src/dialect/mod.rs` drop the `derive-dialect` feature, the `derive_dialect!` re-export and its tests. `sqlparser_derive` 0.6.0 locates the parser source by searching the registry for a `sqlparser-*` directory, and finds no `fsql-sqlparser` package.

`fuzz/Cargo.toml` and `sqlparser_bench/Cargo.toml` name the parser dependency `package = "fsql-sqlparser"`. `sqlparser_bench/Cargo.toml` declares its own `[workspace]`, like `fuzz/Cargo.toml`.

The version badge in `README.md` points at `fsql-sqlparser`.

`src/dialect/mod.rs`

- `Dialect::supports_byte_unit_suffixes` defaults to `false`.
- `Dialect::supports_octal_prefix` defaults to `false`.

`src/tokenizer.rs` and `src/tokenizer/numeric_literals.rs`

- Octal literals with `0o` or `0O` tokenize as decimal numbers when the dialect opts in. `0o7_55` becomes `Number("493")` when numeric underscores are also enabled. Separators may appear only between octal digits. The tokenizer rejects invalid digits, trailing identifiers, and values above `u32::MAX`.
- Byte-unit suffixes `b k m g t p kib mib gib tib pib kb mb gb tb pb` match in any case and scale the literal in place, so `1g` becomes `Number("1073741824")` and `1.5mib` becomes `Number("1572864")`. Decimal and exponent forms use exact base-ten arithmetic, round half up to whole bytes, and reject raw values above `u64::MAX`. The tokenizer uses no floating-point conversion and adds no dependency.
- Both extensions preserve spans over the original source text, report errors at the literal's start, and leave quoted strings, identifiers, and comments untouched. Both are off by default, and a dialect can enable each one on its own.
- Local helpers and regression tests live in `numeric_literals.rs`. Upstream tokenizer edits touch only the integration points.

The local crate uses Rust edition 2024 and omits `ref` binding modifiers that the edition rejects. `cargo clippy --fix` collapses nested `if` blocks into let chains in `src/ast/`, `src/dialect/mssql.rs`, `src/parser/mod.rs`, `src/test_utils.rs` and `src/tokenizer.rs`; repeat it after re-vendoring. Other upstream files keep their original formatting and contents. `dprint.toml` excludes `vendor/*/**`, and the vendor-local `.rustfmt.toml` sets upstream's Rust 2021 style separately from the project config. The copy keeps upstream license notices, including `NOTICE.TXT`.

### Omitted upstream files

This copy omits the upstream-only `.clusterfuzzlite/`, `.gitattributes`, `codecov.yml`, and the tracked root `Cargo.lock`. It also omits upstream repository administration, CI, release tooling, and release history:

| Path                             | Purpose                                                 |
| -------------------------------- | ------------------------------------------------------- |
| `.github/`                       | GitHub workflows, actions, and Dependabot configuration |
| `dev/release/`                   | Apache release packaging and verification tools         |
| `changelog/`, `CHANGELOG.md`     | Upstream release history                                |
| `.asf.yaml`                      | Apache repository administration                        |
| `.tool-versions`, `rustfmt.toml` | Upstream development tool configuration                 |
| `AGENTS.md`                      | Upstream contribution and PR instructions               |
| `SECURITY.md`                    | Upstream repository vulnerability-reporting policy      |

The copy retains source code, Cargo manifests, licensing files, tests and their SQL fixtures, examples, parser documentation, fuzz targets, and benchmarks. It also retains the upstream `.gitignore`, which keeps local build artifacts and Cargo lockfiles out of the vendored tree.

### Re-vendoring

1. Check out the upstream commit you are vendoring in a temporary clone and delete the clone's own `.git` directory.
2. Remove every path listed under **Omitted upstream files** from that clone. Preserve licenses, notices, and symlinks, including `derive/LICENSE.TXT`.
3. Run `diff -ru vendor/sqlparser <clone>` and re-apply the patches listed above.
4. Replace the directory, then update the commit in this file. Keep the package name `fsql-sqlparser` and `[lib] name = "sqlparser"`.
5. Run `cargo test --lib tokenizer -p fsql-sqlparser`.
6. Run `cargo test --workspace`.

[datafusion-sqlparser-rs]: https://github.com/apache/datafusion-sqlparser-rs
[`sqlparser/LICENSE.TXT`]: https://github.com/apache/datafusion-sqlparser-rs/blob/14cbf755b3f479a27f338c050217431279eb1f2e/LICENSE.TXT
[14cbf755b3f479a27f338c050217431279eb1f2e]: https://github.com/apache/datafusion-sqlparser-rs/commit/14cbf755b3f479a27f338c050217431279eb1f2e
