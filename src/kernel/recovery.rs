//! Recovery: turning a durable ledger prefix back into a valid expected_execution graph.
//!
//! # The rule
//!
//! > Recovery reconstructs **only** what a valid expected_execution could have produced.
//!
//! Five rules, and each is a place where a naive replay would build a graph that
//! never existed:
//!
//! 1. **Only durable records count.** The caller hands over the durable prefix.
//!    Nothing speculative is invented, and nothing buffered is assumed.
//! 2. **A delta chain must be contiguous.** Every delta names the version it was
//!    applied to, so a gap is detectable from the record alone. A future whose
//!    chain has a gap is truncated to the last contiguous point rather than
//!    having a value guessed for it.
//! 3. **A `Committed` record is the only thing that moves the trunk.** A future's
//!    own steps never change the trunk, so a crash halfway through the winner's
//!    release cannot leave the trunk half-updated.
//! 4. **A durable commit that has not fully released is *owed* work.** The report
//!    says so explicitly and the kernel finishes it. That is the whole repair
//!    story for "the pointer flipped and then the process died", and it is why the
//!    commit and the release are separate steps.
//! 5. **Lineage is checked after the fact.** Every future's parent and children
//!    must agree, every referenced state must be in the store, and every chain
//!    must be contiguous. [`ExecutionGraph::validate`] is run before the graph is
//!    returned, so a caller cannot accidentally proceed on an inconsistent one.
//!
//! # What is *not* recovered
//!
//! Volatile scheduling state — which future the executor happened to be running,
//! where the wall clock was — is gone, and does not need to be: every future's
//! cursor is in the ledger, so the resumable position is re-derived from durable
//! facts only.

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::effect::{Disposition, EffectKey, EffectValue};
use crate::domain::future::{EffectRecord, Future, FutureError, FutureStatus, Lineage};
use crate::domain::ids::{EffectId, ExecutionId, FutureId, NodeId, PlanId, TRUNK};
use crate::domain::node::{StateNode, StateStore};
use crate::domain::state::State;
use crate::domain::version::{Sequence, Version};
use crate::ledger::journal::EffectJournal;
use crate::ledger::record::{LedgerRecord, Payload};
use crate::ports::StoreError;

/// Why a recovered graph is not usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryError {
    /// The ledger could not be read.
    Store(String),
    /// The ledger has no `Opened` record, so it is not an execution.
    NotAnExecution,
    /// A record belongs to a different expected_execution.
    WrongExecution {
        expected: ExecutionId,
        found: ExecutionId,
    },
    /// The plan in the ledger is not the plan the caller supplied.
    PlanMismatch { expected: PlanId, found: PlanId },
    /// A record names a future that was never created.
    Orphan { future: FutureId },
    /// A future's delta chain has a gap.
    BrokenChain { future: FutureId, at: Version },
    /// The rebuilt graph failed its own consistency checks.
    Inconsistent(String),
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoveryError::Store(e) => write!(f, "ledger error: {e}"),
            RecoveryError::NotAnExecution => {
                write!(
                    f,
                    "the ledger has no opening record, so it is not an execution"
                )
            }
            RecoveryError::WrongExecution { expected, found } => {
                write!(f, "expected records for {expected}, found {found}")
            }
            RecoveryError::PlanMismatch { expected, found } => {
                write!(f, "the ledger runs plan {found}, not plan {expected}")
            }
            RecoveryError::Orphan { future } => {
                write!(
                    f,
                    "a record refers to future {future}, which was never created"
                )
            }
            RecoveryError::BrokenChain { future, at } => {
                write!(f, "future {future} has a broken delta chain at {at}")
            }
            RecoveryError::Inconsistent(m) => write!(f, "recovered graph is inconsistent: {m}"),
        }
    }
}

impl std::error::Error for RecoveryError {}

impl From<crate::domain::future::FutureError> for RecoveryError {
    fn from(e: crate::domain::future::FutureError) -> Self {
        orphan(e)
    }
}

impl From<StoreError> for RecoveryError {
    fn from(e: StoreError) -> Self {
        RecoveryError::Store(e.to_string())
    }
}

/// A valid expected_execution graph rebuilt from a durable ledger prefix.
#[derive(Clone, Debug)]
pub struct ExecutionGraph {
    /// Which plan the expected_execution is running, from the `Opened` record.
    pub plan: PlanId,
    pub lineage: Lineage,
    pub states: StateStore,
    pub journal: EffectJournal,
    /// The authoritative head.
    pub trunk: StateNode,
    /// Version the trunk head corresponds to.
    pub trunk_version: Version,
    /// Highest version any record carried.
    pub version: Version,
    /// Next ledger sequence.
    pub sequence: Sequence,
    /// The attempt the last record was written in.
    pub attempt: u32,
    /// Which future is authoritative. `TRUNK` until a commit happens.
    pub authoritative: FutureId,
    /// The future a durable selection named.
    pub selected: Option<FutureId>,
    /// Deferred effects a durable commit has not released yet, in issue order.
    pub owed: Vec<(FutureId, EffectId)>,
    /// Where each future should resume, from its last record.
    pub cursors: BTreeMap<FutureId, NodeId>,
    /// Records accepted.
    pub records: usize,
    /// Highest timestamp seen, in microseconds.
    pub last_timestamp_us: u64,
    /// Futures that exist and are not in a terminal state.
    pub live: usize,
    /// Futures in a terminal state.
    pub settled: usize,
}

impl ExecutionGraph {
    /// The trunk's state — the authoritative value.
    #[must_use]
    pub fn trunk_state(&self) -> State {
        self.states.get(self.trunk).cloned().unwrap_or_default()
    }

    /// A future's state.
    #[must_use]
    pub fn state_of(&self, future: FutureId) -> Option<State> {
        let f = self.lineage.get(future)?;
        self.states.get(f.head).cloned()
    }

    /// Every future that a fork created, in id order.
    pub fn alternatives(&self) -> impl Iterator<Item = (FutureId, &Future)> {
        self.lineage.alternatives().map(|(id, f)| (*id, f))
    }

    /// Checks the invariants that do not depend on any runtime check having
    /// fired. A `true` here is a claim the runtime can defend.
    ///
    /// Run automatically at the end of [`rebuild`]; exposed so a test or an
    /// operator tool can re-run it against a graph loaded elsewhere.
    pub fn validate(&self) -> Result<(), String> {
        if !self.lineage.contains(TRUNK) {
            return Err("the graph has no trunk".to_owned());
        }
        for (id, f) in self.lineage.iter() {
            f.check_chain().map_err(|e| format!("future {id}: {e}"))?;
            if self.states.get(f.head).is_none() {
                return Err(format!("future {id}: head {} is not in the store", f.head));
            }
            if self.states.get(f.base).is_none() {
                return Err(format!("future {id}: base {} is not in the store", f.base));
            }
            if let Some(parent) = f.parent {
                if !self.lineage.contains(parent) {
                    return Err(format!("future {id}: parent {parent} is missing"));
                }
            }
            for child in &f.children {
                match self.lineage.get(*child) {
                    None => return Err(format!("future {id}: child {child} is missing")),
                    Some(c) => {
                        if c.parent != Some(*id) {
                            return Err(format!(
                                "future {id}: child {child} disagrees about its parent"
                            ));
                        }
                    }
                }
            }
        }
        for (id, cursor) in &self.cursors {
            if !self.lineage.contains(*id) {
                return Err(format!("a cursor names future {id}, which does not exist"));
            }
            let _ = cursor;
        }
        Ok(())
    }
}

/// Rebuilds an expected_execution graph from the durable prefix of a ledger.
///
/// `records` must be the *durable* prefix in ledger order. Anything the store has
/// buffered but not made durable is not in it, and that is the entire reason
/// recovery is sound: there is no path by which a speculative record can reach
/// this function.
pub fn rebuild(
    records: &[LedgerRecord],
    expected_execution: ExecutionId,
    expected_plan: PlanId,
) -> Result<ExecutionGraph, RecoveryError> {
    let mut states = StateStore::new();
    let mut lineage = Lineage::new();
    let mut journal = EffectJournal::new();
    // `heads[future] = (state node, version that node corresponds to)`.
    let mut heads: BTreeMap<FutureId, (StateNode, Version)> = BTreeMap::new();
    let mut cursors: BTreeMap<FutureId, NodeId> = BTreeMap::new();
    let mut broken: BTreeSet<FutureId> = BTreeSet::new();

    let mut plan: Option<PlanId> = None;
    let mut trunk = StateNode::EMPTY;
    let mut trunk_version = Version::ZERO;
    let mut version = Version::ZERO;
    let mut sequence = Sequence::ZERO;
    let mut attempt = 0u32;
    let mut authoritative = TRUNK;
    let mut selected: Option<FutureId> = None;
    let mut owed: Vec<(FutureId, EffectId)> = Vec::new();
    let mut released: BTreeSet<(FutureId, EffectId)> = BTreeSet::new();
    let mut accepted = 0usize;
    let mut last_timestamp_us = 0u64;

    for record in records {
        if record.execution != expected_execution {
            return Err(RecoveryError::WrongExecution {
                expected: expected_execution,
                found: record.execution,
            });
        }
        sequence = record.sequence.next();
        attempt = record.attempt.value() as u32;
        last_timestamp_us = last_timestamp_us.max(record.timestamp_us);
        accepted += 1;

        match &record.payload {
            Payload::Opened { plan: p } => {
                if *p != expected_plan {
                    return Err(RecoveryError::PlanMismatch {
                        expected: expected_plan,
                        found: *p,
                    });
                }
                plan = Some(*p);
                lineage.insert(Future::new(
                    TRUNK,
                    None,
                    None,
                    Vec::new(),
                    Some(NodeId::new(u64::MAX)),
                    StateNode::EMPTY,
                    Version::ZERO,
                ))?;
                heads.insert(TRUNK, (StateNode::EMPTY, Version::ZERO));
                // A trunk that has only an `Opened` record has done nothing, so
                // its resume cursor is whatever the plan's entry turns out to be;
                // the first `Step` or `Fork` for `TRUNK` overwrites this, and until
                // then nothing can have been executed from the wrong place.
                cursors.insert(TRUNK, NodeId::new(0));
            }
            Payload::Step { delta, cursor } => {
                if broken.contains(&delta.future) {
                    continue;
                }
                let (node, base_version) =
                    heads
                        .get(&delta.future)
                        .copied()
                        .ok_or(RecoveryError::Orphan {
                            future: delta.future,
                        })?;
                if delta.parent != base_version {
                    // The chain has a gap. We never write such a ledger, so this
                    // means the store lied; the only safe answer is to stop
                    // applying to this future rather than guess.
                    broken.insert(delta.future);
                    continue;
                }
                let mut state = states.get(node).cloned().unwrap_or_default();
                state.apply(&delta.operation).map_err(|e| {
                    RecoveryError::BrokenChain {
                        future: delta.future,
                        at: delta.version,
                    }
                    .with_detail(e.to_string())
                })?;
                let head = states.intern(&state);
                heads.insert(delta.future, (head, delta.version));
                cursors.insert(delta.future, *cursor);
                let f = lineage.get_mut(delta.future)?;
                f.chain.push(delta.version);
                f.head = head;
                f.head_version = delta.version;
            }
            Payload::Effect { delta, cursor } => {
                let disposition = delta.disposition.clone();
                let key = EffectKey::new(
                    expected_execution,
                    lineage
                        .get(delta.future)
                        .map(|f| f.path.clone())
                        .unwrap_or_default(),
                    delta.ordinal,
                );
                match &disposition {
                    Disposition::Performed { result } | Disposition::Replayed { result } => {
                        journal.record_performed(
                            key.clone(),
                            delta.future,
                            delta.op.clone(),
                            delta.class,
                            result.clone(),
                        );
                    }
                    Disposition::Deferred => {
                        journal.record_intent(
                            key.clone(),
                            delta.future,
                            delta.op.clone(),
                            delta.class,
                        );
                    }
                }
                // A `Read` folds its journalled value into the future's state, so
                // the head moves here exactly as it does for a step. Skipping this
                // is the kind of omission that produces a graph which validates and
                // is still wrong: the trunk's own state would be missing every
                // observation the run made.
                let (prev_node, _) =
                    heads
                        .get(&delta.future)
                        .copied()
                        .ok_or(RecoveryError::Orphan {
                            future: delta.future,
                        })?;
                let mut state = states.get(prev_node).cloned().unwrap_or_default();
                if let (Some(into), Some(value)) = (&delta.reads_into, disposition.result()) {
                    state
                        .apply(&crate::domain::Operation::Set {
                            key: into.clone(),
                            value: value.as_int().unwrap_or(0),
                        })
                        .map_err(|e| RecoveryError::Inconsistent(e.to_string()))?;
                }
                let head = states.intern(&state);
                let f = lineage.get_mut(delta.future).map_err(orphan)?;
                f.effects.push(EffectRecord {
                    key,
                    class: delta.class,
                    op: delta.op.clone(),
                    disposition: disposition.clone(),
                    compensation: delta.compensation.clone(),
                    writes: delta.writes.clone(),
                });
                f.next_effect = f.next_effect.max(delta.ordinal.value() + 1);
                f.chain.push(delta.version);
                f.head = head;
                f.head_version = delta.version;
                heads.insert(delta.future, (head, delta.version));
                cursors.insert(delta.future, *cursor);
            }
            Payload::Fork {
                parent,
                cursor,
                children,
            } => {
                let (parent_head, parent_version) = heads
                    .get(parent)
                    .copied()
                    .ok_or(RecoveryError::Orphan { future: *parent })?;
                for child in children {
                    // Rebuild the same path the kernel derived, so keys recovered
                    // from this lineage match the keys the kernel would mint.
                    let mut path = lineage
                        .get(*parent)
                        .map(|f| f.path.clone())
                        .unwrap_or_default();
                    path.push(child.arm as u32);
                    lineage
                        .insert(Future::new(
                            child.future,
                            Some(*parent),
                            Some(child.arm),
                            path,
                            Some(child.end),
                            parent_head,
                            parent_version,
                        ))
                        .map_err(orphan)?;
                    heads.insert(child.future, (parent_head, parent_version));
                    cursors.insert(child.future, child.root);
                    let p = lineage.require_mut(*parent)?;
                    if !p.children.contains(&child.future) {
                        p.children.push(child.future);
                    }
                }
                cursors.insert(*parent, *cursor);
            }
            Payload::Selected {
                among,
                winner,
                cursor,
                ..
            } => {
                selected = Some(*winner);
                for id in among {
                    if *id == *winner {
                        lineage.require_mut(*winner)?.status = FutureStatus::SelectionRecorded;
                    } else {
                        lineage.require_mut(*id).map_err(orphan)?.status = FutureStatus::Rejected;
                    }
                }
                // The record carries the *executing* future's next cursor, and the
                // executing future is the winner's parent — the `Select` node lives
                // in the parent's plan, not in any arm. Recording it against the
                // winner instead would point a settled future at the middle of the
                // trunk's program, and would leave the trunk to re-run a decision
                // the log had already made.
                if let Some(parent) = lineage.get(*winner).and_then(|f| f.parent) {
                    cursors.insert(parent, *cursor);
                }
            }
            Payload::Committed {
                future,
                cursor,
                trunk: new_trunk,
                trunk_version: new_version,
                release,
            } => {
                // A commit record names a *result*, and recovery must be able to
                // reproduce that result rather than merely believe it. The three
                // inputs are all recoverable — the winner's base, the winner's head
                // and the trunk's head — so the merge is recomputed, interned, and
                // its address compared with the one the record claims. A mismatch
                // means the ledger and the states disagree, which is corruption
                // rather than a crash, and is reported as such.
                let base_state = lineage
                    .get(*future)
                    .and_then(|f| states.get(f.base).cloned())
                    .unwrap_or_default();
                let ours = lineage
                    .get(*future)
                    .and_then(|f| states.get(f.head).cloned())
                    .unwrap_or_default();
                // The trunk's current head is the *trunk future's* head, which is
                // what the executing kernel merged against. Reading the `trunk`
                // variable here would compare against the empty state, because
                // that variable only moves when a commit record is replayed.
                let theirs = heads
                    .get(&TRUNK)
                    .and_then(|(n, _)| states.get(*n).cloned())
                    .unwrap_or_default();
                let merged =
                    crate::domain::state::State::three_way_merge(&base_state, &ours, &theirs)
                        .map_err(|conflicts| {
                            RecoveryError::Inconsistent(format!(
                                "the commit of {future} cannot be re-derived: {}",
                                conflicts
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            ))
                        })?;
                let recomputed = states.intern(&merged);
                if recomputed != *new_trunk {
                    return Err(RecoveryError::Inconsistent(format!(
                        "the commit of {future} claims trunk {new_trunk} but the merge of \
                         its base, its head and the trunk is {recomputed}"
                    )));
                }
                trunk = *new_trunk;
                trunk_version = *new_version;
                authoritative = *future;
                for ordinal in release {
                    owed.push((*future, *ordinal));
                }
                // A durable commit is a fact, so recovery replays it as a *terminal*
                // outcome, not as work in progress. Leaving it non-terminal would
                // mean a recovered graph reports a finished execution as still
                // running, and would let something try to re-commit it.
                lineage.require_mut(*future)?.status = FutureStatus::Committed;
                // The committed future is now the trunk: its head is the merged
                // state, which is what any later reader must see.
                lineage.require_mut(*future)?.head = *new_trunk;
                lineage.require_mut(*future)?.head_version = *new_version;
                lineage.require_mut(TRUNK)?.head = *new_trunk;
                lineage.require_mut(TRUNK)?.head_version = *new_version;
                heads.insert(TRUNK, (*new_trunk, *new_version));
                heads.insert(*future, (*new_trunk, *new_version));
                // The `Commit` node also lives in the *parent's* plan, so the
                // cursor in this record is the parent's next cursor. The committed
                // future is advanced to it as well, because that is what the
                // executing runtime does — but it is the parent a resume must not
                // leave standing on the commit it has already made.
                if let Some(parent) = lineage.get(*future).and_then(|f| f.parent) {
                    cursors.insert(parent, *cursor);
                }
                cursors.insert(*future, *cursor);
            }
            Payload::Settled { future, status } => {
                lineage.get_mut(*future).map_err(orphan)?.status = *status;
            }
            Payload::Released { future, ordinal } => {
                // The effect was issued. Marking the journal is not cosmetic: the
                // kernel decides what a commit still owes from `pending_release`,
                // so a recovered journal that forgot a release would issue the same
                // effect a second time. It would be deduped by key at the target,
                // but the *record* of what happened would be wrong, and that is the
                // thing recovery exists to get right.
                released.insert((*future, *ordinal));
                let key = EffectKey::new(
                    expected_execution,
                    lineage
                        .get(*future)
                        .map(|f| f.path.clone())
                        .unwrap_or_default(),
                    *ordinal,
                );
                if journal.get(&key).is_some_and(|e| e.deferred) {
                    journal.mark_issued(&key, EffectValue::Unit);
                }
            }
        }
        version = version.max(record.version);
    }

    let Some(plan) = plan else {
        return Err(RecoveryError::NotAnExecution);
    };

    // A durable commit that has not fully released is owed work.
    owed.retain(|k| !released.contains(k));

    // Freeze the head of every future.
    for (id, (node, at)) in &heads {
        if let Ok(f) = lineage.require_mut(*id) {
            f.head = *node;
            f.head_version = *at;
            f.durable_head = *node;
            f.durable_version = *at;
        }
    }
    // The trunk head *is* the trunk future's head. Only a `Committed` record makes
    // some other future authoritative, and that handler already moves the trunk
    // future's head to the merged state, so deriving it here is correct in both
    // cases — and it avoids a second source of truth for "where the trunk is".
    if let Some(t) = lineage.get(TRUNK) {
        trunk = t.head;
        trunk_version = t.head_version;
    }
    if let Ok(f) = lineage.require_mut(authoritative) {
        if f.status == FutureStatus::Committing {
            f.status = FutureStatus::Committed;
        }
    }

    let live = lineage.live().len();
    let settled = lineage.settled().len();
    let graph = ExecutionGraph {
        plan,
        lineage,
        states,
        journal,
        trunk,
        trunk_version,
        version,
        sequence,
        attempt,
        authoritative,
        selected,
        owed,
        cursors,
        records: accepted,
        last_timestamp_us,
        live,
        settled,
    };
    graph.validate().map_err(RecoveryError::Inconsistent)?;
    Ok(graph)
}

fn orphan(e: FutureError) -> RecoveryError {
    match e {
        FutureError::Unknown(id) => RecoveryError::Orphan { future: id },
        other => RecoveryError::Inconsistent(other.to_string()),
    }
}

impl RecoveryError {
    fn with_detail(self, detail: String) -> Self {
        match self {
            RecoveryError::BrokenChain { future, at } => {
                RecoveryError::BrokenChain { future, at }.with_note(detail)
            }
            other => other,
        }
    }

    fn with_note(self, note: String) -> Self {
        match self {
            RecoveryError::BrokenChain { future, at } => RecoveryError::Inconsistent(format!(
                "future {future}: delta chain broken at {at} ({note})"
            )),
            other => other,
        }
    }
}
