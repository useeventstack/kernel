//! Futures: durable alternative continuations, their lifecycle, and lineage.
//!
//! # A future is a pointer plus a chain
//!
//! Structurally, a future is `(base node, cursor, delta chain)`. It does not own
//! a copy of the state it started from — it names it. That is why forking is
//! free, and why "future A cannot corrupt future B" needs no runtime check: the
//! two chains are separate values and the state store is insert-only.
//!
//! # The lifecycle, and what each state *means*
//!
//! ```text
//!                     ┌──────────► Failed        (the plan failed; terminal)
//!                     │
//!   Open ──► Running ─┼──► Blocked ──► Evaluable ──► Selected ──► Committing ──► Committed
//!    │                 │
//!    └─────────────────┴──► Abandoned            (dropped by a decision)
//!                                          └────► Rejected   (lost a selection)
//! ```
//!
//! Every state has an invariant, and the invariant is what makes the transition
//! table total rather than suggestive:
//!
//! | status | invariant |
//! |---|---|
//! | `Open` | head == base, chain empty, no effects |
//! | `Running` | cursor strictly inside the plan, chain non-empty or cursor > 0 |
//! | `Blocked` | sitting on a `Fork` node with at least two live children |
//! | `Evaluable` | cursor == arm end, every delta in the chain is well formed |
//! | `Selected` | was `Evaluable`, and a durable selection names it |
//! | `Rejected` | was `Evaluable`, and a durable selection named a *different* future |
//! | `Committed` | a durable commit record names it; it is the trunk's head |
//! | `Abandoned` | deliberately dropped; its deferred effects were never issued |
//! | `Failed` | the plan reached `Operation::Fail` |
//!
//! `Committed` and `Rejected` are *derived from durable records*, not set by
//! the executor. Recovery therefore reconstructs them by replay, and they can
//! never disagree with the ledger.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::domain::effect::{Disposition, EffectClass};
use crate::domain::ids::{EffectId, FutureId, NodeId};
use crate::domain::node::StateNode;
use crate::domain::state::State;
use crate::domain::version::Version;

/// Lifecycle position of a future.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FutureStatus {
    /// Created, nothing executed.
    Open,
    /// Executing.
    Running,
    /// Waiting on the children of a fork.
    Blocked,
    /// Finished its arm; ready to be evaluated.
    Evaluable,
    /// Chosen by a selection.
    Selected,
    /// Chosen by a selection, and the selection has been made durable.
    SelectionRecorded,
    /// Making the trunk's head this future.
    Committing,
    /// The trunk's head is this future. Terminal.
    Committed,
    /// Lost a selection. Terminal.
    Rejected,
    /// Dropped deliberately. Terminal.
    Abandoned,
    /// The plan failed. Terminal.
    Failed,
}

impl FutureStatus {
    /// Every status, for exhaustive tests.
    pub const ALL: [FutureStatus; 11] = [
        FutureStatus::Open,
        FutureStatus::Running,
        FutureStatus::Blocked,
        FutureStatus::Evaluable,
        FutureStatus::Selected,
        FutureStatus::SelectionRecorded,
        FutureStatus::Committing,
        FutureStatus::Committed,
        FutureStatus::Rejected,
        FutureStatus::Abandoned,
        FutureStatus::Failed,
    ];

    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            FutureStatus::Committed
                | FutureStatus::Rejected
                | FutureStatus::Abandoned
                | FutureStatus::Failed
        )
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FutureStatus::Open => "Open",
            FutureStatus::Running => "Running",
            FutureStatus::Blocked => "Blocked",
            FutureStatus::Evaluable => "Evaluable",
            FutureStatus::Selected => "Selected",
            FutureStatus::SelectionRecorded => "SelectionRecorded",
            FutureStatus::Committing => "Committing",
            FutureStatus::Committed => "Committed",
            FutureStatus::Rejected => "Rejected",
            FutureStatus::Abandoned => "Abandoned",
            FutureStatus::Failed => "Failed",
        }
    }

    /// Wire code, used by the ledger encoding.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            FutureStatus::Open => 0,
            FutureStatus::Running => 1,
            FutureStatus::Blocked => 2,
            FutureStatus::Evaluable => 3,
            FutureStatus::Selected => 4,
            FutureStatus::SelectionRecorded => 5,
            FutureStatus::Committing => 6,
            FutureStatus::Committed => 7,
            FutureStatus::Rejected => 8,
            FutureStatus::Abandoned => 9,
            FutureStatus::Failed => 10,
        }
    }

    /// Inverse of [`FutureStatus::code`].
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => FutureStatus::Open,
            1 => FutureStatus::Running,
            2 => FutureStatus::Blocked,
            3 => FutureStatus::Evaluable,
            4 => FutureStatus::Selected,
            5 => FutureStatus::SelectionRecorded,
            6 => FutureStatus::Committing,
            7 => FutureStatus::Committed,
            8 => FutureStatus::Rejected,
            9 => FutureStatus::Abandoned,
            10 => FutureStatus::Failed,
            _ => return None,
        })
    }
}

impl fmt::Display for FutureStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a future is where it is — the error surface for illegal transitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FutureError {
    /// The future does not exist.
    Unknown(FutureId),
    /// A transition that the lifecycle forbids.
    Illegal {
        future: FutureId,
        from: FutureStatus,
        to: FutureStatus,
    },
    /// The future is already finished in some other way.
    AlreadySettled {
        future: FutureId,
        status: FutureStatus,
    },
    /// The future's cursor is not where the caller believes.
    CursorMismatch {
        future: FutureId,
        expected: NodeId,
        actual: NodeId,
    },
    /// The delta chain is not contiguous from the base.
    BrokenChain { future: FutureId, at: Version },
}

impl fmt::Display for FutureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FutureError::Unknown(id) => write!(f, "no such future: {id}"),
            FutureError::Illegal { future, from, to } => {
                write!(f, "{future}: cannot move from {from} to {to}")
            }
            FutureError::AlreadySettled { future, status } => {
                write!(f, "{future} is already {status}")
            }
            FutureError::CursorMismatch {
                future,
                expected,
                actual,
            } => write!(
                f,
                "{future}: expected the cursor to be {expected}, found {actual}"
            ),
            FutureError::BrokenChain { future, at } => {
                write!(f, "{future}: delta chain is broken at {at}")
            }
        }
    }
}

impl std::error::Error for FutureError {}

/// A journalled effect occurrence, kept for observability and for the commit
/// protocol's release list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectRecord {
    pub key: crate::domain::effect::EffectKey,
    pub class: EffectClass,
    pub op: crate::domain::effect::EffectOp,
    pub disposition: Disposition,
    /// The compensation declared by the plan, if any.
    pub compensation: Option<crate::domain::effect::EffectOp>,
    /// State key the effect writes, if the plan declared one.
    pub writes: Option<String>,
}

/// One durable alternative continuation.
#[derive(Clone, Debug)]
pub struct Future {
    pub id: FutureId,
    pub parent: Option<FutureId>,
    /// Index of this future's arm, when it came from a fork.
    pub arm: Option<usize>,
    /// Sequence of fork-arm indices from the root; empty for the trunk.
    ///
    /// This is the future's *stable* identity. `id` is assigned by the runtime and
    /// changes whenever a rollback discards a fork and re-executes it, so nothing
    /// that has to survive a retry may be built from it — an effect's idempotency
    /// key most of all. The path is regenerated identically by the re-executed fork,
    /// which is exactly what makes it stable.
    pub path: Vec<u32>,
    /// The cursor at which this future's arm ends. `None` for the trunk, whose
    /// arm is the whole plan.
    ///
    /// Stored rather than re-derived, because "this child has finished" has to be
    /// decidable from the future alone: the parent's cursor has already moved past
    /// the fork, so the extent cannot be found by looking at the parent.
    pub arm_end: Option<NodeId>,
    /// The state this future started from. Never mutated.
    pub base: StateNode,
    /// The state version this future started from.
    pub base_version: Version,
    /// Highest version this future has produced.
    pub head: StateNode,
    /// Highest version this future has produced that is *known durable*.
    pub durable_head: StateNode,
    /// Version `head` corresponds to.
    pub head_version: Version,
    /// Version `durable_head` corresponds to.
    pub durable_version: Version,
    /// The cursor the future was at when `durable_head` was produced.
    ///
    /// Rollback needs this: a future must resume where its *durable* work ended,
    /// not where its speculative work happened to reach. Restoring the speculative
    /// cursor would replay from the wrong node and produce a delta chain that does
    /// not continue the durable one.
    pub durable_cursor: NodeId,
    pub cursor: NodeId,
    pub status: FutureStatus,
    /// Versions this future produced, in order. The delta chain.
    pub chain: Vec<Version>,
    /// Effect occurrences, in order.
    pub effects: Vec<EffectRecord>,
    /// Next effect ordinal for this future. Part of every idempotency key.
    pub next_effect: u64,
    /// How many times this future has been rolled back and re-executed.
    pub attempts: u32,
    /// Children created by a fork and not yet settled.
    pub children: Vec<FutureId>,
    /// The durable selection that named this future, if any.
    pub score: Option<i64>,
}

impl Future {
    /// Creates a future at `base`. `parent` is `None` for the trunk.
    /// Creates a future at `base`. `parent` is `None` for the trunk.
    #[must_use]
    /// `path` is the stable fork-arm path from the root, and must be supplied here
    /// rather than patched in later: a key built from anything else is not stable
    /// under retry.
    pub fn new(
        id: FutureId,
        parent: Option<FutureId>,
        arm: Option<usize>,
        path: Vec<u32>,
        arm_end: Option<NodeId>,
        base: StateNode,
        version: Version,
    ) -> Self {
        Self {
            id,
            parent,
            arm,
            path,
            arm_end,
            base,
            base_version: version,
            head: base,
            durable_head: base,
            head_version: version,
            durable_version: version,
            durable_cursor: NodeId::new(0),
            cursor: NodeId::new(0),
            status: FutureStatus::Open,
            chain: Vec::new(),
            effects: Vec::new(),
            next_effect: 0,
            attempts: 0,
            children: Vec::new(),
            score: None,
        }
    }

    /// Number of steps executed.
    #[must_use]
    pub fn steps(&self) -> usize {
        self.chain.len()
    }

    /// The version of the future's head.
    #[must_use]
    pub fn head_version(&self) -> Version {
        self.head_version
    }

    /// Deferred effects: those recorded as intents and not yet issued.
    pub fn deferred_effects(&self) -> impl Iterator<Item = &EffectRecord> {
        self.effects
            .iter()
            .filter(|e| e.disposition == Disposition::Deferred)
    }

    /// Effects that already reached the world.
    pub fn performed_effects(&self) -> impl Iterator<Item = &EffectRecord> {
        self.effects
            .iter()
            .filter(|e| e.disposition.touched_world())
    }

    /// Whether the future wrote anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chain.is_empty() && self.effects.is_empty()
    }

    /// Allocates the next effect ordinal and its key.
    pub fn next_effect_key(&mut self) -> EffectId {
        let id = EffectId::new(self.next_effect);
        self.next_effect += 1;
        id
    }

    /// Checks that the delta chain is ordered.
    ///
    /// Deliberately weaker than "consecutive", and the reason is worth stating
    /// because the obvious version of this check is wrong. Versions are allocated
    /// **globally**, so with several futures running, one future's own versions
    /// are interleaved with another's: a trunk that steps before and after three
    /// children have run has the chain `[V1, V9]`, not `[V1, V2]`. Demanding
    /// consecutiveness would reject every valid branching execution.
    ///
    /// What actually guarantees a well formed chain is enforced elsewhere and
    /// more strongly: during replay, every delta's `parent` is compared against
    /// the head the previous record left behind, and a mismatch stops that future
    /// rather than guessing (see [`crate::kernel::recovery::rebuild`]). That is a
    /// per-record check against real state, which is strictly stronger than
    /// anything inferable from the version numbers alone.
    ///
    /// So this function checks the property the version numbers *can* express:
    /// each future's own versions are strictly increasing, so its history cannot
    /// be a permutation or a replay of itself.
    pub fn check_chain(&self) -> Result<(), FutureError> {
        let mut previous = self.base_version;
        for v in &self.chain {
            if *v <= previous {
                return Err(FutureError::BrokenChain {
                    future: self.id,
                    at: *v,
                });
            }
            previous = *v;
        }
        Ok(())
    }

    /// Discards everything after the durable head, and re-arms for a new attempt.
    ///
    /// Used after a failure. Two details are load bearing:
    ///
    /// * the effect ordinals are **rewound** to the durable head, so a
    ///   re-executed step reuses its original idempotency key. If they were not, a
    ///   retry after a crash would be a *second, distinct* effect as far as the
    ///   target is concerned, which is precisely the bug idempotency keys exist to
    ///   prevent;
    /// * effects that already touched the world stay in the journal. Dropping
    ///   them would make replay non-deterministic by re-observing, and would hide
    ///   the fact that something irreversible already happened.
    pub fn rollback_to_durable(&mut self, head: StateNode, version: Version) {
        let cursor = self.durable_cursor;
        self.head = head;
        self.durable_head = head;
        self.head_version = version;
        self.durable_version = version;
        let keep = self
            .chain
            .iter()
            .position(|v| *v == version)
            .map_or(0, |i| i + 1);
        self.chain.truncate(keep);
        let performed: Vec<EffectRecord> = self
            .effects
            .iter()
            .filter(|e| e.disposition.touched_world())
            .cloned()
            .collect();
        self.effects = performed;
        self.next_effect = self.effects.len() as u64;
        self.cursor = cursor;
        self.status = FutureStatus::Running;
        self.attempts += 1;
    }
}

/// What an evaluator sees.
///
/// It owns two clones of state rather than borrowing them, so a view can be
/// handed to an evaluator without keeping a borrow of the kernel alive. Evaluation
/// happens once per fork, so the copies are not on any hot path — and owning the
/// data is what makes the "read-only by construction" claim true rather than
/// merely intended: there is no handle here that could mutate anything.
#[derive(Clone, Debug)]
pub struct FutureView {
    pub id: FutureId,
    pub parent: Option<FutureId>,
    pub arm: Option<usize>,
    pub status: FutureStatus,
    pub head: State,
    pub base: State,
    pub steps: usize,
    pub cost_ms: u64,
    pub deferred_effects: usize,
    pub performed_effects: usize,
    pub irreversible_deferred: usize,
    pub children: Vec<FutureId>,
}

impl FutureView {
    /// The value of a state key on this future's head. The thing most evaluators
    /// want.
    #[must_use]
    pub fn get(&self, key: &str) -> i64 {
        self.head.get(key)
    }

    /// Keys whose value differs between this future's base and its head — its
    /// write set, derived rather than tracked.
    #[must_use]
    pub fn wrote(&self) -> Vec<(String, i64, i64)> {
        self.base.diff(&self.head)
    }
}

/// The whole lineage of one execution: the trunk plus every future it forked.
#[derive(Clone, Debug, Default)]
pub struct Lineage {
    futures: BTreeMap<FutureId, Future>,
}

impl Lineage {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a future. Fails if the id is taken, which would mean two
    /// different runs produced the same identity — a lineage bug, not a
    /// recoverable condition.
    pub fn insert(&mut self, future: Future) -> Result<(), FutureError> {
        if self.futures.contains_key(&future.id) {
            return Err(FutureError::AlreadySettled {
                future: future.id,
                status: future.status,
            });
        }
        self.futures.insert(future.id, future);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, id: FutureId) -> Option<&Future> {
        self.futures.get(&id)
    }

    /// Forgets a future and everything it forked. Used by rollback, which
    /// discards work that created it.
    pub fn remove(&mut self, id: FutureId) -> bool {
        let Some(f) = self.futures.remove(&id) else {
            return false;
        };
        for child in f.children {
            self.remove(child);
        }
        true
    }

    /// Whether the lineage knows this future.
    #[must_use]
    pub fn contains(&self, id: FutureId) -> bool {
        self.futures.contains_key(&id)
    }

    /// Futures in ascending id order, which is the deterministic iteration order
    /// every kernel decision uses.
    pub fn ordered(&self) -> Vec<(FutureId, &Future)> {
        self.futures.iter().map(|(id, f)| (*id, f)).collect()
    }

    /// Like [`Lineage::get`] but with a `Result`, so callers that already return
    /// one can use `?` on a lineage lookup.
    pub fn require(&self, id: FutureId) -> Result<&Future, FutureError> {
        self.get(id).ok_or(FutureError::Unknown(id))
    }

    /// Mutable counterpart of [`Lineage::require`].
    pub fn require_mut(&mut self, id: FutureId) -> Result<&mut Future, FutureError> {
        self.get_mut(id)
    }

    pub fn get_mut(&mut self, id: FutureId) -> Result<&mut Future, FutureError> {
        self.futures.get_mut(&id).ok_or(FutureError::Unknown(id))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.futures.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.futures.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&FutureId, &Future)> {
        self.futures.iter()
    }

    /// Mutable iteration in the same deterministic order as [`Lineage::iter`].
    pub fn iter_mut_pairs(&mut self) -> impl Iterator<Item = (FutureId, &mut Future)> {
        self.futures.iter_mut().map(|(id, f)| (*id, f))
    }

    /// Every future except the trunk, in id order.
    pub fn alternatives(&self) -> impl Iterator<Item = (&FutureId, &Future)> {
        self.futures
            .iter()
            .filter(|(id, _)| **id != crate::domain::ids::TRUNK)
    }

    /// Ids of futures in no terminal state.
    #[must_use]
    pub fn live(&self) -> Vec<FutureId> {
        self.futures
            .iter()
            .filter(|(_, f)| !f.status.is_terminal())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Ids of futures in a terminal state.
    #[must_use]
    pub fn settled(&self) -> Vec<FutureId> {
        self.futures
            .iter()
            .filter(|(_, f)| f.status.is_terminal())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Every state version any future reaches, i.e. the set the store must keep
    /// alive. Used by reclamation.
    #[must_use]
    pub fn reachable_nodes(&self) -> BTreeSet<StateNode> {
        let mut out = BTreeSet::new();
        for f in self.futures.values() {
            out.insert(f.base);
            out.insert(f.head);
            out.insert(f.durable_head);
        }
        out
    }

    /// Depth of the future tree, used by reports and by the isolation tests.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.futures
            .values()
            .filter_map(|f| f.parent)
            .map(|p| self.get(p).map_or(0, |_| 1))
            .sum()
    }

    /// Every live ancestor of `id`, nearest first. Used to prove that a commit
    /// only ever reaches the trunk through a recorded edge.
    #[must_use]
    pub fn ancestors(&self, id: FutureId) -> Vec<FutureId> {
        let mut out = Vec::new();
        let mut cursor = self.get(id).and_then(|f| f.parent);
        let mut guard = 0usize;
        while let Some(p) = cursor {
            if out.contains(&p) || guard > self.futures.len() {
                break; // a cycle is a bug; stop rather than loop
            }
            out.push(p);
            cursor = self.get(p).and_then(|f| f.parent);
            guard += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::TRUNK;

    fn future(id: u64) -> Future {
        Future::new(
            FutureId::new(id),
            if id == 0 { None } else { Some(TRUNK) },
            None,
            if id == 0 { Vec::new() } else { vec![id as u32] },
            None,
            StateNode::EMPTY,
            Version::ZERO,
        )
    }

    #[test]
    fn status_codes_round_trip() {
        for s in FutureStatus::ALL {
            assert_eq!(FutureStatus::from_code(s.code()), Some(s));
        }
        assert_eq!(FutureStatus::from_code(99), None);
    }

    #[test]
    fn terminal_statuses_are_exactly_the_four_ends() {
        let terminal: Vec<_> = FutureStatus::ALL
            .into_iter()
            .filter(|s| s.is_terminal())
            .collect();
        assert_eq!(
            terminal,
            vec![
                FutureStatus::Committed,
                FutureStatus::Rejected,
                FutureStatus::Abandoned,
                FutureStatus::Failed
            ]
        );
    }

    #[test]
    fn a_future_knows_where_its_arm_ends() {
        let trunk = Future::new(
            TRUNK,
            None,
            None,
            Vec::new(),
            None,
            StateNode::EMPTY,
            Version::ZERO,
        );
        assert!(trunk.arm_end.is_none());
        let child = Future::new(
            FutureId::new(1),
            Some(TRUNK),
            Some(0),
            vec![0],
            Some(NodeId::new(3)),
            StateNode::EMPTY,
            Version::ZERO,
        );
        assert_eq!(child.arm_end, Some(NodeId::new(3)));
    }

    #[test]
    fn a_fresh_future_is_empty_and_at_its_base() {
        let f = future(1);
        assert!(f.is_empty());
        assert_eq!(f.head, f.base);
        assert_eq!(f.status, FutureStatus::Open);
    }

    #[test]
    fn an_ordered_chain_validates_and_a_repeated_version_does_not() {
        let mut f = future(1);
        f.chain = vec![Version::new(1), Version::new(2), Version::new(3)];
        assert!(f.check_chain().is_ok());
        f.chain = vec![Version::new(1), Version::new(2), Version::new(2)];
        assert!(matches!(
            f.check_chain(),
            Err(FutureError::BrokenChain { at, .. }) if at == Version::new(2)
        ));
    }

    #[test]
    fn a_chain_may_skip_versions_because_they_belong_to_other_futures() {
        // The trunk steps at V1, three children run at V2..V7, and the trunk steps
        // again at V8. Its own chain is [V1, V8] and that is a *valid* execution.
        let mut f = future(1);
        f.chain = vec![Version::new(1), Version::new(8)];
        assert!(f.check_chain().is_ok());
    }

    #[test]
    fn effect_keys_are_reissued_after_a_rollback() {
        let mut f = future(1);
        assert_eq!(f.next_effect_key(), EffectId::new(0));
        assert_eq!(f.next_effect_key(), EffectId::new(1));
        f.rollback_to_durable(StateNode::EMPTY, Version::ZERO);
        // The key must not depend on the attempt, or a retry would be a second,
        // distinct effect as far as the target is concerned.
        assert_eq!(f.next_effect_key(), EffectId::new(0));
    }

    #[test]
    fn a_rollback_resumes_at_the_durable_cursor_not_the_speculative_one() {
        let mut f = future(1);
        f.durable_cursor = NodeId::new(4);
        f.cursor = NodeId::new(9);
        f.rollback_to_durable(StateNode::EMPTY, Version::ZERO);
        assert_eq!(f.cursor, NodeId::new(4), "resumed from durable work");
    }

    #[test]
    fn lineage_rejects_a_duplicate_identity() {
        let mut l = Lineage::new();
        l.insert(future(1)).unwrap();
        assert!(matches!(
            l.insert(future(1)),
            Err(FutureError::AlreadySettled { .. })
        ));
    }

    #[test]
    fn lineage_separates_live_from_settled() {
        let mut l = Lineage::new();
        let mut trunk = future(0);
        trunk.status = FutureStatus::Evaluable;
        l.insert(trunk).unwrap();
        let mut a = future(1);
        a.status = FutureStatus::Evaluable;
        l.insert(a).unwrap();
        let mut b = future(2);
        b.status = FutureStatus::Rejected;
        l.insert(b).unwrap();
        assert_eq!(l.live(), vec![FutureId::new(0), FutureId::new(1)]);
        assert_eq!(l.settled(), vec![FutureId::new(2)]);
    }

    #[test]
    fn ancestors_walk_up_and_stop_on_a_cycle() {
        let mut l = Lineage::new();
        l.insert(future(0)).unwrap();
        let mut a = future(1);
        a.parent = Some(FutureId::new(0));
        l.insert(a).unwrap();
        let mut b = future(2);
        b.parent = Some(FutureId::new(1));
        l.insert(b).unwrap();
        assert_eq!(
            l.ancestors(FutureId::new(2)),
            vec![FutureId::new(1), FutureId::new(0)]
        );
        // Introduce a cycle: future 0 claims 2 as its parent.
        l.get_mut(FutureId::new(0)).unwrap().parent = Some(FutureId::new(2));
        assert_eq!(l.ancestors(FutureId::new(2)).len(), 3);
    }

    #[test]
    fn reachable_nodes_include_bases_and_heads() {
        let mut l = Lineage::new();
        let mut f = future(1);
        f.base = StateNode::new(11);
        f.head = StateNode::new(12);
        f.durable_head = StateNode::new(11);
        l.insert(f).unwrap();
        let r = l.reachable_nodes();
        assert!(r.contains(&StateNode::new(11)));
        assert!(r.contains(&StateNode::new(12)));
    }
}
