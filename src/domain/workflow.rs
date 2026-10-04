//! The workflow model.
//!
//! Deliberately tiny: an ordered list of steps, each of which runs exactly one
//! [`Operation`]. There is no branching, no parallelism and no DSL. The point
//! is to have one *identical* logical workflow that can be executed by either
//! persistence strategy.

use std::fmt;
use std::time::Duration;

use crate::domain::delta::Operation;
use crate::domain::ids::{StepId, WorkflowId};

/// One step of a workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub id: StepId,
    pub name: String,
    pub operation: Operation,
    /// Simulated cost of *executing* the step. This is the deterministic
    /// "useful work" time that persistence latency is supposed to hide behind.
    pub cost: Duration,
}

impl Step {
    #[must_use]
    pub fn new(id: StepId, name: &str, operation: Operation) -> Self {
        Self {
            id,
            name: name.to_owned(),
            operation,
            cost: Duration::from_millis(1),
        }
    }
}

/// A linear workflow definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workflow {
    pub id: WorkflowId,
    pub name: String,
    pub steps: Vec<Step>,
}

impl Workflow {
    /// Default per-step execution cost.
    pub const DEFAULT_STEP_COST: Duration = Duration::from_millis(1);

    #[must_use]
    pub fn new(id: WorkflowId, name: &str, operations: Vec<Operation>) -> Self {
        let steps = operations
            .into_iter()
            .enumerate()
            .map(|(i, op)| {
                let step_id = StepId::new(i as u64);
                Step::new(step_id, &format!("step-{}", i + 1), op)
            })
            .collect();
        Self {
            id,
            name: name.to_owned(),
            steps,
        }
    }

    /// Replaces the simulated execution cost of every step.
    #[must_use]
    pub fn with_step_cost(mut self, cost: Duration) -> Self {
        for step in &mut self.steps {
            step.cost = cost;
        }
        self
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    #[must_use]
    pub fn step(&self, id: StepId) -> Option<&Step> {
        self.steps.get(id.value() as usize)
    }

    #[must_use]
    pub fn step_at(&self, index: usize) -> Option<&Step> {
        self.steps.get(index)
    }

    /// Index of `id`, or `None` if the step is not part of this workflow.
    #[must_use]
    pub fn index_of(&self, id: StepId) -> Option<usize> {
        self.steps.iter().position(|s| s.id == id)
    }
}

impl fmt::Display for Workflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({} steps)", self.name, self.steps.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_are_sequentially_identified() {
        let wf = Workflow::new(
            WorkflowId::new(1),
            "t",
            vec![
                Operation::Add {
                    key: "a".into(),
                    by: 1,
                },
                Operation::Add {
                    key: "a".into(),
                    by: 1,
                },
            ],
        );
        assert_eq!(wf.len(), 2);
        assert_eq!(wf.step_at(0).unwrap().id, StepId::new(0));
        assert_eq!(wf.index_of(StepId::new(1)), Some(1));
        assert_eq!(wf.index_of(StepId::new(9)), None);
    }

    #[test]
    fn step_cost_is_configurable() {
        let wf = Workflow::new(WorkflowId::new(1), "t", vec![Operation::Sleep { ms: 1 }])
            .with_step_cost(Duration::from_millis(7));
        assert_eq!(wf.steps[0].cost, Duration::from_millis(7));
    }
}
