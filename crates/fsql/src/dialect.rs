use sqlparser::ast::{BinaryOperator, Expr};
use sqlparser::dialect::Dialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};

#[derive(Debug, Default, Clone, Copy)]
pub struct FsqlDialect;

impl Dialect for FsqlDialect {
    fn is_identifier_start(&self, ch: char) -> bool {
        ch.is_ascii_alphabetic() || ch == '_'
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '_'
    }

    fn supports_byte_unit_suffixes(&self) -> bool {
        true
    }

    fn supports_octal_prefix(&self) -> bool {
        true
    }

    fn supports_numeric_literal_underscores(&self) -> bool {
        true
    }

    fn parse_infix(
        &self,
        parser: &mut Parser,
        expr: &Expr,
        precedence: u8,
    ) -> Option<Result<Expr, ParserError>> {
        for (keyword, op) in [
            (Keyword::GLOB, BinaryOperator::Glob),
            (Keyword::REGEXP, BinaryOperator::Regexp),
            (Keyword::MATCH, BinaryOperator::Match),
        ] {
            if parser.parse_keyword(keyword) {
                let left = Box::new(expr.clone());
                return Some(
                    parser
                        .parse_subexpr(precedence)
                        .map(|right| Expr::BinaryOp {
                            left,
                            op,
                            right: Box::new(right),
                        }),
                );
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::{SetExpr, Statement};

    fn selection(sql: &str) -> String {
        let statements = Parser::parse_sql(&FsqlDialect, sql).expect("parse");
        let [Statement::Query(query)] = statements.as_slice() else {
            panic!("expected a single query");
        };
        let SetExpr::Select(select) = query.body.as_ref() else {
            panic!("expected a select");
        };
        select.selection.as_ref().expect("where clause").to_string()
    }

    #[test]
    fn binary_byte_suffix_scales_by_powers_of_two() {
        assert_eq!(
            selection("select path from files where size > 1g"),
            "size > 1073741824"
        );
        assert_eq!(
            selection("select path from files where size > 512K"),
            "size > 524288"
        );
    }

    #[test]
    fn decimal_byte_suffix_scales_by_powers_of_ten() {
        assert_eq!(
            selection("select path from files where size > 4gb"),
            "size > 4000000000"
        );
    }

    #[test]
    fn fractional_byte_literal_rounds_to_whole_bytes() {
        assert_eq!(
            selection("select path from files where size > 1.5mib"),
            "size > 1572864"
        );
    }

    #[test]
    fn underscores_combine_with_suffix() {
        assert_eq!(
            selection("select path from files where size > 1_024k"),
            "size > 1048576"
        );
    }

    #[test]
    fn octal_literal_reads_as_permission_bits() {
        assert_eq!(
            selection("select path from files where mode = 0o755"),
            "mode = 493"
        );
    }

    #[test]
    fn glob_binds_tighter_than_and() {
        assert_eq!(
            selection("select path from files where path glob '*.tmp' and size > 1k"),
            "path GLOB '*.tmp' AND size > 1024"
        );
    }

    #[test]
    fn regexp_and_match_are_operators() {
        assert_eq!(
            selection("select path from files where name regexp '^a' or name match 'b'"),
            "name REGEXP '^a' OR name MATCH 'b'"
        );
    }

    #[test]
    fn unknown_suffix_is_rejected() {
        assert!(Parser::parse_sql(&FsqlDialect, "select 1 where size > 1gx").is_err());
    }

    #[test]
    fn octal_without_digits_is_rejected() {
        assert!(Parser::parse_sql(&FsqlDialect, "select 1 where mode = 0o").is_err());
        assert!(Parser::parse_sql(&FsqlDialect, "select 1 where mode = 0o8").is_err());
    }

    #[test]
    fn oversized_byte_literal_is_rejected() {
        assert!(Parser::parse_sql(&FsqlDialect, "select 1 where size > 99999999pib").is_err());
    }
}
