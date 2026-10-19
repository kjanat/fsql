use sqlparser::ast::{Expr, Value as Literal};

/// Remove parentheses without changing expression semantics.
pub(crate) fn unnest(mut expr: &Expr) -> &Expr {
    while let Expr::Nested(inner) = expr {
        expr = inner;
    }
    expr
}

pub(crate) fn constant_globs(expr: &Expr) -> Option<Vec<String>> {
    let Expr::Array(array) = unnest(expr) else {
        return None;
    };
    array
        .elem
        .iter()
        .map(|expr| string_literal(expr).map(str::to_owned))
        .collect()
}

pub(crate) fn string_literal(expr: &Expr) -> Option<&str> {
    match unnest(expr) {
        Expr::Value(value) => match &value.value {
            Literal::SingleQuotedString(s) | Literal::DoubleQuotedString(s) => Some(s),
            _ => None,
        },
        _ => None,
    }
}
