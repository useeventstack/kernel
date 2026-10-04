//! Deterministic failure injection at the execution-graph lifecycle points.
//!
//! Failure is never random. The injector is a set of named checkpoints that the
//! kernel consults at well defined points, and it fires at most once per attempt,
//! so one experiment has one reproducible failure.
//!
//! The points below are the *whole lifecycle of a durable future*, and that is
//! the point of the list: it is the attack surface a crash can land on. A
//! durable execution system is only as good as its behaviour when it is killed in
//! the least convenient place, so the inconvenient places get names.
//!
//! ```text
//!   before_fork ─ fork ─ after_fork
//!   before_step ─ step ─ after_step
//!   before_effect ─ effect ─ after_effect
//!   before_checkpoint ─ checkpoint ─ after_checkpoint
//!   before_select ─ select ─ after_select
//!   before_commit ─ commit(durable) ─ after_commit ─ after_release
//!   during_recovery
//! ```
//!
//! `BeforeCommit` and `AfterCommit` are separated because that is the whole
//! window in which a commit can be interrupted, and the two sides of it have
//! different correct answers: before the record is durable, nothing happened; after
//! it is durable, the commit is a fact and the effects must be released.

use std::fmt;

/// Where a failure is injected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailurePoint {
    /// Just before the given step of the given future executes.
    BeforeStep { future: u64, n: usize },
    /// Just after the given step of the given future has been recorded.
    AfterStep { future: u64, n: usize },
    /// Just before the given effect of the given future is requested.
    BeforeEffect { future: u64, n: usize },
    /// Just after the effect has been journalled.
    AfterEffect { future: u64, n: usize },
    /// Just before a batch is made durable.
    BeforeCheckpoint,
    /// While a batch is in flight.
    DuringCheckpoint,
    /// Just after a batch has been made durable.
    AfterCheckpoint,
    /// Just before the selection record is appended.
    BeforeSelect,
    /// Just after the selection record is appended but before it is durable.
    AfterSelect,
    /// Just before the commit record is appended.
    BeforeCommit,
    /// The commit record is appended but not yet durable: the interruption window.
    DuringCommit,
    /// The commit record is durable but no effect has been released yet.
    AfterCommit,
    /// The commit is durable and every effect has been released.
    AfterRelease,
    /// While recovery is rebuilding the graph.
    DuringRecovery,
}

impl FailurePoint {
    /// Every point that does not name a specific future, for the failure matrix.
    pub const GLOBAL: [FailurePoint; 10] = [
        FailurePoint::BeforeCheckpoint,
        FailurePoint::DuringCheckpoint,
        FailurePoint::AfterCheckpoint,
        FailurePoint::BeforeSelect,
        FailurePoint::AfterSelect,
        FailurePoint::BeforeCommit,
        FailurePoint::DuringCommit,
        FailurePoint::AfterCommit,
        FailurePoint::AfterRelease,
        FailurePoint::DuringRecovery,
    ];

    /// Every point for a specific future and ordinal.
    pub fn per_record(future: u64, n: usize) -> [FailurePoint; 4] {
        [
            FailurePoint::BeforeStep { future, n },
            FailurePoint::AfterStep { future, n },
            FailurePoint::BeforeEffect { future, n },
            FailurePoint::AfterEffect { future, n },
        ]
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FailurePoint::BeforeStep { .. } => "before_step",
            FailurePoint::AfterStep { .. } => "after_step",
            FailurePoint::BeforeEffect { .. } => "before_effect",
            FailurePoint::AfterEffect { .. } => "after_effect",
            FailurePoint::BeforeCheckpoint => "before_checkpoint",
            FailurePoint::DuringCheckpoint => "during_checkpoint",
            FailurePoint::AfterCheckpoint => "after_checkpoint",
            FailurePoint::BeforeSelect => "before_select",
            FailurePoint::AfterSelect => "after_select",
            FailurePoint::BeforeCommit => "before_commit",
            FailurePoint::DuringCommit => "during_commit",
            FailurePoint::AfterCommit => "after_commit",
            FailurePoint::AfterRelease => "after_release",
            FailurePoint::DuringRecovery => "during_recovery",
        }
    }

    /// The exact point, or a human description of the phase.
    pub fn describe(self) -> String {
        match self {
            FailurePoint::BeforeStep { future, n } => format!("future {future} before step {n}"),
            FailurePoint::AfterStep { future, n } => format!("future {future} after step {n}"),
            FailurePoint::BeforeEffect { future, n } => {
                format!("future {future} before effect {n}")
            }
            FailurePoint::AfterEffect { future, n } => format!("future {future} after effect {n}"),
            FailurePoint::DuringRecovery => "during recovery".to_owned(),
            other => other.as_str().replace('_', " "),
        }
    }
}

impl fmt::Display for FailurePoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Runtime locations the injector observes. Carries enough context to be matched
/// precisely, and nothing more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checkpoint {
    BeforeStep {
        future: u64,
        n: usize,
    },
    AfterStep {
        future: u64,
        n: usize,
    },
    BeforeEffect {
        future: u64,
        n: usize,
    },
    AfterEffect {
        future: u64,
        n: usize,
    },
    BeforeCheckpoint {
        batch: u64,
    },
    DuringCheckpoint {
        batch: u64,
    },
    AfterCheckpoint {
        batch: u64,
    },
    BeforeSelect {
        candidates: usize,
    },
    AfterSelect {
        candidates: usize,
    },
    BeforeCommit,
    /// Appended, not yet durable.
    DuringCommit,
    /// Durable, effects not yet released.
    AfterCommit,
    AfterRelease {
        effects: usize,
    },
    DuringRecovery {
        futures: usize,
    },
}

impl Checkpoint {
    fn matches(self, point: FailurePoint) -> bool {
        match (self, point) {
            (
                Checkpoint::BeforeStep { future, n },
                FailurePoint::BeforeStep { future: f, n: m },
            )
            | (Checkpoint::AfterStep { future, n }, FailurePoint::AfterStep { future: f, n: m })
            | (
                Checkpoint::BeforeEffect { future, n },
                FailurePoint::BeforeEffect { future: f, n: m },
            )
            | (
                Checkpoint::AfterEffect { future, n },
                FailurePoint::AfterEffect { future: f, n: m },
            ) => future == f && n == m,
            (Checkpoint::BeforeCheckpoint { .. }, FailurePoint::BeforeCheckpoint) => true,
            (Checkpoint::DuringCheckpoint { .. }, FailurePoint::DuringCheckpoint) => true,
            (Checkpoint::AfterCheckpoint { .. }, FailurePoint::AfterCheckpoint) => true,
            (Checkpoint::BeforeSelect { .. }, FailurePoint::BeforeSelect) => true,
            (Checkpoint::AfterSelect { .. }, FailurePoint::AfterSelect) => true,
            (Checkpoint::BeforeCommit, FailurePoint::BeforeCommit) => true,
            (Checkpoint::DuringCommit, FailurePoint::DuringCommit) => true,
            (Checkpoint::AfterCommit, FailurePoint::AfterCommit) => true,
            (Checkpoint::AfterRelease { .. }, FailurePoint::AfterRelease) => true,
            (Checkpoint::DuringRecovery { .. }, FailurePoint::DuringRecovery) => true,
            _ => false,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Checkpoint::BeforeStep { .. } => "before_step",
            Checkpoint::AfterStep { .. } => "after_step",
            Checkpoint::BeforeEffect { .. } => "before_effect",
            Checkpoint::AfterEffect { .. } => "after_effect",
            Checkpoint::BeforeCheckpoint { .. } => "before_checkpoint",
            Checkpoint::DuringCheckpoint { .. } => "during_checkpoint",
            Checkpoint::AfterCheckpoint { .. } => "after_checkpoint",
            Checkpoint::BeforeSelect { .. } => "before_select",
            Checkpoint::AfterSelect { .. } => "after_select",
            Checkpoint::BeforeCommit => "before_commit",
            Checkpoint::DuringCommit => "during_commit",
            Checkpoint::AfterCommit => "after_commit",
            Checkpoint::AfterRelease { .. } => "after_release",
            Checkpoint::DuringRecovery { .. } => "during_recovery",
        }
    }
}

/// A failure the injector decided to raise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InjectedFailure {
    pub point: FailurePoint,
    pub checkpoint: Checkpoint,
}

impl fmt::Display for InjectedFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "injected failure at {} ({})",
            self.point,
            self.checkpoint.as_str()
        )
    }
}

impl std::error::Error for InjectedFailure {}

/// Fires at most one failure per arming, at the first matching checkpoint.
#[derive(Clone, Copy, Debug, Default)]
pub struct FailureInjector {
    point: Option<FailurePoint>,
    fired: Option<InjectedFailure>,
    /// Whether an automatic re-arm after a rollback should happen.
    ///
    /// This distinction is load bearing. A *one-shot* injection models a single
    /// transient fault, and the runtime must recover from it and finish. If the
    /// injector re-armed on every attempt, a one-shot fault would be a fault at
    /// every attempt, the run would burn its whole attempt budget, and the test
    /// would be asserting the behaviour of a different failure model.
    repeating: bool,
}

impl FailureInjector {
    #[must_use]
    pub fn none() -> Self {
        Self {
            point: None,
            fired: None,
            repeating: false,
        }
    }

    /// A one-shot failure: it fires once, and the run must then recover and finish.
    #[must_use]
    pub fn at(point: FailurePoint) -> Self {
        Self {
            point: Some(point),
            fired: None,
            repeating: false,
        }
    }

    /// A failure that fires on *every* attempt, so a test can drive the runtime
    /// into its attempt budget.
    #[must_use]
    pub fn repeating(point: FailurePoint) -> Self {
        Self {
            point: Some(point),
            fired: None,
            repeating: true,
        }
    }

    #[must_use]
    pub fn point(&self) -> Option<FailurePoint> {
        self.point
    }

    #[must_use]
    pub fn has_fired(&self) -> bool {
        self.fired.is_some()
    }

    #[must_use]
    pub fn failure(&self) -> Option<InjectedFailure> {
        self.fired
    }

    /// Arms the injector for another attempt.
    ///
    /// Only a [`FailureInjector::repeating`] injector re-arms automatically; a
    /// one-shot injector stays fired, so recovery from a single transient fault is
    /// actually exercised rather than being retried until the budget runs out.
    pub fn rearm(&mut self) {
        if self.point.is_some() && self.repeating {
            self.fired = None;
        }
    }

    /// Forces a re-arm regardless of the mode, for a test that wants a second
    /// failure at the same point.
    pub fn force_rearm(&mut self) {
        self.fired = None;
    }

    /// Whether this injector fires on every attempt.
    #[must_use]
    pub fn is_repeating(&self) -> bool {
        self.repeating
    }

    /// The failure that was injected, if one was.
    ///
    /// Lets a harness distinguish "the fault I set happened and the runtime coped"
    /// from "the fault I set was never reachable for this plan", which look
    /// identical from the outside and are not the same result.
    #[must_use]
    pub fn fired(&self) -> Option<InjectedFailure> {
        self.fired
    }

    /// Returns a failure if this checkpoint triggers the injector.
    pub fn observe(&mut self, checkpoint: Checkpoint) -> Option<InjectedFailure> {
        let point = self.point?;
        if self.fired.is_some() {
            return None;
        }
        if !checkpoint.matches(point) {
            return None;
        }
        let failure = InjectedFailure { point, checkpoint };
        self.fired = Some(failure);
        Some(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_point_matches_only_its_own_future_and_ordinal() {
        let mut inj = FailureInjector::at(FailurePoint::AfterStep { future: 2, n: 3 });
        assert!(inj
            .observe(Checkpoint::AfterStep { future: 1, n: 3 })
            .is_none());
        assert!(inj
            .observe(Checkpoint::AfterStep { future: 2, n: 4 })
            .is_none());
        assert!(inj
            .observe(Checkpoint::AfterStep { future: 2, n: 3 })
            .is_some());
        assert!(inj
            .observe(Checkpoint::AfterStep { future: 2, n: 3 })
            .is_none());
    }

    #[test]
    fn an_effect_point_does_not_match_a_step_checkpoint() {
        let mut inj = FailureInjector::at(FailurePoint::BeforeEffect { future: 1, n: 0 });
        assert!(inj
            .observe(Checkpoint::BeforeStep { future: 1, n: 0 })
            .is_none());
        assert!(inj
            .observe(Checkpoint::BeforeEffect { future: 1, n: 0 })
            .is_some());
    }

    #[test]
    fn no_injector_never_fires() {
        let mut inj = FailureInjector::none();
        assert!(inj
            .observe(Checkpoint::AfterStep { future: 1, n: 1 })
            .is_none());
        assert!(!inj.has_fired());
        assert!(inj.point().is_none());
    }

    #[test]
    fn every_global_point_matches_exactly_its_own_checkpoint() {
        for point in FailurePoint::GLOBAL {
            let mut inj = FailureInjector::at(point);
            let mut hits = 0;
            for other in FailurePoint::GLOBAL {
                let cp = checkpoint_for(other);
                if inj.observe(cp).is_some() {
                    hits += 1;
                }
            }
            assert_eq!(hits, 1, "{point} matched {hits} checkpoints");
        }
    }

    fn checkpoint_for(point: FailurePoint) -> Checkpoint {
        match point {
            FailurePoint::BeforeCheckpoint => Checkpoint::BeforeCheckpoint { batch: 1 },
            FailurePoint::DuringCheckpoint => Checkpoint::DuringCheckpoint { batch: 1 },
            FailurePoint::AfterCheckpoint => Checkpoint::AfterCheckpoint { batch: 1 },
            FailurePoint::BeforeSelect => Checkpoint::BeforeSelect { candidates: 2 },
            FailurePoint::AfterSelect => Checkpoint::AfterSelect { candidates: 2 },
            FailurePoint::BeforeCommit => Checkpoint::BeforeCommit,
            FailurePoint::DuringCommit => Checkpoint::DuringCommit,
            FailurePoint::AfterCommit => Checkpoint::AfterCommit,
            FailurePoint::AfterRelease => Checkpoint::AfterRelease { effects: 1 },
            FailurePoint::DuringRecovery => Checkpoint::DuringRecovery { futures: 2 },
            other => panic!("{other} is not a global point"),
        }
    }

    #[test]
    fn a_one_shot_failure_does_not_re_arm_on_its_own() {
        let mut inj = FailureInjector::at(FailurePoint::AfterStep { future: 1, n: 1 });
        assert!(inj
            .observe(Checkpoint::AfterStep { future: 1, n: 1 })
            .is_some());
        assert!(inj
            .observe(Checkpoint::AfterStep { future: 1, n: 1 })
            .is_none());
        inj.rearm();
        assert!(
            inj.observe(Checkpoint::AfterStep { future: 1, n: 1 })
                .is_none(),
            "a one-shot failure stays fired, so recovery is actually exercised"
        );
        inj.force_rearm();
        assert!(inj
            .observe(Checkpoint::AfterStep { future: 1, n: 1 })
            .is_some());
    }

    #[test]
    fn a_repeating_failure_re_arms_on_every_attempt() {
        let mut inj = FailureInjector::repeating(FailurePoint::AfterStep { future: 1, n: 1 });
        assert!(inj.is_repeating());
        for _ in 0..4 {
            assert!(inj
                .observe(Checkpoint::AfterStep { future: 1, n: 1 })
                .is_some());
            inj.rearm();
        }
    }

    #[test]
    fn descriptions_are_human_readable() {
        assert_eq!(
            FailurePoint::AfterStep { future: 3, n: 15 }.to_string(),
            "future 3 after step 15"
        );
        assert_eq!(FailurePoint::DuringCommit.to_string(), "during commit");
    }
}
