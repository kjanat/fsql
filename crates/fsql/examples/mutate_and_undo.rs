//! Preview, apply, and undo a deletion in an automatically removed fixture.
use std::fs;

use fsql::Engine;
use fsql::mutate::{self, Outcome};
use fsql::walk::WalkOptions;

fn assert_success(outcome: &Outcome) {
    assert_eq!(outcome.applied, 1, "{outcome:?}");
    assert!(outcome.failures.is_empty(), "{outcome:?}");
    assert!(outcome.partial.is_empty(), "{outcome:?}");
    assert!(outcome.recovery_required.is_empty(), "{outcome:?}");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = fixture.path().join("files");
    let journals = fixture.path().join("journals");
    fs::create_dir(&root)?;
    fs::write(root.join("scratch.tmp"), "temporary contents")?;
    fs::write(root.join("keep.txt"), "keep me")?;

    let mut engine = Engine::new(&root, WalkOptions::default());
    engine.mutation_cap = 1;
    let mutation = engine.resolve_mutation(include_str!("queries/delete-temporary.fsql"))?;
    println!("Preview: {:?}", mutation.paths());
    assert_eq!(mutation.len(), 1);

    let (outcome, id) = mutation.apply(&journals)?;
    assert_success(&outcome);
    assert!(!root.join("scratch.tmp").exists());
    println!("Deleted one file; journal {id}");

    assert_success(&mutate::undo(&journals, &id)?);
    assert_eq!(
        fs::read_to_string(root.join("scratch.tmp"))?,
        "temporary contents"
    );
    assert_eq!(fs::read_to_string(root.join("keep.txt"))?, "keep me");
    println!("Undo restored its contents; keep.txt is unchanged.");
    fixture.close()?;
    Ok(())
}
