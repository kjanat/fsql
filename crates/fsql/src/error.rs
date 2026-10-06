use std::fmt;
use std::path::PathBuf;

use sqlparser::parser::ParserError;

#[derive(Debug)]
pub enum Error {
    Parse(ParserError),
    UnknownColumn(String),
    UnknownTable(String),
    UnknownFunction(String),
    Arity {
        function: String,
        expected: usize,
        got: usize,
    },
    TypeMismatch {
        operation: String,
        left: String,
        right: String,
    },
    InvalidPattern {
        pattern: String,
        reason: String,
    },
    InvalidTimestamp(String),
    IntervalUnit(String),
    Overflow(String),
    DivideByZero,
    AggregateOutsidePlan(String),
    Unsupported(String),
    Plan(String),
    Stale(PathBuf),
    Walk {
        root: PathBuf,
        reason: String,
    },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "{e}"),
            Self::UnknownColumn(name) => write!(f, "unknown column `{name}`"),
            Self::UnknownTable(name) => write!(f, "unknown table `{name}`"),
            Self::UnknownFunction(name) => write!(f, "unknown function `{name}`"),
            Self::Arity {
                function,
                expected,
                got,
            } => write!(f, "`{function}` takes {expected} argument(s), got {got}"),
            Self::TypeMismatch {
                operation,
                left,
                right,
            } => write!(f, "cannot apply `{operation}` to {left} and {right}"),
            Self::InvalidPattern { pattern, reason } => {
                write!(f, "invalid pattern `{pattern}`: {reason}")
            }
            Self::InvalidTimestamp(text) => write!(f, "cannot read `{text}` as a timestamp"),
            Self::IntervalUnit(unit) => write!(f, "unsupported interval unit `{unit}`"),
            Self::Overflow(operation) => write!(f, "arithmetic overflow in `{operation}`"),
            Self::DivideByZero => write!(f, "division by zero"),
            Self::AggregateOutsidePlan(name) => {
                write!(f, "aggregate `{name}` is not allowed here")
            }
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::Plan(reason) => write!(f, "{reason}"),
            Self::Stale(path) => write!(
                f,
                "{} changed between resolve and apply, nothing was touched",
                path.display()
            ),
            Self::Walk { root, reason } => {
                write!(f, "walk of {} stopped: {reason}", root.display())
            }
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<ParserError> for Error {
    fn from(e: ParserError) -> Self {
        Self::Parse(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
