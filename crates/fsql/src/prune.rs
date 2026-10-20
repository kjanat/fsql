//! Conservative proofs that a single source's descendants cannot match.
use globset::GlobSet;
use sqlparser::ast::{BinaryOperator, Expr};
use std::path::Path;

use crate::error::Result;
use crate::pattern::{compile_glob, compile_globs};

#[derive(Debug, Default)]
pub(crate) struct Pruning {
    excluded: Option<GlobSet>,
}

impl Pruning {
    pub(crate) fn for_filter(filter: Option<&Expr>, alias: &str) -> Result<Self> {
        let mut patterns = Vec::new();
        if let Some(filter) = filter
            && !subtree_exclusions(filter, alias, &mut patterns, &mut true)
        {
            patterns.clear();
        }
        Ok(Self {
            excluded: if patterns.is_empty() {
                None
            } else {
                Some(compile_globs(patterns.iter().map(String::as_str))?)
            },
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.excluded.is_none()
    }

    pub(crate) fn excludes(&self, relative: &Path) -> bool {
        // The source root is empty; its children have no leading slash.
        !relative.as_os_str().is_empty()
            && self
                .excluded
                .as_ref()
                .is_some_and(|set| set.is_match(relative))
    }
}

/// Only mandatory negative predicates can prove that descendants are excluded.
/// In particular, never infer pruning from a directory failing a row predicate.
fn subtree_exclusions(
    expr: &Expr,
    alias: &str,
    patterns: &mut Vec<String>,
    prefix_safe: &mut bool,
) -> bool {
    use crate::expr::{constant_globs, unnest};
    let mut safe = infallible_guard(expr, alias);
    match unnest(expr) {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            return subtree_exclusions(left, alias, patterns, prefix_safe)
                && subtree_exclusions(right, alias, patterns, prefix_safe);
        }
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Not,
            expr,
        } => {
            let candidates = match unnest(expr) {
                Expr::AnyOp {
                    left,
                    compare_op: BinaryOperator::Glob,
                    right,
                    ..
                } if local_relative_path(left, alias) => constant_globs(right),
                Expr::BinaryOp {
                    left,
                    op: BinaryOperator::Glob,
                    right,
                } if local_relative_path(left, alias) => {
                    crate::expr::string_literal(right).map(|s| vec![s.to_owned()])
                }
                _ => None,
            };
            if let Some(candidates) = candidates {
                // Never prune away rows that would expose an invalid candidate.
                if candidates
                    .iter()
                    .any(|pattern| compile_glob(pattern).is_err())
                {
                    return false;
                }
                safe = true;
                for pattern in candidates {
                    // Escapes could change the meaning of the suffix boundary.
                    if pattern.contains('\\') {
                        continue;
                    }
                    if let Some(prefix) = pattern.strip_suffix("{,/**}")
                        .or_else(|| pattern.strip_suffix("/**"))
                        && !prefix.is_empty()
                        // Removing a suffix can turn terminal ** into a recursive
                        // glob token with different zero-component semantics.
                        && !prefix.ends_with("**")
                        // Full candidates were validated atomically above.
                        && compile_glob(prefix).is_ok()
                        && *prefix_safe
                    {
                        patterns.push(prefix.to_owned());
                    }
                }
            }
        }
        _ => {}
    }
    // AND evaluates left to right. An earlier exclusion can still prune, but
    // later exclusions must not hide errors from an unproven predecessor.
    *prefix_safe &= safe;
    true
}

/// Intentionally narrow proof: depth is available without metadata I/O and
/// comparison with a representable integer cannot fail. In particular, kind
/// may need a stat on filesystems without directory-entry type hints.
fn infallible_guard(expr: &Expr, alias: &str) -> bool {
    use crate::expr::unnest;
    use sqlparser::ast::Value;
    match unnest(expr) {
        Expr::Value(value) => matches!(value.value, Value::Boolean(_) | Value::Null),
        Expr::BinaryOp {
            left,
            op:
                BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Gt
                | BinaryOperator::Lt
                | BinaryOperator::GtEq
                | BinaryOperator::LtEq,
            right,
        } => {
            let integer = |expr: &Expr| {
                matches!(unnest(expr), Expr::Value(v)
                if matches!(&v.value, Value::Number(n, _) if n.parse::<i64>().is_ok()))
            };
            (local_column(left, alias, "depth") && integer(right))
                || (integer(left) && local_column(right, alias, "depth"))
        }
        _ => false,
    }
}

fn local_relative_path(expr: &Expr, alias: &str) -> bool {
    local_column(expr, alias, "relative_path")
}

fn local_column(expr: &Expr, alias: &str, column: &str) -> bool {
    match crate::expr::unnest(expr) {
        Expr::Identifier(name) => name.value.eq_ignore_ascii_case(column),
        Expr::CompoundIdentifier(parts) => matches!(parts.as_slice(), [table, name]
            if table.value.eq_ignore_ascii_case(alias) && name.value.eq_ignore_ascii_case(column)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::FsqlDialect;
    use sqlparser::parser::Parser;

    fn policy(sql: &str) -> Pruning {
        let expr = Parser::new(&FsqlDialect)
            .try_with_sql(sql)
            .unwrap()
            .parse_expr()
            .unwrap();
        Pruning::for_filter(Some(&expr), "f").unwrap()
    }

    #[test]
    fn only_local_mandatory_subtree_exclusions_prune() {
        for (sql, expected) in [
            ("NOT (relative_path GLOB 'build/**')", true),
            (
                "depth > 0 AND NOT (f.relative_path GLOB SOME (ARRAY['build{,/**}']))",
                true,
            ),
            ("NOT (relative_path GLOB 'build')", false),
            ("NOT (name GLOB 'build/**')", false),
            ("NOT (outer_row.relative_path GLOB 'build/**')", false),
            (
                "NOT (relative_path GLOB 'build/**') OR name = 'keep'",
                false,
            ),
            (
                "NOT (relative_path GLOB ALL (ARRAY['build/**', 'other/**']))",
                false,
            ),
            ("NOT (relative_path GLOB ANY (ARRAY[name]))", false),
            ("NOT (relative_path GLOB ANY (ARRAY['[broken/**']))", false),
            ("NOT (relative_path GLOB 'build\\{,/**}')", false),
        ] {
            assert_eq!(policy(sql).excludes(Path::new("build")), expected, "{sql}");
        }
    }

    #[test]
    fn pruning_respects_left_to_right_errors() {
        for prefix in [
            "1 / (depth - 2) > 0",
            "size > 0",
            "kind = 'file'",
            "depth > 'bad'",
        ] {
            assert!(
                policy(&format!(
                    "{prefix} AND NOT (relative_path GLOB 'build{{,/**}}')"
                ))
                .is_empty()
            );
            assert!(
                policy(&format!(
                    "depth > 0 AND ({prefix} AND NOT (relative_path GLOB 'build{{,/**}}'))"
                ))
                .is_empty()
            );
            assert!(
                policy(&format!(
                    "NOT (relative_path GLOB 'build{{,/**}}') AND {prefix}"
                ))
                .excludes(Path::new("build"))
            );
        }
        for prefix in ["depth > 0", "0 < f.depth", "true"] {
            assert!(
                policy(&format!(
                    "{prefix} AND NOT (relative_path GLOB 'build{{,/**}}')"
                ))
                .excludes(Path::new("build"))
            );
        }
    }

    #[test]
    fn invalid_candidate_disables_all_pruning() {
        for sql in [
            "NOT (relative_path GLOB ANY (ARRAY['build{,/**}', '[']))",
            "NOT (relative_path GLOB ANY (ARRAY['[', 'build{,/**}']))",
            "NOT (relative_path GLOB 'build/**') AND NOT (relative_path GLOB ANY (ARRAY['[']))",
        ] {
            assert!(policy(sql).is_empty(), "{sql}");
        }
    }

    #[test]
    fn every_pruned_directory_has_only_excluded_descendants() {
        for pattern in [
            "build/**",
            "build{,/**}",
            "**/build{,/**}",
            "{build,dist}{,/**}",
            "**/{build,dist}/**",
            "b*{,/**}",
            "[bd]*{,/**}",
            "**{,/**}",
            "build/**{,/**}",
            "build/**/**",
            "build/*{,/**}",
            "**/*{,/**}",
            "src/**/temp{,/**}",
        ] {
            let pruning = policy(&format!("NOT (relative_path GLOB '{pattern}')"));
            let original = compile_glob(pattern).unwrap().compile_matcher();
            for directory in [
                "",
                "build",
                "dist",
                "builder",
                "src/build",
                "src/temp",
                "src/x/temp",
                "other",
            ] {
                let directory = Path::new(directory);
                if pruning.excludes(directory) {
                    for child in ["file", ".hidden", "nested/file", "nested/build/file"] {
                        assert!(
                            original.is_match(directory.join(child)),
                            "{pattern}: {directory:?}/{child}"
                        );
                    }
                }
            }
        }
    }
}
