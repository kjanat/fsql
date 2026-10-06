//! Stable entry points with explicit execution and mutation policies.
use std::path::{Path, PathBuf};

use crate::execution::{Completion, ExecutionOptions};
use crate::journal::Journal;
use crate::mutate::{self, Outcome, Resolved};
use crate::output::ResultSet;
use crate::plan::{Plan, Planner, QueryPlan};
use crate::walk::WalkOptions;
use crate::{Error, Result};

#[derive(Clone)]
pub struct Engine {
    planner: Planner,
    pub execution: ExecutionOptions,
    /// Zero explicitly disables the mutation entry cap.
    pub mutation_cap: usize,
}
impl Engine {
    pub fn new(root: impl Into<PathBuf>, walk: WalkOptions) -> Self {
        Self {
            planner: Planner::new(root, walk),
            execution: ExecutionOptions::default(),
            mutation_cap: 10_000,
        }
    }
    pub fn prepare_query(&self, sql: &str) -> Result<PreparedQuery> {
        let mut plans = self.planner.plan(sql)?;
        if plans.len() != 1 {
            return Err(Error::Plan("expected one query".into()));
        }
        let Plan::Select(plan) = plans.remove(0) else {
            return Err(Error::Plan("expected SELECT".into()));
        };
        Ok(PreparedQuery {
            plan: *plan,
            planner: self.planner.clone(),
            options: self.execution.clone(),
        })
    }
    pub fn resolve_mutation(&self, sql: &str) -> Result<ResolvedMutation> {
        let mut plans = self.planner.plan(sql)?;
        if plans.len() != 1 || !plans[0].is_mutation() {
            return Err(Error::Plan("expected one mutation".into()));
        }
        let resolved = mutate::resolve_with_options(
            &plans.remove(0),
            &self.planner,
            self.execution.clone(),
            &mut |_| {},
        )?;
        if self.mutation_cap != 0 && resolved.len() > self.mutation_cap {
            return Err(Error::Plan(format!(
                "mutation exceeds cap of {} entries",
                self.mutation_cap
            )));
        }
        Ok(ResolvedMutation {
            resolved,
            sql: sql.to_owned(),
        })
    }
}

pub struct PreparedQuery {
    plan: QueryPlan,
    planner: Planner,
    options: ExecutionOptions,
}
impl PreparedQuery {
    pub fn collect(&self) -> Result<(ResultSet, Completion)> {
        crate::exec::run_with_options(&self.plan, &self.planner, self.options.clone())
    }
    /// Plain scans emit rows immediately. Blocking operators materialize within
    /// the configured budgets. A break from the callback stops consumption.
    pub fn stream(&self, emit: &mut crate::exec::RowSink<'_>) -> Result<Completion> {
        crate::exec::stream(&self.plan, &self.planner, self.options.clone(), emit)
    }
}

pub struct ResolvedMutation {
    resolved: Resolved,
    sql: String,
}
impl ResolvedMutation {
    pub fn len(&self) -> usize {
        self.resolved.len()
    }
    pub fn is_empty(&self) -> bool {
        self.resolved.is_empty()
    }
    pub fn paths(&self) -> Vec<(&Path, u64)> {
        self.resolved.paths()
    }
    pub fn apply(self, journal_base: &Path) -> Result<(Outcome, String)> {
        let mut journal = Journal::open(journal_base, &self.sql)?;
        let outcome = mutate::apply(&self.resolved, Some(&mut journal))?;
        Ok((outcome, journal.id().to_owned()))
    }
    /// Explicitly opt out of recovery records.
    pub fn apply_without_journal(self) -> Result<Outcome> {
        mutate::apply(&self.resolved, None)
    }
}
