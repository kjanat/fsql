use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
    Timestamp(Nanos),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Nanos(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Bool,
    Int,
    Float,
    Text,
    Blob,
    Timestamp,
}

impl Value {
    pub fn type_of(&self) -> Option<Type> {
        match self {
            Self::Null => None,
            Self::Bool(_) => Some(Type::Bool),
            Self::Int(_) => Some(Type::Int),
            Self::Float(_) => Some(Type::Float),
            Self::Text(_) => Some(Type::Text),
            Self::Blob(_) => Some(Type::Blob),
            Self::Timestamp(_) => Some(Type::Timestamp),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn truth(&self) -> Option<bool> {
        match self {
            Self::Null => None,
            Self::Bool(b) => Some(*b),
            Self::Int(n) => Some(*n != 0),
            Self::Float(f) => Some(*f != 0.0),
            Self::Text(s) => Some(!s.is_empty()),
            Self::Blob(b) => Some(!b.is_empty()),
            Self::Timestamp(_) => Some(true),
        }
    }

    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Text(s) => Some(s.as_bytes()),
            Self::Blob(b) => Some(b),
            _ => None,
        }
    }

    pub fn compare(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Null, _) | (_, Self::Null) => None,
            (Self::Bool(a), Self::Bool(b)) => Some(a.cmp(b)),
            (Self::Int(a), Self::Int(b)) => Some(a.cmp(b)),
            (Self::Timestamp(a), Self::Timestamp(b)) => Some(a.cmp(b)),
            (Self::Float(a), Self::Float(b)) => a.partial_cmp(b),
            (Self::Int(a), Self::Float(b)) => (*a as f64).partial_cmp(b),
            (Self::Float(a), Self::Int(b)) => a.partial_cmp(&(*b as f64)),
            (a, b) => match (a.bytes(), b.bytes()) {
                (Some(a), Some(b)) => Some(a.cmp(b)),
                _ => None,
            },
        }
    }
}

pub fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

pub fn or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

pub fn not(a: Option<bool>) -> Option<bool> {
    a.map(|b| !b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_comparisons_are_unknown() {
        assert_eq!(Value::Null.compare(&Value::Int(1)), None);
        assert_eq!(Value::Int(1).compare(&Value::Null), None);
    }

    #[test]
    fn text_and_blob_compare_bytewise() {
        let text = Value::Text("abc".to_owned());
        let blob = Value::Blob(b"abd".to_vec());
        assert_eq!(text.compare(&blob), Some(Ordering::Less));
    }

    #[test]
    fn int_and_float_compare_numerically() {
        assert_eq!(
            Value::Int(2).compare(&Value::Float(2.5)),
            Some(Ordering::Less)
        );
        assert_eq!(
            Value::Float(2.5).compare(&Value::Int(2)),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn nan_compares_as_unknown() {
        assert_eq!(Value::Float(f64::NAN).compare(&Value::Int(1)), None);
    }

    #[test]
    fn three_valued_and_short_circuits_on_false() {
        assert_eq!(and(Some(false), None), Some(false));
        assert_eq!(and(None, Some(false)), Some(false));
        assert_eq!(and(Some(true), None), None);
    }

    #[test]
    fn three_valued_or_short_circuits_on_true() {
        assert_eq!(or(Some(true), None), Some(true));
        assert_eq!(or(None, Some(true)), Some(true));
        assert_eq!(or(Some(false), None), None);
    }

    #[test]
    fn not_unknown_stays_unknown() {
        assert_eq!(not(None), None);
        assert_eq!(not(Some(true)), Some(false));
    }
}
