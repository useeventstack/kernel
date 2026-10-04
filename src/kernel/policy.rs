//! Execution policy: the complete set of knobs, and nothing else.
//!
//! # Why this is small
//!
//! The temptation with a runtime that supports several execution semantics is to
//! expose them as a matrix — `mode = X`, `durability = Y`, `speculation = Z`,
//! `effects = W` — and let the caller pick a corner. That matrix does not stay
//! coherent: the number of combinations grows faster than anyone can reason about,
//! most of them are nonsense, and the interesting property ("is this effect safe
//! while speculative?") gets buried in a dimension the user can set wrongly.
//!
//! So the policy has exactly four fields, and two of the things that *look* like
//! configuration have been removed for good:
//!
//! 1. **Effect safety is not a knob.** It is a total function of the effect's
//!    class and of whether its future is authoritative
//!    ([`crate::domain::effect::EffectRequest::may_perform_now`]). There is no
//!    setting that lets a speculative future perform an irreversible effect,
//!    because there is no setting to get wrong.
//! 2. **Speculation width is not a mode.** It is a bound inside
//!    [`Durability`], alongside the other two durabilities, so there is one
//!    place to look for "how far ahead does this run".
//!
//! What is left is the actual policy surface, and all four fields have defaults
//! that a developer who knows nothing about speculation should use.

use std::fmt;

use crate::domain::state::Conflict;
use crate::kernel::evaluate::Evaluator;

/// When the executor may proceed without waiting for durability.
///
/// The three variants are *the same runtime with different timing*, not different
/// runtimes. They share the ledger, the state store, the effect journal, the
/// commit protocol and the recovery path; only this field differs. That is what
/// makes them a fair benchmark comparison — a benchmark of four separate engines
/// would be a benchmark of four different implementations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Durability {
    /// Every record is durable before the next step runs.
    ///
    /// The control condition. Its cost is `steps × flush`, and it is the only
    /// variant with no speculative state at all.
    #[default]
    Synchronous,
    /// The executor may run up to `batch` records ahead, then must wait for them
    /// to become durable.
    ///
    /// This is the fair baseline for a futures benchmark. It is *not* the same as
    /// [`Durability::Synchronous`] — it gets most of the group-commit win — and
    /// comparing a futures runtime against only the synchronous baseline would
    /// credit the futures runtime with a batching trick.
    GroupCommit { batch: usize },
    /// The executor runs ahead freely, bounded by `window` records in flight; a
    /// background writer makes records durable in groups *while the executor
    /// works*.
    ///
    /// The executor is never blocked by durability except at a window bound, at a
    /// decision point, or once at the end to make the result recoverable. This is
    /// the original durable-speculative-execution thesis expressed as a policy.
    Async { window: usize },
}

impl Durability {
    /// Whether this variant keeps speculative, not-yet-durable state.
    #[must_use]
    pub fn is_speculative(self) -> bool {
        !matches!(self, Durability::Synchronous)
    }

    /// How many records the executor may hold that are not yet durable.
    #[must_use]
    pub fn window(self) -> usize {
        match self {
            Durability::Synchronous => 0,
            Durability::GroupCommit { batch } => batch.max(1),
            Durability::Async { window } => window.max(1),
        }
    }

    /// Whether a record that has not yet been made durable may count as
    /// recoverable. Only the synchronous variant says yes.
    #[must_use]
    pub fn requires_durability_before_proceed(self) -> bool {
        matches!(self, Durability::Synchronous)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Durability::Synchronous => "synchronous",
            Durability::GroupCommit { .. } => "group_commit",
            Durability::Async { .. } => "async",
        }
    }
}

impl fmt::Display for Durability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Durability::GroupCommit { batch } => write!(f, "group_commit({batch})"),
            Durability::Async { window } => write!(f, "async(window={window})"),
            other => f.write_str(other.as_str()),
        }
    }
}

/// What to do when the trunk moved under a future and both changed the same key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Refuse the commit and report the conflicting keys. The default, and the
    /// only honest option: the runtime does not know which value is correct.
    #[default]
    Abort,
    /// Overwrite the trunk's value with the future's, discarding the trunk's
    /// concurrent change.
    ///
    /// Available because sometimes it is the right call, and named precisely
    /// because it is the weaker one. It does not remove the conflict, it chooses
    /// a winner without being asked.
    TakeFuture,
}

/// The complete execution policy.
pub struct ExecutionPolicy {
    /// When the executor may proceed without waiting for durability.
    pub durability: Durability,
    /// How alternative futures are scored and one is chosen.
    pub evaluator: Box<dyn Evaluator>,
    /// What to do about a commit conflict.
    pub conflict: ConflictPolicy,
    /// How many times a failed attempt may be retried before giving up.
    pub max_attempts: u32,
    /// How many futures may execute at once.
    ///
    /// One is the honest default for a single-threaded runtime. More than one
    /// models a runtime with more than one executor slot; the wall clock then
    /// reflects overlap rather than the sum of all futures' work, which is the
    /// entire economic argument for running alternatives at all.
    pub workers: usize,
}

impl Default for ExecutionPolicy {
    /// What a developer who has heard of none of this should get: fully
    /// synchronous, deterministic, and correct.
    fn default() -> Self {
        Self {
            durability: Durability::Synchronous,
            evaluator: Box::new(crate::kernel::evaluate::HighestScore::new("score")),
            conflict: ConflictPolicy::Abort,
            max_attempts: 4,
            workers: 1,
        }
    }
}

impl fmt::Debug for ExecutionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionPolicy")
            .field("durability", &FriendlyDurability(self.durability))
            .field("evaluator", &self.evaluator.name())
            .field("conflict", &self.conflict)
            .field("max_attempts", &self.max_attempts)
            .field("workers", &self.workers)
            .finish()
    }
}

impl ExecutionPolicy {
    /// The plain durable-workflow default.
    #[must_use]
    pub fn durable() -> Self {
        Self::default()
    }

    /// A run-ahead policy: the original durable-speculative-execution thesis.
    #[must_use]
    pub fn speculative(window: usize) -> Self {
        Self {
            durability: Durability::Async { window },
            ..Self::default()
        }
    }

    /// A policy that evaluates alternatives and commits the best.
    #[must_use]
    pub fn exploring(evaluator: Box<dyn Evaluator>, window: usize) -> Self {
        Self {
            durability: Durability::Async { window },
            evaluator,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    #[must_use]
    pub fn with_evaluator(mut self, evaluator: Box<dyn Evaluator>) -> Self {
        self.evaluator = evaluator;
        self
    }

    #[must_use]
    pub fn with_conflict(mut self, conflict: ConflictPolicy) -> Self {
        self.conflict = conflict;
        self
    }

    #[must_use]
    pub fn with_max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    #[must_use]
    pub fn with_workers(mut self, n: usize) -> Self {
        self.workers = n.max(1);
        self
    }
}

/// Renders a [`Durability`] the way the CLI prints it, so a debug dump and the
/// human report agree instead of showing two spellings of the same policy.
struct FriendlyDurability(Durability);

impl fmt::Debug for FriendlyDurability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A commit that cannot proceed because the world moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitConflict {
    pub future: crate::domain::ids::FutureId,
    pub conflicts: Vec<Conflict>,
}

impl fmt::Display for CommitConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "future {} conflicts with the trunk on {} key(s): {}",
            self.future,
            self.conflicts.len(),
            self.conflicts
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )
    }
}

impl std::error::Error for CommitConflict {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_fully_synchronous_and_speculation_free() {
        let p = ExecutionPolicy::default();
        assert_eq!(p.durability, Durability::Synchronous);
        assert!(!p.durability.is_speculative());
        assert_eq!(p.durability.window(), 0);
        assert_eq!(p.conflict, ConflictPolicy::Abort);
        assert_eq!(p.workers, 1);
    }

    #[test]
    fn the_window_is_never_zero_for_a_speculative_policy() {
        assert_eq!(Durability::Async { window: 0 }.window(), 1);
        assert_eq!(Durability::GroupCommit { batch: 0 }.window(), 1);
    }

    #[test]
    fn only_the_synchronous_variant_blocks_on_durability() {
        assert!(Durability::Synchronous.requires_durability_before_proceed());
        assert!(!Durability::GroupCommit { batch: 4 }.requires_durability_before_proceed());
        assert!(!Durability::Async { window: 4 }.requires_durability_before_proceed());
    }

    #[test]
    fn policies_render_readably() {
        assert_eq!(
            Durability::GroupCommit { batch: 8 }.to_string(),
            "group_commit(8)"
        );
        assert_eq!(
            Durability::Async { window: 16 }.to_string(),
            "async(window=16)"
        );
        let debug = format!("{:?}", ExecutionPolicy::speculative(4));
        assert!(debug.contains("async(window=4)"), "{debug}");
        assert!(debug.contains("evaluator"), "{debug}");
        assert!(debug.contains("workers: 1"), "{debug}");
    }

    #[test]
    fn the_workers_field_can_never_be_zero() {
        assert_eq!(ExecutionPolicy::default().with_workers(0).workers, 1);
    }
}
