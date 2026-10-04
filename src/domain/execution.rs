//! Logical execution state: where an execution currently is.
//!
//! This is *not* the persistence format. It is rebuilt from the durable log
//! during recovery, and it always describes the logical progress of the
//! workflow (which step comes next, which logical state has been reached).

use std::fmt;

use crate::domain::ids::{AttemptId, ExecutionId, StepId, WorkflowId};
use crate::domain::state::State;
use crate::domain::version::{Sequence, Version};
use crate::runtime::mode::ExecutionMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExecutionStatus {
    Running,
    Completed,
    Failed,
    Recovering,
    RolledBack,
}

impl ExecutionStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionStatus::Running => "Running",
            ExecutionStatus::Completed => "Completed",
            ExecutionStatus::Failed => "Failed",
            ExecutionStatus::Recovering => "Recovering",
            ExecutionStatus::RolledBack => "RolledBack",
        }
    }

    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            ExecutionStatus::Running => 0,
            ExecutionStatus::Completed => 1,
            ExecutionStatus::Failed => 2,
            ExecutionStatus::Recovering => 3,
            ExecutionStatus::RolledBack => 4,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => ExecutionStatus::Running,
            1 => ExecutionStatus::Completed,
            2 => ExecutionStatus::Failed,
            3 => ExecutionStatus::Recovering,
            4 => ExecutionStatus::RolledBack,
            _ => return None,
        })
    }
}

impl fmt::Display for ExecutionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Progress of one workflow execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionState {
    pub execution_id: ExecutionId,
    pub workflow_id: WorkflowId,
    pub status: ExecutionStatus,
    /// The step that will be executed next.
    pub current_step: StepId,
    /// Logical state after every step executed so far.
    pub logical_state: State,
    /// Version produced by the last executed step.
    pub version: Version,
    /// Number of durable records written for this execution.
    pub sequence: Sequence,
    /// Increments once per recovery.
    pub attempt: AttemptId,
    pub mode: ExecutionMode,
    pub recoveries: u32,
}

impl ExecutionState {
    #[must_use]
    pub fn new(execution_id: ExecutionId, workflow_id: WorkflowId, mode: ExecutionMode) -> Self {
        Self {
            execution_id,
            workflow_id,
            status: ExecutionStatus::Running,
            current_step: StepId::new(0),
            logical_state: State::new(),
            version: Version::ZERO,
            sequence: Sequence::ZERO,
            attempt: AttemptId::new(0),
            mode,
            recoveries: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_round_trip() {
        for s in [
            ExecutionStatus::Running,
            ExecutionStatus::Completed,
            ExecutionStatus::Failed,
            ExecutionStatus::Recovering,
            ExecutionStatus::RolledBack,
        ] {
            assert_eq!(ExecutionStatus::from_code(s.code()), Some(s));
        }
        assert_eq!(ExecutionStatus::from_code(99), None);
    }

    #[test]
    fn new_execution_starts_at_first_step() {
        let s = ExecutionState::new(
            ExecutionId::new(1),
            WorkflowId::new(1),
            ExecutionMode::DseExperimental,
        );
        assert_eq!(s.current_step, StepId::new(0));
        assert_eq!(s.version, Version::ZERO);
        assert_eq!(s.status, ExecutionStatus::Running);
    }
}
