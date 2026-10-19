//! Shared glob semantics for row evaluation and traversal proofs.
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::{Error, Result};

pub(crate) fn compile_glob(pattern: &str) -> Result<Glob> {
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|error| Error::InvalidPattern {
            pattern: pattern.to_owned(),
            reason: error.to_string(),
        })
}

pub(crate) fn compile_globs<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(compile_glob(pattern)?);
    }
    builder
        .build()
        .map_err(|error| Error::Plan(format!("glob set: {error}")))
}
