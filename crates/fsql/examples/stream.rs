//! Stream at most five file paths without collecting a result set.
use std::ops::ControlFlow;
use std::path::PathBuf;

use fsql::walk::WalkOptions;
use fsql::{Engine, Value};

fn main() -> fsql::Result<()> {
    let root = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let mut engine = Engine::new(root, WalkOptions::default());
    engine.execution.max_work = 10_000;
    let query = engine.prepare_query(include_str!("queries/file-paths.fsql"))?;
    let mut seen = 0;
    let completion = query.stream(&mut |_, row| {
        if let Value::Text(path) = &row[0] {
            println!("{path}");
        }
        seen += 1;
        Ok(if seen == 5 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })?;
    assert!(completion.is_complete(), "{:?}", completion.diagnostics);
    eprintln!("Consumed {seen} rows; traversal order is unspecified.");
    Ok(())
}
