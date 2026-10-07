//! Collect and print the largest files below an optional directory argument.
use std::io;
use std::path::PathBuf;

use fsql::Engine;
use fsql::output::{self, Format};
use fsql::walk::WalkOptions;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let engine = Engine::new(root, WalkOptions::default());
    let query = engine.prepare_query(include_str!("queries/largest-files.fsql"))?;
    let (results, completion) = query.collect()?;
    assert!(completion.is_complete(), "{:?}", completion.diagnostics);
    output::write(&mut io::stdout().lock(), &results, Format::Table)?;
    Ok(())
}
