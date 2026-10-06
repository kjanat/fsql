use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};
use regex::bytes::Regex;
use std::rc::Rc;

use sqlparser::ast::{
    BinaryOperator, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Interval,
    Query, UnaryOperator, Value as Literal, ValueWithSpan,
};

use crate::output::ResultSet;

use crate::error::{Error, Result};
use crate::time;
use crate::value::{self, Nanos, Value};

pub trait Row {
    fn column(&self, name: &str) -> Result<Value>;
}

pub const AGGREGATES: [&str; 7] = [
    "count",
    "sum",
    "min",
    "max",
    "avg",
    "group_concat",
    "string_agg",
];

pub fn is_aggregate(name: &str) -> bool {
    AGGREGATES.contains(&name.to_ascii_lowercase().as_str())
}

pub type SubqueryRunner = dyn Fn(&Query, Option<&dyn Row>) -> Result<ResultSet>;

pub struct Evaluator {
    now: Nanos,
    globs: HashMap<String, GlobMatcher>,
    regexes: HashMap<String, Regex>,
    pub aggregates: HashMap<String, Value>,
    pub subqueries: Option<Rc<SubqueryRunner>>,
    cached: HashMap<String, Option<Rc<ResultSet>>>,
}

impl Default for Evaluator {
    fn default() -> Self {
        Self::new(time::now())
    }
}

impl Evaluator {
    pub fn new(now: Nanos) -> Self {
        Self {
            now,
            globs: HashMap::new(),
            regexes: HashMap::new(),
            aggregates: HashMap::new(),
            subqueries: None,
            cached: HashMap::new(),
        }
    }

    pub fn now(&self) -> Nanos {
        self.now
    }

    pub fn eval(&mut self, expr: &Expr, row: &dyn Row) -> Result<Value> {
        match expr {
            Expr::Identifier(ident) => column(&ident.value, row),
            Expr::CompoundIdentifier(parts) => {
                let name = parts
                    .iter()
                    .map(|p| p.value.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                column(&name, row)
            }
            Expr::Subquery(query) => {
                let set = self.subquery(query, row)?;
                if set.headers.len() != 1 {
                    return Err(Error::Plan(format!(
                        "scalar subquery yields {} columns",
                        set.headers.len()
                    )));
                }
                match set.rows.len() {
                    0 => Ok(Value::Null),
                    1 => Ok(set.rows[0][0].clone()),
                    n => Err(Error::Plan(format!("scalar subquery yields {n} rows"))),
                }
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let needle = self.eval(expr, row)?;
                let set = self.subquery(subquery, row)?;
                if set.headers.len() != 1 {
                    return Err(Error::Plan(format!(
                        "IN subquery yields {} columns",
                        set.headers.len()
                    )));
                }
                if needle.is_null() {
                    return Ok(Value::Null);
                }
                let mut saw_null = false;
                for candidate in &set.rows {
                    if candidate[0].is_null() {
                        saw_null = true;
                        continue;
                    }
                    if compare(&BinaryOperator::Eq, needle.clone(), candidate[0].clone())?
                        == Value::Bool(true)
                    {
                        return Ok(Value::Bool(!negated));
                    }
                }
                Ok(if saw_null {
                    Value::Null
                } else {
                    Value::Bool(*negated)
                })
            }
            Expr::Exists { subquery, negated } => {
                let set = self.subquery(subquery, row)?;
                Ok(Value::Bool(set.rows.is_empty() == *negated))
            }
            Expr::AnyOp {
                left,
                compare_op,
                right,
                ..
            } => self.quantified(left, compare_op, right, row, true),
            Expr::AllOp {
                left,
                compare_op,
                right,
            } => self.quantified(left, compare_op, right, row, false),
            Expr::Value(literal) => literal_value(&literal.value),
            Expr::Nested(inner) => self.eval(inner, row),
            Expr::UnaryOp { op, expr } => {
                let operand = self.eval(expr, row)?;
                unary(op, operand)
            }
            Expr::BinaryOp { left, op, right } => self.binary(left, op, right, row),
            Expr::IsNull(inner) => Ok(Value::Bool(self.eval(inner, row)?.is_null())),
            Expr::IsNotNull(inner) => Ok(Value::Bool(!self.eval(inner, row)?.is_null())),
            Expr::IsTrue(inner) => Ok(Value::Bool(self.eval(inner, row)?.truth() == Some(true))),
            Expr::IsNotTrue(inner) => Ok(Value::Bool(self.eval(inner, row)?.truth() != Some(true))),
            Expr::IsFalse(inner) => Ok(Value::Bool(self.eval(inner, row)?.truth() == Some(false))),
            Expr::IsNotFalse(inner) => {
                Ok(Value::Bool(self.eval(inner, row)?.truth() != Some(false)))
            }
            Expr::IsUnknown(inner) => Ok(Value::Bool(self.eval(inner, row)?.truth().is_none())),
            Expr::IsNotUnknown(inner) => Ok(Value::Bool(self.eval(inner, row)?.truth().is_some())),
            Expr::InList {
                expr,
                list,
                negated,
            } => self.in_list(expr, list, *negated, row),
            Expr::Between {
                expr,
                negated,
                low,
                high,
            } => self.between(expr, low, high, *negated, row),
            Expr::Like {
                negated,
                expr,
                pattern,
                escape_char,
                ..
            } => self.like(expr, pattern, escape_char.as_ref(), false, *negated, row),
            Expr::ILike {
                negated,
                expr,
                pattern,
                escape_char,
                ..
            } => self.like(expr, pattern, escape_char.as_ref(), true, *negated, row),
            Expr::Interval(interval) => self.interval(interval, row),
            Expr::Function(function) => self.function(function, row),
            Expr::Cast {
                expr, data_type, ..
            } => cast(self.eval(expr, row)?, &data_type.to_string()),
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                let mut args = vec![self.eval(expr, row)?];
                args.push(match substring_from {
                    Some(from) => self.eval(from, row)?,
                    None => Value::Int(1),
                });
                if let Some(length) = substring_for {
                    args.push(self.eval(length, row)?);
                }
                substr_values("substr", &args)
            }
            Expr::Trim {
                trim_where,
                trim_what,
                expr,
                trim_characters,
            } => {
                let subject = self.eval(expr, row)?;
                let what = match (trim_what, trim_characters.as_ref().and_then(|c| c.first())) {
                    (Some(what), _) => Some(self.eval(what, row)?),
                    (None, Some(what)) => Some(self.eval(what, row)?),
                    (None, None) => None,
                };
                let side = trim_where
                    .as_ref()
                    .map(|w| w.to_string().to_ascii_lowercase())
                    .unwrap_or_else(|| "both".to_owned());
                trim(subject, what, &side)
            }
            Expr::Extract { field, expr, .. } => {
                let subject = self.eval(expr, row)?;
                extract(&field.to_string(), subject)
            }
            other => Err(Error::Unsupported(format!("expression `{other}`"))),
        }
    }

    fn subquery(&mut self, query: &Query, row: &dyn Row) -> Result<Rc<ResultSet>> {
        let Some(runner) = self.subqueries.clone() else {
            return Err(Error::Unsupported("subquery".to_owned()));
        };
        let key = query.to_string();
        match self.cached.get(&key) {
            Some(Some(set)) => return Ok(set.clone()),
            Some(None) => return runner(query, Some(row)).map(Rc::new),
            None => {}
        }
        match runner(query, None) {
            Ok(set) => {
                let set = Rc::new(set);
                self.cached.insert(key, Some(set.clone()));
                Ok(set)
            }
            Err(Error::UnknownColumn(_)) => {
                self.cached.insert(key, None);
                runner(query, Some(row)).map(Rc::new)
            }
            Err(error) => Err(error),
        }
    }

    fn quantified(
        &mut self,
        left: &Expr,
        op: &BinaryOperator,
        right: &Expr,
        row: &dyn Row,
        any: bool,
    ) -> Result<Value> {
        let subject = self.eval(left, row)?;
        let candidates: Vec<Value> = match right {
            Expr::Subquery(query) => {
                let set = self.subquery(query, row)?;
                if set.headers.len() != 1 {
                    return Err(Error::Plan(format!(
                        "quantified subquery yields {} columns",
                        set.headers.len()
                    )));
                }
                set.rows.iter().map(|r| r[0].clone()).collect()
            }
            other => vec![self.eval(other, row)?],
        };
        let mut unknown = false;
        for candidate in candidates {
            match compare(op, subject.clone(), candidate)? {
                Value::Bool(true) if any => return Ok(Value::Bool(true)),
                Value::Bool(false) if !any => return Ok(Value::Bool(false)),
                Value::Bool(_) => {}
                _ => unknown = true,
            }
        }
        Ok(if unknown {
            Value::Null
        } else {
            Value::Bool(!any)
        })
    }

    fn binary(
        &mut self,
        left: &Expr,
        op: &BinaryOperator,
        right: &Expr,
        row: &dyn Row,
    ) -> Result<Value> {
        match op {
            BinaryOperator::And => {
                let l = self.eval(left, row)?.truth();
                if l == Some(false) {
                    return Ok(Value::Bool(false));
                }
                let r = self.eval(right, row)?.truth();
                Ok(from_truth(value::and(l, r)))
            }
            BinaryOperator::Or => {
                let l = self.eval(left, row)?.truth();
                if l == Some(true) {
                    return Ok(Value::Bool(true));
                }
                let r = self.eval(right, row)?.truth();
                Ok(from_truth(value::or(l, r)))
            }
            _ => {
                let l = self.eval(left, row)?;
                let r = self.eval(right, row)?;
                self.apply(op, l, r)
            }
        }
    }

    fn apply(&mut self, op: &BinaryOperator, l: Value, r: Value) -> Result<Value> {
        match op {
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Gt
            | BinaryOperator::Lt
            | BinaryOperator::GtEq
            | BinaryOperator::LtEq => compare(op, l, r),
            BinaryOperator::Plus
            | BinaryOperator::Minus
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Modulo => arithmetic(op, l, r),
            BinaryOperator::BitwiseAnd
            | BinaryOperator::BitwiseOr
            | BinaryOperator::BitwiseXor
            | BinaryOperator::Xor => bitwise(op, l, r),
            BinaryOperator::StringConcat => concat(l, r),
            BinaryOperator::Glob => self.glob(l, r),
            BinaryOperator::Regexp => self.regexp(l, r),
            other => Err(Error::Unsupported(format!("operator `{other}`"))),
        }
    }

    fn in_list(
        &mut self,
        expr: &Expr,
        list: &[Expr],
        negated: bool,
        row: &dyn Row,
    ) -> Result<Value> {
        let needle = self.eval(expr, row)?;
        if needle.is_null() {
            return Ok(Value::Null);
        }
        let mut saw_null = false;
        for candidate in list {
            let candidate = self.eval(candidate, row)?;
            if candidate.is_null() {
                saw_null = true;
                continue;
            }
            if let Value::Bool(true) = compare(&BinaryOperator::Eq, needle.clone(), candidate)? {
                return Ok(Value::Bool(!negated));
            }
        }
        if saw_null {
            return Ok(Value::Null);
        }
        Ok(Value::Bool(negated))
    }

    fn between(
        &mut self,
        expr: &Expr,
        low: &Expr,
        high: &Expr,
        negated: bool,
        row: &dyn Row,
    ) -> Result<Value> {
        let subject = self.eval(expr, row)?;
        let low = self.eval(low, row)?;
        let high = self.eval(high, row)?;
        let above = compare(&BinaryOperator::GtEq, subject.clone(), low)?.truth();
        let below = compare(&BinaryOperator::LtEq, subject, high)?.truth();
        let inside = value::and(above, below);
        Ok(from_truth(if negated {
            value::not(inside)
        } else {
            inside
        }))
    }

    fn like(
        &mut self,
        expr: &Expr,
        pattern: &Expr,
        escape: Option<&ValueWithSpan>,
        fold: bool,
        negated: bool,
        row: &dyn Row,
    ) -> Result<Value> {
        let subject = self.eval(expr, row)?;
        let pattern = self.eval(pattern, row)?;
        if subject.is_null() || pattern.is_null() {
            return Ok(Value::Null);
        }
        let escape = match escape.map(|e| &e.value) {
            None => None,
            Some(Literal::SingleQuotedString(s)) | Some(Literal::DoubleQuotedString(s)) => {
                s.chars().next()
            }
            Some(other) => {
                return Err(Error::Unsupported(format!("escape `{other}`")));
            }
        };
        let matched = match (&subject, &pattern) {
            (Value::Text(text), Value::Text(pat)) => {
                let (text, pat) = if fold {
                    (text.to_lowercase(), pat.to_lowercase())
                } else {
                    (text.clone(), pat.clone())
                };
                let text: Vec<char> = text.chars().collect();
                let pat: Vec<char> = pat.chars().collect();
                let escape = escape.map(|c| if fold { c.to_ascii_lowercase() } else { c });
                like_match(&text, &pat, escape)
            }
            _ => {
                let text =
                    text_bytes(&subject).ok_or_else(|| mismatch("LIKE", &subject, &pattern))?;
                let pat =
                    text_bytes(&pattern).ok_or_else(|| mismatch("LIKE", &subject, &pattern))?;
                let (text, pat): (Vec<u8>, Vec<u8>) = if fold {
                    (text.to_ascii_lowercase(), pat.to_ascii_lowercase())
                } else {
                    (text.into_owned(), pat.into_owned())
                };
                let escape = escape.and_then(|c| u8::try_from(c).ok());
                like_match(&text, &pat, escape)
            }
        };
        Ok(Value::Bool(matched != negated))
    }

    fn glob(&mut self, subject: Value, pattern: Value) -> Result<Value> {
        if subject.is_null() || pattern.is_null() {
            return Ok(Value::Null);
        }
        let Value::Text(pattern_text) = &pattern else {
            return Err(mismatch("GLOB", &subject, &pattern));
        };
        let bytes = text_bytes(&subject).ok_or_else(|| mismatch("GLOB", &subject, &pattern))?;
        if !self.globs.contains_key(pattern_text) {
            if self.globs.len() >= 64 {
                self.globs.clear();
            }
            let glob = GlobBuilder::new(pattern_text)
                .literal_separator(false)
                .build()
                .map_err(|e| Error::InvalidPattern {
                    pattern: pattern_text.clone(),
                    reason: e.kind().to_string(),
                })?;
            self.globs
                .insert(pattern_text.clone(), glob.compile_matcher());
        }
        let matcher = &self.globs[pattern_text];
        Ok(Value::Bool(
            matcher.is_match(Path::new(OsStr::from_bytes(&bytes))),
        ))
    }

    fn regexp(&mut self, subject: Value, pattern: Value) -> Result<Value> {
        if subject.is_null() || pattern.is_null() {
            return Ok(Value::Null);
        }
        let Value::Text(pattern_text) = &pattern else {
            return Err(mismatch("REGEXP", &subject, &pattern));
        };
        let bytes = text_bytes(&subject).ok_or_else(|| mismatch("REGEXP", &subject, &pattern))?;
        if !self.regexes.contains_key(pattern_text) {
            if self.regexes.len() >= 64 {
                self.regexes.clear();
            }
            let regex = Regex::new(pattern_text).map_err(|e| Error::InvalidPattern {
                pattern: pattern_text.clone(),
                reason: e.to_string(),
            })?;
            self.regexes.insert(pattern_text.clone(), regex);
        }
        Ok(Value::Bool(self.regexes[pattern_text].is_match(&bytes)))
    }

    fn interval(&mut self, interval: &Interval, row: &dyn Row) -> Result<Value> {
        let amount = self.eval(&interval.value, row)?;
        let (amount, unit_text): (f64, Option<String>) = match amount {
            Value::Null => return Ok(Value::Null),
            Value::Int(n) => (n as f64, None),
            Value::Float(f) => (f, None),
            Value::Text(text) => {
                let trimmed = text.trim();
                let split = trimmed
                    .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
                    .unwrap_or(trimmed.len());
                let number: f64 = trimmed[..split]
                    .parse()
                    .map_err(|_| Error::IntervalUnit(text.clone()))?;
                let unit = trimmed[split..].trim().to_ascii_lowercase();
                (number, if unit.is_empty() { None } else { Some(unit) })
            }
            other => return Err(mismatch("INTERVAL", &other, &Value::Null)),
        };
        let unit = match (&interval.leading_field, unit_text) {
            (Some(field), _) => {
                let name = field.to_string().to_ascii_lowercase();
                let name = name.split('(').next().unwrap_or(&name).trim().to_owned();
                unit_nanos(&name).ok_or(Error::IntervalUnit(name))?
            }
            (None, Some(name)) => unit_nanos(&name).ok_or(Error::IntervalUnit(name))?,
            (None, None) => return Err(Error::IntervalUnit("missing".to_owned())),
        };
        let nanos = amount * unit as f64;
        if !nanos.is_finite() || nanos.abs() > i64::MAX as f64 {
            return Err(Error::Overflow("interval".to_owned()));
        }
        Ok(Value::Int(nanos.round() as i64))
    }

    fn function(&mut self, function: &Function, row: &dyn Row) -> Result<Value> {
        crate::bind::function_shape(function)?;
        let name = function.name.to_string().to_ascii_lowercase();
        if is_aggregate(&name) {
            return self
                .aggregates
                .get(&function.to_string())
                .cloned()
                .ok_or(Error::AggregateOutsidePlan(name));
        }
        let args = self.args(&function.args, row)?;
        let arity = |expected: usize| -> Result<()> {
            if args.len() == expected {
                Ok(())
            } else {
                Err(Error::Arity {
                    function: name.clone(),
                    expected,
                    got: args.len(),
                })
            }
        };
        match name.as_str() {
            "now" | "current_timestamp" => {
                arity(0)?;
                Ok(Value::Timestamp(self.now))
            }
            "lower" => {
                arity(1)?;
                Ok(map_text(
                    &args[0],
                    |s| s.to_lowercase(),
                    |b| b.to_ascii_lowercase(),
                ))
            }
            "upper" => {
                arity(1)?;
                Ok(map_text(
                    &args[0],
                    |s| s.to_uppercase(),
                    |b| b.to_ascii_uppercase(),
                ))
            }
            "length" => {
                arity(1)?;
                Ok(match &args[0] {
                    Value::Null => Value::Null,
                    Value::Text(s) => {
                        Value::Int(i64::try_from(s.chars().count()).unwrap_or(i64::MAX))
                    }
                    Value::Blob(b) => Value::Int(i64::try_from(b.len()).unwrap_or(i64::MAX)),
                    other => Value::Int(i64::try_from(render(other).len()).unwrap_or(i64::MAX)),
                })
            }
            "coalesce" => {
                if args.is_empty() {
                    return Err(Error::Arity {
                        function: name,
                        expected: 1,
                        got: 0,
                    });
                }
                Ok(args
                    .into_iter()
                    .find(|v| !v.is_null())
                    .unwrap_or(Value::Null))
            }
            "ifnull" => {
                arity(2)?;
                let mut args = args.into_iter();
                let first = args.next().unwrap_or(Value::Null);
                let second = args.next().unwrap_or(Value::Null);
                Ok(if first.is_null() { second } else { first })
            }
            "nullif" => {
                arity(2)?;
                let equal = compare(&BinaryOperator::Eq, args[0].clone(), args[1].clone())?;
                Ok(if equal == Value::Bool(true) {
                    Value::Null
                } else {
                    args[0].clone()
                })
            }
            "abs" => {
                arity(1)?;
                Ok(match &args[0] {
                    Value::Null => Value::Null,
                    Value::Int(n) => Value::Int(
                        n.checked_abs()
                            .ok_or_else(|| Error::Overflow("abs".to_owned()))?,
                    ),
                    Value::Float(f) => Value::Float(f.abs()),
                    other => return Err(mismatch("abs", other, &Value::Null)),
                })
            }
            "starts_with" | "ends_with" | "contains" => {
                arity(2)?;
                if args[0].is_null() || args[1].is_null() {
                    return Ok(Value::Null);
                }
                let haystack =
                    text_bytes(&args[0]).ok_or_else(|| mismatch(&name, &args[0], &args[1]))?;
                let needle =
                    text_bytes(&args[1]).ok_or_else(|| mismatch(&name, &args[0], &args[1]))?;
                Ok(Value::Bool(match name.as_str() {
                    "starts_with" => haystack.starts_with(&needle),
                    "ends_with" => haystack.ends_with(&needle),
                    _ => needle.is_empty() || haystack.windows(needle.len()).any(|w| w == &*needle),
                }))
            }
            "replace" => {
                arity(3)?;
                if args.iter().any(Value::is_null) {
                    return Ok(Value::Null);
                }
                let subject = render(&args[0]);
                let from = render(&args[1]);
                let to = render(&args[2]);
                Ok(Value::Text(if from.is_empty() {
                    subject
                } else {
                    subject.replace(&from, &to)
                }))
            }
            "substr" | "substring" => substr_values(&name, &args),
            "trim" | "ltrim" | "rtrim" => {
                if args.is_empty() || args.len() > 2 {
                    return Err(Error::Arity {
                        function: name,
                        expected: 1,
                        got: args.len(),
                    });
                }
                let side = match name.as_str() {
                    "ltrim" => "leading",
                    "rtrim" => "trailing",
                    _ => "both",
                };
                trim(args[0].clone(), args.get(1).cloned(), side)
            }
            "basename" | "dirname" | "extension" => {
                arity(1)?;
                if args[0].is_null() {
                    return Ok(Value::Null);
                }
                let bytes =
                    text_bytes(&args[0]).ok_or_else(|| mismatch(&name, &args[0], &Value::Null))?;
                let path = Path::new(OsStr::from_bytes(&bytes));
                let part = match name.as_str() {
                    "basename" => path.file_name().map(OsStr::as_bytes),
                    "dirname" => path.parent().map(|p| p.as_os_str().as_bytes()),
                    _ => path.extension().map(OsStr::as_bytes),
                };
                Ok(part.map(from_bytes).unwrap_or(Value::Null))
            }
            "human" | "format_size" => {
                arity(1)?;
                Ok(match &args[0] {
                    Value::Null => Value::Null,
                    Value::Int(n) => Value::Text(format_size(*n)),
                    Value::Float(f) => Value::Text(format_size(*f as i64)),
                    other => return Err(mismatch(&name, other, &Value::Null)),
                })
            }
            "oct" => {
                arity(1)?;
                Ok(match &args[0] {
                    Value::Null => Value::Null,
                    Value::Int(n) => Value::Text(format!("{:o}", n & 0o7777)),
                    other => return Err(mismatch(&name, other, &Value::Null)),
                })
            }
            "typeof" => {
                arity(1)?;
                Ok(Value::Text(type_name(&args[0]).to_owned()))
            }
            _ => Err(Error::UnknownFunction(name)),
        }
    }

    fn args(&mut self, args: &FunctionArguments, row: &dyn Row) -> Result<Vec<Value>> {
        match args {
            FunctionArguments::None => Ok(Vec::new()),
            FunctionArguments::Subquery(_) => {
                Err(Error::Unsupported("subquery argument".to_owned()))
            }
            FunctionArguments::List(list) => list
                .args
                .iter()
                .map(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => self.eval(expr, row),
                    other => Err(Error::Unsupported(format!("argument `{other}`"))),
                })
                .collect(),
        }
    }
}

fn column(name: &str, row: &dyn Row) -> Result<Value> {
    row.column(&name.to_ascii_lowercase())
}

fn literal_value(literal: &Literal) -> Result<Value> {
    match literal {
        Literal::Number(text, _) => {
            let cleaned: String = text.chars().filter(|c| *c != '_').collect();
            if let Ok(n) = cleaned.parse::<i64>() {
                return Ok(Value::Int(n));
            }
            cleaned
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| Error::Unsupported(format!("number `{text}`")))
        }
        Literal::SingleQuotedString(s) | Literal::DoubleQuotedString(s) => {
            Ok(Value::Text(s.clone()))
        }
        Literal::Boolean(b) => Ok(Value::Bool(*b)),
        Literal::Null => Ok(Value::Null),
        Literal::HexStringLiteral(hex) => {
            let digits: Vec<u8> = hex
                .bytes()
                .map(|b| char::from(b).to_digit(16).map(|d| d as u8))
                .collect::<Option<_>>()
                .ok_or_else(|| Error::Unsupported(format!("hex literal `{hex}`")))?;
            if !digits.len().is_multiple_of(2) {
                return Err(Error::Unsupported(format!("hex literal `{hex}`")));
            }
            Ok(Value::Blob(
                digits
                    .chunks(2)
                    .map(|pair| pair[0] << 4 | pair[1])
                    .collect(),
            ))
        }
        other => Err(Error::Unsupported(format!("literal `{other}`"))),
    }
}

fn unary(op: &UnaryOperator, operand: Value) -> Result<Value> {
    match op {
        UnaryOperator::Not => Ok(from_truth(value::not(operand.truth()))),
        UnaryOperator::Minus => match operand {
            Value::Null => Ok(Value::Null),
            Value::Int(n) => n
                .checked_neg()
                .map(Value::Int)
                .ok_or_else(|| Error::Overflow("negate".to_owned())),
            Value::Float(f) => Ok(Value::Float(-f)),
            other => Err(mismatch("-", &other, &Value::Null)),
        },
        UnaryOperator::Plus => match operand {
            Value::Null | Value::Int(_) | Value::Float(_) => Ok(operand),
            other => Err(mismatch("+", &other, &Value::Null)),
        },
        UnaryOperator::BitwiseNot => match operand {
            Value::Null => Ok(Value::Null),
            Value::Int(n) => Ok(Value::Int(!n)),
            other => Err(mismatch("~", &other, &Value::Null)),
        },
        other => Err(Error::Unsupported(format!("operator `{other}`"))),
    }
}

fn compare(op: &BinaryOperator, l: Value, r: Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let (l, r) = coerce_pair(l, r)?;
    if !comparable(&l, &r) {
        return Err(mismatch(&op.to_string(), &l, &r));
    }
    let Some(ordering) = l.compare(&r) else {
        return Ok(Value::Null);
    };
    Ok(Value::Bool(match op {
        BinaryOperator::Eq => ordering.is_eq(),
        BinaryOperator::NotEq => ordering.is_ne(),
        BinaryOperator::Gt => ordering.is_gt(),
        BinaryOperator::Lt => ordering.is_lt(),
        BinaryOperator::GtEq => ordering.is_ge(),
        _ => ordering.is_le(),
    }))
}

fn coerce_pair(l: Value, r: Value) -> Result<(Value, Value)> {
    match (&l, &r) {
        (Value::Timestamp(_), Value::Text(text)) => {
            let parsed =
                time::parse_iso(text).ok_or_else(|| Error::InvalidTimestamp(text.clone()))?;
            Ok((l, Value::Timestamp(parsed)))
        }
        (Value::Text(text), Value::Timestamp(_)) => {
            let parsed =
                time::parse_iso(text).ok_or_else(|| Error::InvalidTimestamp(text.clone()))?;
            Ok((Value::Timestamp(parsed), r))
        }
        _ => Ok((l, r)),
    }
}

fn comparable(l: &Value, r: &Value) -> bool {
    matches!(
        (l, r),
        (
            Value::Int(_) | Value::Float(_),
            Value::Int(_) | Value::Float(_)
        ) | (
            Value::Text(_) | Value::Blob(_),
            Value::Text(_) | Value::Blob(_)
        ) | (Value::Bool(_), Value::Bool(_))
            | (Value::Timestamp(_), Value::Timestamp(_))
    )
}

fn arithmetic(op: &BinaryOperator, l: Value, r: Value) -> Result<Value> {
    let name = op.to_string();
    match (&l, &r) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Int(a), Value::Int(b)) => {
            let (a, b) = (*a, *b);
            let result = match op {
                BinaryOperator::Plus => a.checked_add(b),
                BinaryOperator::Minus => a.checked_sub(b),
                BinaryOperator::Multiply => a.checked_mul(b),
                BinaryOperator::Divide => {
                    if b == 0 {
                        return Err(Error::DivideByZero);
                    }
                    a.checked_div(b)
                }
                _ => {
                    if b == 0 {
                        return Err(Error::DivideByZero);
                    }
                    a.checked_rem(b)
                }
            };
            result.map(Value::Int).ok_or(Error::Overflow(name))
        }
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            let a = as_float(&l);
            let b = as_float(&r);
            let result = match op {
                BinaryOperator::Plus => a + b,
                BinaryOperator::Minus => a - b,
                BinaryOperator::Multiply => a * b,
                BinaryOperator::Divide => {
                    if b == 0.0 {
                        return Err(Error::DivideByZero);
                    }
                    a / b
                }
                _ => {
                    if b == 0.0 {
                        return Err(Error::DivideByZero);
                    }
                    a % b
                }
            };
            Ok(Value::Float(result))
        }
        (Value::Timestamp(t), Value::Int(n)) => match op {
            BinaryOperator::Plus => t.0.checked_add(*n),
            BinaryOperator::Minus => t.0.checked_sub(*n),
            _ => return Err(mismatch(&name, &l, &r)),
        }
        .map(|n| Value::Timestamp(Nanos(n)))
        .ok_or(Error::Overflow(name)),
        (Value::Int(n), Value::Timestamp(t)) if matches!(op, BinaryOperator::Plus) => {
            t.0.checked_add(*n)
                .map(|n| Value::Timestamp(Nanos(n)))
                .ok_or(Error::Overflow(name))
        }
        (Value::Timestamp(a), Value::Timestamp(b)) if matches!(op, BinaryOperator::Minus) => {
            a.0.checked_sub(b.0)
                .map(Value::Int)
                .ok_or(Error::Overflow(name))
        }
        _ => Err(mismatch(&name, &l, &r)),
    }
}

fn bitwise(op: &BinaryOperator, l: Value, r: Value) -> Result<Value> {
    match (&l, &r) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Int(a), Value::Int(b)) => Ok(Value::Int(match op {
            BinaryOperator::BitwiseAnd => a & b,
            BinaryOperator::BitwiseOr => a | b,
            _ => a ^ b,
        })),
        (Value::Bool(a), Value::Bool(b)) if matches!(op, BinaryOperator::Xor) => {
            Ok(Value::Bool(a ^ b))
        }
        _ => Err(mismatch(&op.to_string(), &l, &r)),
    }
}

fn concat(l: Value, r: Value) -> Result<Value> {
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }
    let mut bytes = text_bytes(&l)
        .ok_or_else(|| mismatch("||", &l, &r))?
        .into_owned();
    bytes.extend_from_slice(&text_bytes(&r).ok_or_else(|| mismatch("||", &l, &r))?);
    Ok(from_bytes(&bytes))
}

fn cast(value: Value, type_name: &str) -> Result<Value> {
    let target = type_name.to_ascii_lowercase();
    let target = target
        .split('(')
        .next()
        .unwrap_or(&target)
        .trim()
        .to_owned();
    if value.is_null() {
        return Ok(Value::Null);
    }
    let fail = || Error::TypeMismatch {
        operation: format!("cast to {target}"),
        left: type_name_owned(&value),
        right: target.clone(),
    };
    match target.as_str() {
        "int" | "integer" | "bigint" | "smallint" | "int8" | "int4" => match &value {
            Value::Int(_) => Ok(value),
            Value::Float(f) if f.is_finite() => Ok(Value::Int(f.trunc() as i64)),
            Value::Bool(b) => Ok(Value::Int(i64::from(*b))),
            Value::Text(s) => s.trim().parse().map(Value::Int).map_err(|_| fail()),
            Value::Timestamp(t) => Ok(Value::Int(t.0)),
            _ => Err(fail()),
        },
        "real" | "double" | "double precision" | "float" | "numeric" | "decimal" => match &value {
            Value::Int(n) => Ok(Value::Float(*n as f64)),
            Value::Float(_) => Ok(value),
            Value::Text(s) => s.trim().parse().map(Value::Float).map_err(|_| fail()),
            _ => Err(fail()),
        },
        "text" | "varchar" | "char" | "string" => Ok(Value::Text(render(&value))),
        "bool" | "boolean" => match &value {
            Value::Bool(_) => Ok(value),
            Value::Int(n) => Ok(Value::Bool(*n != 0)),
            Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" | "yes" => Ok(Value::Bool(true)),
                "false" | "f" | "0" | "no" => Ok(Value::Bool(false)),
                _ => Err(fail()),
            },
            _ => Err(fail()),
        },
        "timestamp" | "datetime" => match &value {
            Value::Timestamp(_) => Ok(value),
            Value::Int(n) => Ok(Value::Timestamp(Nanos(*n))),
            Value::Text(s) => time::parse_iso(s)
                .map(Value::Timestamp)
                .ok_or_else(|| Error::InvalidTimestamp(s.clone())),
            _ => Err(fail()),
        },
        _ => Err(Error::Unsupported(format!("cast to `{type_name}`"))),
    }
}

fn like_match<T: LikeChar + PartialEq>(text: &[T], pattern: &[T], escape: Option<T>) -> bool {
    enum Token<T> {
        Any,
        One,
        Literal(T),
    }
    let mut tokens: Vec<Token<T>> = Vec::with_capacity(pattern.len());
    let mut i = 0;
    while i < pattern.len() {
        let ch = pattern[i];
        if Some(ch) == escape && i + 1 < pattern.len() {
            tokens.push(Token::Literal(pattern[i + 1]));
            i += 2;
            continue;
        }
        tokens.push(match ch.like_class() {
            LikeClass::Percent => Token::Any,
            LikeClass::Underscore => Token::One,
            LikeClass::Other => Token::Literal(ch),
        });
        i += 1;
    }
    let (mut t, mut p) = (0usize, 0usize);
    let mut backtrack: Option<(usize, usize)> = None;
    loop {
        if p < tokens.len() {
            match tokens[p] {
                Token::Any => {
                    backtrack = Some((p, t));
                    p += 1;
                    continue;
                }
                Token::One if t < text.len() => {
                    p += 1;
                    t += 1;
                    continue;
                }
                Token::Literal(expected) if t < text.len() && text[t] == expected => {
                    p += 1;
                    t += 1;
                    continue;
                }
                _ => {}
            }
        } else if t == text.len() {
            return true;
        }
        match backtrack {
            Some((bp, bt)) if bt < text.len() => {
                backtrack = Some((bp, bt + 1));
                p = bp + 1;
                t = bt + 1;
            }
            _ => return false,
        }
    }
}

enum LikeClass {
    Percent,
    Underscore,
    Other,
}

trait LikeChar: Copy {
    fn like_class(self) -> LikeClass;
}

impl LikeChar for char {
    fn like_class(self) -> LikeClass {
        match self {
            '%' => LikeClass::Percent,
            '_' => LikeClass::Underscore,
            _ => LikeClass::Other,
        }
    }
}

impl LikeChar for u8 {
    fn like_class(self) -> LikeClass {
        match self {
            b'%' => LikeClass::Percent,
            b'_' => LikeClass::Underscore,
            _ => LikeClass::Other,
        }
    }
}

fn unit_nanos(unit: &str) -> Option<i64> {
    Some(match unit {
        "ns" | "nanosecond" | "nanoseconds" => 1,
        "us" | "microsecond" | "microseconds" => 1_000,
        "ms" | "millisecond" | "milliseconds" => 1_000_000,
        "s" | "sec" | "secs" | "second" | "seconds" => time::NANOS_PER_SECOND,
        "min" | "mins" | "minute" | "minutes" => time::NANOS_PER_MINUTE,
        "h" | "hr" | "hrs" | "hour" | "hours" => time::NANOS_PER_HOUR,
        "d" | "day" | "days" => time::NANOS_PER_DAY,
        "w" | "wk" | "wks" | "week" | "weeks" => time::NANOS_PER_WEEK,
        _ => return None,
    })
}

fn from_truth(truth: Option<bool>) -> Value {
    truth.map(Value::Bool).unwrap_or(Value::Null)
}

fn as_float(value: &Value) -> f64 {
    match value {
        Value::Int(n) => *n as f64,
        Value::Float(f) => *f,
        _ => f64::NAN,
    }
}

fn text_bytes(value: &Value) -> Option<Cow<'_, [u8]>> {
    match value {
        Value::Null => None,
        Value::Text(s) => Some(Cow::Borrowed(s.as_bytes())),
        Value::Blob(b) => Some(Cow::Borrowed(b)),
        other => Some(Cow::Owned(render(other).into_bytes())),
    }
}

pub fn render(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Blob(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Timestamp(t) => time::format_iso(*t),
    }
}

pub fn from_bytes(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(s) => Value::Text(s.to_owned()),
        Err(_) => Value::Blob(bytes.to_vec()),
    }
}

fn map_text(
    value: &Value,
    text: impl Fn(&str) -> String,
    blob: impl Fn(&[u8]) -> Vec<u8>,
) -> Value {
    match value {
        Value::Null => Value::Null,
        Value::Text(s) => Value::Text(text(s)),
        Value::Blob(b) => Value::Blob(blob(b)),
        other => Value::Text(text(&render(other))),
    }
}

pub fn format_size(bytes: i64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let negative = bytes < 0;
    let mut value = bytes.unsigned_abs() as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let sign = if negative { "-" } else { "" };
    if unit == 0 {
        format!("{sign}{} {}", bytes.unsigned_abs(), UNITS[unit])
    } else if value < 10.0 {
        format!("{sign}{value:.1} {}", UNITS[unit])
    } else {
        format!("{sign}{value:.0} {}", UNITS[unit])
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Text(_) => "text",
        Value::Blob(_) => "blob",
        Value::Timestamp(_) => "timestamp",
    }
}

fn type_name_owned(value: &Value) -> String {
    type_name(value).to_owned()
}

fn mismatch(operation: &str, l: &Value, r: &Value) -> Error {
    Error::TypeMismatch {
        operation: operation.to_owned(),
        left: type_name_owned(l),
        right: type_name_owned(r),
    }
}

fn substr_values(name: &str, args: &[Value]) -> Result<Value> {
    if args.len() != 2 && args.len() != 3 {
        return Err(Error::Arity {
            function: name.to_owned(),
            expected: 2,
            got: args.len(),
        });
    }
    if args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let subject: Vec<char> = render(&args[0]).chars().collect();
    let Value::Int(start) = args[1] else {
        return Err(mismatch(name, &args[0], &args[1]));
    };
    let length = match args.get(2) {
        None => None,
        Some(Value::Int(n)) => Some(*n),
        Some(other) => return Err(mismatch(name, &args[0], other)),
    };
    let begin = usize::try_from((start - 1).max(0))
        .unwrap_or(0)
        .min(subject.len());
    let end = match length {
        None => subject.len(),
        Some(n) => begin
            .saturating_add(usize::try_from(n.max(0)).unwrap_or(0))
            .min(subject.len()),
    };
    Ok(Value::Text(subject[begin..end].iter().collect()))
}

fn trim(subject: Value, what: Option<Value>, side: &str) -> Result<Value> {
    if subject.is_null() || what.as_ref().is_some_and(Value::is_null) {
        return Ok(Value::Null);
    }
    let text = render(&subject);
    let set: Vec<char> = match &what {
        Some(what) => render(what).chars().collect(),
        None => Vec::new(),
    };
    let matches = |c: char| {
        if set.is_empty() {
            c.is_whitespace()
        } else {
            set.contains(&c)
        }
    };
    let trimmed = match side {
        "leading" => text.trim_start_matches(matches),
        "trailing" => text.trim_end_matches(matches),
        _ => text.trim_matches(matches),
    };
    Ok(Value::Text(trimmed.to_owned()))
}

fn extract(field: &str, subject: Value) -> Result<Value> {
    let nanos = match subject {
        Value::Null => return Ok(Value::Null),
        Value::Timestamp(t) => t,
        other => return Err(mismatch("extract", &other, &Value::Null)),
    };
    let field = field.to_ascii_lowercase();
    let field = field.split('(').next().unwrap_or(&field).trim();
    let days = nanos.0.div_euclid(time::NANOS_PER_DAY);
    let rem = nanos.0.rem_euclid(time::NANOS_PER_DAY);
    let (year, month, day) = time::civil_from_days(days);
    let value = match field {
        "year" | "years" => year,
        "month" | "months" => i64::from(month),
        "day" | "days" => i64::from(day),
        "hour" | "hours" => rem / time::NANOS_PER_HOUR,
        "minute" | "minutes" => (rem % time::NANOS_PER_HOUR) / time::NANOS_PER_MINUTE,
        "second" | "seconds" => (rem % time::NANOS_PER_MINUTE) / time::NANOS_PER_SECOND,
        "millisecond" | "milliseconds" => (rem % time::NANOS_PER_SECOND) / 1_000_000,
        "microsecond" | "microseconds" => (rem % time::NANOS_PER_SECOND) / 1_000,
        "nanosecond" | "nanoseconds" => rem % time::NANOS_PER_SECOND,
        "epoch" => nanos.0.div_euclid(time::NANOS_PER_SECOND),
        "dow" | "dayofweek" | "day_of_week" => (days + 4).rem_euclid(7),
        "doy" | "dayofyear" | "day_of_year" => days - time::days_from_civil(year, 1, 1) + 1,
        _ => return Err(Error::Unsupported(format!("extract `{field}`"))),
    };
    Ok(Value::Int(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::FsqlDialect;
    use sqlparser::parser::Parser;

    struct MapRow(HashMap<String, Value>);

    impl Row for MapRow {
        fn column(&self, name: &str) -> Result<Value> {
            self.0
                .get(name)
                .cloned()
                .ok_or_else(|| Error::UnknownColumn(name.to_owned()))
        }
    }

    fn row() -> MapRow {
        let mut map = HashMap::new();
        map.insert(
            "path".to_owned(),
            Value::Text("/home/kjanat/projects/a/build/x.tmp".to_owned()),
        );
        map.insert("name".to_owned(), Value::Text("x.tmp".to_owned()));
        map.insert("ext".to_owned(), Value::Text("tmp".to_owned()));
        map.insert("kind".to_owned(), Value::Text("file".to_owned()));
        map.insert("size".to_owned(), Value::Int(2 * 1024 * 1024 * 1024));
        map.insert("mode".to_owned(), Value::Int(0o100644));
        map.insert(
            "mtime".to_owned(),
            Value::Timestamp(time::parse_iso("2026-07-20T00:00:00Z").expect("mtime")),
        );
        map.insert("target".to_owned(), Value::Null);
        MapRow(map)
    }

    fn eval(sql: &str) -> Result<Value> {
        let expr = Parser::new(&FsqlDialect)
            .try_with_sql(sql)
            .expect("tokenize")
            .parse_expr()
            .expect("parse");
        let now = time::parse_iso("2026-07-30T12:00:00Z").expect("now");
        Evaluator::new(now).eval(&expr, &row())
    }

    fn truth(sql: &str) -> Option<bool> {
        eval(sql).expect(sql).truth()
    }

    #[test]
    fn byte_literals_compare_against_size() {
        assert_eq!(truth("size > 1g"), Some(true));
        assert_eq!(truth("size > 3g"), Some(false));
        assert_eq!(truth("size between 1g and 3g"), Some(true));
    }

    #[test]
    fn octal_literals_mask_mode() {
        assert_eq!(truth("mode & 0o777 = 0o644"), Some(true));
        assert_eq!(
            eval("oct(mode)").expect("oct"),
            Value::Text("644".to_owned())
        );
    }

    #[test]
    fn interval_arithmetic_on_timestamps() {
        assert_eq!(truth("mtime > now() - interval '7' day"), Some(false));
        assert_eq!(truth("mtime > now() - interval '2 weeks'"), Some(true));
        assert_eq!(truth("mtime < now() - interval 240 hour"), Some(true));
        assert_eq!(
            eval("interval '1.5 h'").expect("interval"),
            Value::Int(90 * time::NANOS_PER_MINUTE)
        );
    }

    #[test]
    fn iso_text_coerces_against_timestamps() {
        assert_eq!(truth("mtime > '2026-07-01'"), Some(true));
        assert_eq!(truth("'2026-08-01' > mtime"), Some(true));
        assert!(matches!(
            eval("mtime > 'yesterday'"),
            Err(Error::InvalidTimestamp(_))
        ));
    }

    #[test]
    fn glob_regexp_and_like() {
        assert_eq!(truth("path glob '*/build/*.tmp'"), Some(true));
        assert_eq!(truth("path glob '*.rs'"), Some(false));
        assert_eq!(truth("name regexp '^x\\.'"), Some(true));
        assert_eq!(truth("name like 'x_t%'"), Some(true));
        assert_eq!(truth("name like 'X%'"), Some(false));
        assert_eq!(truth("name ilike 'X%'"), Some(true));
        assert_eq!(truth("name not like '%.rs'"), Some(true));
        assert_eq!(truth("'50%' like '50\\%' escape '\\'"), Some(true));
        assert_eq!(truth("'50x' like '50\\%' escape '\\'"), Some(false));
    }

    #[test]
    fn like_handles_multibyte_underscore() {
        assert_eq!(truth("'héllo' like 'h_llo'"), Some(true));
    }

    #[test]
    fn null_semantics() {
        assert_eq!(truth("target is null"), Some(true));
        assert_eq!(truth("target = 'x'"), None);
        assert_eq!(truth("target = 'x' or true"), Some(true));
        assert_eq!(truth("target = 'x' and false"), Some(false));
        assert_eq!(truth("kind in ('dir', null)"), None);
        assert_eq!(truth("kind in ('dir', 'file')"), Some(true));
        assert_eq!(truth("kind not in ('dir')"), Some(true));
        assert_eq!(
            eval("coalesce(target, ext)").expect("coalesce"),
            Value::Text("tmp".to_owned())
        );
    }

    #[test]
    fn mismatched_types_are_loud() {
        assert!(matches!(
            eval("size > 'big'"),
            Err(Error::TypeMismatch { .. })
        ));
        assert!(matches!(eval("size / 0"), Err(Error::DivideByZero)));
        assert!(matches!(
            eval("count(*)"),
            Err(Error::AggregateOutsidePlan(_))
        ));
        assert!(matches!(eval("nope(1)"), Err(Error::UnknownFunction(_))));
        assert!(matches!(eval("nope = 1"), Err(Error::UnknownColumn(_))));
        assert!(matches!(eval("lower()"), Err(Error::Arity { .. })));
    }

    #[test]
    fn scalar_functions() {
        assert_eq!(
            eval("upper(ext)").expect("upper"),
            Value::Text("TMP".to_owned())
        );
        assert_eq!(eval("length(name)").expect("length"), Value::Int(5));
        assert_eq!(
            eval("human(size)").expect("human"),
            Value::Text("2.0 GiB".to_owned())
        );
        assert_eq!(
            eval("human(1536)").expect("human"),
            Value::Text("1.5 KiB".to_owned())
        );
        assert_eq!(
            eval("human(512)").expect("human"),
            Value::Text("512 B".to_owned())
        );
        assert_eq!(
            eval("basename(path)").expect("basename"),
            Value::Text("x.tmp".to_owned())
        );
        assert_eq!(
            eval("dirname(path)").expect("dirname"),
            Value::Text("/home/kjanat/projects/a/build".to_owned())
        );
        assert_eq!(
            eval("substr(name, 2, 2)").expect("substr"),
            Value::Text(".t".to_owned())
        );
        assert_eq!(
            eval("replace(name, 'tmp', 'rs')").expect("replace"),
            Value::Text("x.rs".to_owned())
        );
        assert_eq!(
            eval("name || '.bak'").expect("concat"),
            Value::Text("x.tmp.bak".to_owned())
        );
        assert_eq!(truth("starts_with(path, '/home')"), Some(true));
        assert_eq!(truth("ends_with(path, '.rs')"), Some(false));
        assert_eq!(truth("contains(path, 'build')"), Some(true));
        assert_eq!(
            eval("typeof(size)").expect("typeof"),
            Value::Text("int".to_owned())
        );
        assert_eq!(
            eval("cast(size as text)").expect("cast"),
            Value::Text("2147483648".to_owned())
        );
        assert_eq!(eval("cast('42' as int)").expect("cast"), Value::Int(42));
    }

    #[test]
    fn special_form_functions() {
        assert_eq!(
            eval("substring(name from 2 for 2)").expect("substring"),
            Value::Text(".t".to_owned())
        );
        assert_eq!(
            eval("trim('  x  ')").expect("trim"),
            Value::Text("x".to_owned())
        );
        assert_eq!(
            eval("trim(leading '/' from path)").expect("trim leading"),
            Value::Text("home/kjanat/projects/a/build/x.tmp".to_owned())
        );
        assert_eq!(
            eval("rtrim(name, 'pmt')").expect("rtrim"),
            Value::Text("x.".to_owned())
        );
        assert_eq!(
            eval("extract(year from mtime)").expect("year"),
            Value::Int(2026)
        );
        assert_eq!(
            eval("extract(month from mtime)").expect("month"),
            Value::Int(7)
        );
        assert_eq!(
            eval("extract(day from mtime)").expect("day"),
            Value::Int(20)
        );
        assert_eq!(eval("extract(dow from mtime)").expect("dow"), Value::Int(1));
        assert_eq!(
            eval("extract(doy from mtime)").expect("doy"),
            Value::Int(201)
        );
        assert_eq!(
            eval("extract(epoch from mtime)").expect("epoch"),
            Value::Int(1_784_505_600)
        );
    }

    #[test]
    fn integer_and_float_arithmetic() {
        assert_eq!(eval("7 / 2").expect("div"), Value::Int(3));
        assert_eq!(eval("7 / 2.0").expect("div"), Value::Float(3.5));
        assert_eq!(eval("7 % 4").expect("mod"), Value::Int(3));
        assert_eq!(
            eval("-size").expect("neg"),
            Value::Int(-(2 * 1024 * 1024 * 1024))
        );
        assert!(matches!(
            eval("9223372036854775807 + 1"),
            Err(Error::Overflow(_))
        ));
        assert_eq!(eval("mtime - mtime").expect("ts diff"), Value::Int(0));
    }
}
