//! Commit: making one future authoritative.
//!
//! # What commit means, precisely
//!
//! Commit has two halves and they are deliberately separated, because conflating
//! them is how "exactly-once" gets claimed where only at-least-once exists.
//!
//! **The pointer.** A single `Committed` record is appended to the ledger. That
//! record names the future, the merged trunk state and the new trunk version. It
//! is the atomic commit point:
//!
//! * before it is durable, **nothing has happened** — the trunk still has the old
//!   head, and a retry is safe;
//! * once it is durable, **the commit is a fact** — the trunk has the new head
//!   and recovery will rebuild it that way.
//!
//! Because the ledger is append-only and the record is a single frame, the
//! transition is a prefix property: a reader sees the old trunk or the new one,
//! never a mixture.
//!
//! **The effects.** The deferred effects the winning future recorded are released
//! *after* the pointer is durable, each with its effect key as the idempotency
//! token, so a crash between the pointer and the last effect is repaired by
//! re-releasing with the same keys.
//!
//! # What is promised, and what is not
//!
//! | question | answer |
//! |---|---|
//! | Is the state transition atomic? | **Yes**, with respect to a crash: the ledger append is the commit point. |
//! | Is it retryable? | **Yes.** Before the record is durable, retry; after, recovery finishes the release. |
//! | Is it idempotent? | **Yes** for the pointer: a second commit of the same future finds the trunk already derived from it and reports `AlreadyCommitted`. |
//! | Can two futures commit at once? | **No.** The trunk is a single value; the second commit is validated against a trunk head that has moved, and either merges cleanly or reports a conflict. |
//! | Can it fail? | **Yes**, on a conflict, with the conflicting keys and all three values. |
//! | Are released effects exactly-once? | **Only if the target honours the effect key.** The runtime delivers at-least-once and deduplicates by key; a target that ignores keys gets duplicates, and the runtime does not pretend otherwise. |
//!
//! The last row is the honest ceiling. Nothing in the runtime can make a
//! third-party payment processor exactly-once, and a design that implied it
//! would be lying at exactly the point where it matters most.

use std::fmt;

use crate::domain::effect::{EffectKey, EffectValue};
use crate::domain::ids::EffectId;
use crate::domain::ids::FutureId;
use crate::domain::node::StateNode;
use crate::domain::state::{Conflict, State};
use crate::domain::version::Version;
use crate::kernel::policy::{CommitConflict, ConflictPolicy};

/// What a validated commit will do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitPlan {
    /// The future that becomes authoritative.
    pub future: FutureId,
    /// The trunk state after the merge.
    pub merged: State,
    /// Content address of `merged`.
    pub merged_node: StateNode,
    /// The trunk version after the merge.
    pub trunk_version: Version,
    /// Keys the merge changed, as `(key, from, to)`.
    pub changed: Vec<(String, i64, i64)>,
    /// Effects this commit releases, in issue order.
    pub release: Vec<EffectId>,
    /// Keys where both the future and the trunk moved, incompatibly.
    pub conflicts: Vec<Conflict>,
    /// Whether the trunk had moved at all since the future forked.
    pub trunk_moved: bool,
}

impl CommitPlan {
    /// Whether the merge was clean.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Why a commit could not proceed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitError {
    /// The future is not a legal candidate.
    NotEligible {
        future: FutureId,
        reason: &'static str,
    },
    /// The future has already been committed, so committing it again would make
    /// it authoritative twice.
    AlreadyCommitted { future: FutureId },
    /// The trunk moved under the future and the two cannot be merged.
    Conflict(CommitConflict),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommitError::NotEligible { future, reason } => {
                write!(f, "future {future} cannot be committed: {reason}")
            }
            CommitError::AlreadyCommitted { future } => {
                write!(f, "future {future} is already authoritative")
            }
            CommitError::Conflict(c) => write!(f, "{c}"),
        }
    }
}

impl std::error::Error for CommitError {}

/// The inputs a commit decision needs, gathered by the kernel.
pub struct MergeInput<'a> {
    /// The future being committed.
    pub future: FutureId,
    /// The state the future forked from — the merge base.
    pub base: &'a State,
    /// The future's head.
    pub ours: &'a State,
    /// The trunk's current head.
    pub theirs: &'a State,
    /// The version the future forked at.
    pub base_version: Version,
    /// The trunk's current version.
    pub trunk_version: Version,
    /// Effects the future recorded as unissued intents, in ordinal order.
    pub pending: Vec<EffectId>,
    /// The future that is already authoritative, if any.
    pub already_authoritative: Option<FutureId>,
}

/// Validates a commit and produces the plan, or explains why not.
///
/// The three-way merge uses `base` as the merge base — the state the future
/// forked from — which is the same primitive as `git merge-base`, and the reason
/// for three-way rather than two-way: with only two inputs, a change the trunk
/// made that the future did not touch is indistinguishable from a conflict, and
/// overwriting it would silently lose work.
pub fn plan_merge(
    input: &MergeInput<'_>,
    policy: ConflictPolicy,
) -> Result<CommitPlan, CommitError> {
    if input.already_authoritative == Some(input.future) {
        return Err(CommitError::AlreadyCommitted {
            future: input.future,
        });
    }
    let trunk_moved = input.trunk_version != input.base_version;
    let (merged, conflicts) = match State::three_way_merge(input.base, input.ours, input.theirs) {
        Ok(merged) => (merged, Vec::new()),
        Err(conflicts) => match policy {
            ConflictPolicy::Abort => {
                return Err(CommitError::Conflict(CommitConflict {
                    future: input.future,
                    conflicts,
                }))
            }
            ConflictPolicy::TakeFuture => (input.ours.clone(), conflicts),
        },
    };
    let changed = input.theirs.diff(&merged);
    Ok(CommitPlan {
        future: input.future,
        merged,
        merged_node: StateNode::EMPTY, // filled in by the kernel, which owns the store
        trunk_version: input.trunk_version.next(),
        changed,
        release: input.pending.clone(),
        conflicts,
        trunk_moved,
    })
}

/// The release step: issues one deferred effect, idempotently.
///
/// The key is the idempotency token. Issuing the same effect twice with the same
/// key is indistinguishable from issuing it once *if and only if* the target
/// honours keys, and that condition is the whole of the effect guarantee — so it
/// is stated at the call site rather than buried in a doc comment elsewhere.
pub fn release_effect<S: crate::domain::effect::EffectSink>(
    sink: &mut S,
    key: &EffectKey,
    op: &crate::domain::effect::EffectOp,
    class: crate::domain::effect::EffectClass,
) -> Result<EffectValue, crate::domain::effect::EffectError> {
    sink.perform(key, op, class)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(pairs: &[(&str, i64)]) -> State {
        let mut s = State::new();
        for (k, v) in pairs {
            s.set(k, *v);
        }
        s
    }

    fn input<'a>(
        base: &'a State,
        ours: &'a State,
        theirs: &'a State,
        already: Option<FutureId>,
    ) -> MergeInput<'a> {
        MergeInput {
            future: FutureId::new(2),
            base,
            ours,
            theirs,
            base_version: Version::new(5),
            trunk_version: Version::new(7),
            pending: vec![EffectId::new(0), EffectId::new(1)],
            already_authoritative: already,
        }
    }

    #[test]
    fn a_clean_merge_takes_the_future_and_releases_its_effects() {
        let base = state(&[("shared", 1)]);
        let mut ours = base.clone();
        ours.set("shared", 2);
        let plan = plan_merge(&input(&base, &ours, &base, None), ConflictPolicy::Abort).unwrap();
        assert!(plan.is_clean());
        assert_eq!(plan.merged.get("shared"), 2);
        assert_eq!(plan.changed, vec![("shared".to_owned(), 1, 2)]);
        assert_eq!(plan.release, vec![EffectId::new(0), EffectId::new(1)]);
        assert_eq!(plan.trunk_version, Version::new(8));
        assert!(
            plan.trunk_moved,
            "the trunk version differs from the fork point"
        );
    }

    #[test]
    fn a_conflict_aborts_by_default_and_names_every_key() {
        let base = state(&[("a", 1), ("b", 1)]);
        let mut ours = base.clone();
        ours.set("a", 2);
        ours.set("b", 2);
        let mut theirs = base.clone();
        theirs.set("a", 9);
        theirs.set("b", 9);
        let err =
            plan_merge(&input(&base, &ours, &theirs, None), ConflictPolicy::Abort).unwrap_err();
        let CommitError::Conflict(c) = err else {
            panic!("expected a conflict, got {err:?}")
        };
        assert_eq!(c.conflicts.len(), 2);
        assert!(c.to_string().contains("key `a`"));
    }

    #[test]
    fn the_weaker_policy_commits_and_reports_what_it_overwrote() {
        let base = state(&[("a", 1)]);
        let mut ours = base.clone();
        ours.set("a", 2);
        let mut theirs = base.clone();
        theirs.set("a", 9);
        let plan = plan_merge(
            &input(&base, &ours, &theirs, None),
            ConflictPolicy::TakeFuture,
        )
        .unwrap();
        assert!(!plan.is_clean());
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.merged.get("a"), 2);
    }

    #[test]
    fn committing_twice_is_refused() {
        let base = state(&[]);
        let err = plan_merge(
            &input(&base, &base, &base, Some(FutureId::new(2))),
            ConflictPolicy::Abort,
        )
        .unwrap_err();
        assert_eq!(
            err,
            CommitError::AlreadyCommitted {
                future: FutureId::new(2)
            }
        );
        assert!(err.to_string().contains("already authoritative"));
    }

    #[test]
    fn a_second_commit_over_the_same_state_cannot_silently_replace_the_first() {
        // Two futures both commit from the same base. The second is validated
        // against a trunk that has moved, so it either merges cleanly or
        // conflicts; it can never replace the winner without one of those.
        let base = state(&[("x", 0)]);
        let mut a = base.clone();
        a.set("x", 1);
        let mut b = base.clone();
        b.set("x", 2);
        let first = plan_merge(&input(&base, &a, &base, None), ConflictPolicy::Abort).unwrap();
        let second = plan_merge(
            &input(&base, &b, &first.merged, None),
            ConflictPolicy::Abort,
        );
        assert!(matches!(second, Err(CommitError::Conflict(_))));
    }

    #[test]
    fn independent_keys_from_two_commits_merge_without_conflict() {
        let base = state(&[("x", 0), ("y", 0)]);
        let mut a = base.clone();
        a.set("x", 1);
        let mut b = base.clone();
        b.set("y", 2);
        let first = plan_merge(&input(&base, &a, &base, None), ConflictPolicy::Abort).unwrap();
        let second = plan_merge(
            &input(&base, &b, &first.merged, None),
            ConflictPolicy::Abort,
        )
        .unwrap();
        assert!(second.is_clean());
        assert_eq!(second.merged.get("x"), 1, "the first commit survived");
        assert_eq!(second.merged.get("y"), 2, "the second change was applied");
    }

    #[test]
    fn releasing_an_effect_twice_issues_it_once() {
        use crate::domain::effect::{EffectClass, EffectKey, EffectOp, RecordingSink};
        use crate::domain::ids::ExecutionId;
        let mut sink = RecordingSink::new();
        let key = EffectKey::new(ExecutionId::new(1), vec![0], EffectId::new(0));
        let op = EffectOp::new("charge", 100);
        release_effect(&mut sink, &key, &op, EffectClass::Irreversible).unwrap();
        release_effect(&mut sink, &key, &op, EffectClass::Irreversible).unwrap();
        assert_eq!(sink.observed_len(), 1, "the world was touched once");
    }
}
