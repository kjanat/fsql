//! SQL query and mutation engine over the filesystem.

mod bind;
pub mod column;
pub mod dialect;
pub mod engine;
pub mod error;
pub mod eval;
pub mod exec;
pub mod execution;
pub mod journal;
pub mod mounts;
pub mod mutate;
pub mod output;
pub mod plan;
pub mod row;
pub mod time;
pub mod value;
pub mod walk;
pub mod xattr;

pub use column::{Column, Cost, Table};
pub use dialect::FsqlDialect;
pub use engine::{Engine, PreparedQuery, ResolvedMutation};
pub use error::{Error, Result};
pub use execution::{CancellationToken, Completion, ErrorPolicy, ExecutionOptions};
pub use value::{Nanos, Type, Value};
