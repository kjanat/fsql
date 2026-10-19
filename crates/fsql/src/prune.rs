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
            && !subtree_exclusions(filter, alias, &mut patterns)
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
fn subtree_exclusions(expr: &Expr, alias: &str, patterns: &mut Vec<String>) -> bool {
    use crate::expr::{constant_globs, unnest};
    match unnest(expr) {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            return subtree_exclusions(left, alias, patterns)
                && subtree_exclusions(right, alias, patterns);
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
                    {
                        patterns.push(prefix.to_owned());
                    }
                }
            }
        }
        _ => {}
    }
    true
}

fn local_relative_path(expr: &Expr, alias: &str) -> bool {
    match crate::expr::unnest(expr) {
        Expr::Identifier(name) => name.value.eq_ignore_ascii_case("relative_path"),
        Expr::CompoundIdentifier(parts) => matches!(parts.as_slice(), [table, name]
            if table.value.eq_ignore_ascii_case(alias) && name.value.eq_ignore_ascii_case("relative_path")),
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
                "kind = 'file' AND NOT (f.relative_path GLOB SOME (ARRAY['build{,/**}']))",
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
