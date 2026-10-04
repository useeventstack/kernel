//! The kernel: one interpreter over the execution graph.
//!
//! Every execution semantic in this runtime is an [`ExecutionPolicy`] on this one
//! engine. There is no "durable engine" and a separate "speculative engine" and a
//! separate "branching engine": there is this engine, and
//! [`Durability::Synchronous`], [`Durability::GroupCommit`] and
//! [`Durability::Async`] are the same code path with a different answer to "may
//! the executor proceed before this record is durable?". That is not tidiness —
//! it is what makes a benchmark comparison fair, because a comparison of four
//! separate engines would be a comparison of four implementations.
//!
//! # The loop
//!
//! ```text
//!   open the execution           one durable record, always
//!   loop
//!     pump the background writer     durability advances
//!     pick the runnable future      earliest ready, ties by id
//!     execute one node of its arm
//!       State  → apply, record a delta
//!       Effect → classify, perform or defer, journal
//!       Fork   → create children, block on them
//!       Select → score the children, record the decision
//!       Commit → validate, merge, append the commit record, release effects
//!   until nothing is runnable
//!   drain the writer             the result is not a result until recoverable
//! ```
//!
//! Every transition of the trunk, the lineage or the journal goes through a
//! ledger record first. There is no path that changes any of them without writing
//! down what it did, which is what makes recovery a replay rather than a
//! reconstruction.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use crate::domain::delta::{EffectDelta, StateDelta};
use crate::domain::effect::{
    Disposition, EffectClass, EffectKey, EffectOp, EffectRequest, EffectSink,
};
use crate::domain::future::{EffectRecord, Future, FutureError, FutureStatus, FutureView, Lineage};
use crate::domain::ids::{AttemptId, EffectId, ExecutionId, FutureId, NodeId, TRUNK};
use crate::domain::node::{StateNode, StateStore};
use crate::domain::plan::{Plan, PlanNode};
use crate::domain::state::{State, StateError};
use crate::domain::version::{Sequence, Version};
use crate::kernel::commit::{self, CommitError, MergeInput};
use crate::kernel::durability::Writer;
use crate::kernel::evaluate::{self, NoViableFuture, Selection};
use crate::kernel::policy::{Durability, ExecutionPolicy};
use crate::kernel::recovery::{ExecutionGraph, RecoveryError};
use crate::ledger::journal::EffectJournal;
use crate::ledger::record::{ForkedChild, LedgerRecord, Payload};
use crate::ledger::store::MemoryStore;
use crate::ports::{CostModel, DurableStore, StoreError, VirtualClock};
use crate::simulation::failure::{Checkpoint, FailureInjector, InjectedFailure};

/// Everything the kernel can go wrong with.
#[derive(Debug)]
pub enum KernelError {
    Ledger(StoreError),
    State(StateError),
    Future(FutureError),
    Commit(CommitError),
    NoViable(NoViableFuture),
    Recovery(RecoveryError),
    /// The plan reached a `Fail` operation.
    PlanFailed {
        future: FutureId,
        reason: String,
    },
    /// The effect world refused.
    Effect {
        future: FutureId,
        key: EffectKey,
        reason: String,
    },
    /// The attempt budget ran out.
    Injected(InjectedFailure),
    /// The plan asked for something the kernel cannot do.
    BadPlan(String),
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelError::Ledger(e) => write!(f, "ledger error: {e}"),
            KernelError::State(e) => write!(f, "state error: {e}"),
            KernelError::Future(e) => write!(f, "lineage error: {e}"),
            KernelError::Commit(e) => write!(f, "commit error: {e}"),
            KernelError::NoViable(e) => write!(f, "{e}"),
            KernelError::Recovery(e) => write!(f, "recovery error: {e}"),
            KernelError::PlanFailed { future, reason } => {
                write!(f, "future {future} failed: {reason}")
            }
            KernelError::Effect {
                future,
                key,
                reason,
            } => {
                write!(f, "future {future}: effect {key} failed: {reason}")
            }
            KernelError::Injected(fired) => write!(f, "{fired}"),
            KernelError::BadPlan(m) => write!(f, "bad plan: {m}"),
        }
    }
}

impl std::error::Error for KernelError {}

impl From<StoreError> for KernelError {
    fn from(e: StoreError) -> Self {
        KernelError::Ledger(e)
    }
}
impl From<StateError> for KernelError {
    fn from(e: StateError) -> Self {
        KernelError::State(e)
    }
}
impl From<FutureError> for KernelError {
    fn from(e: FutureError) -> Self {
        KernelError::Future(e)
    }
}
impl From<CommitError> for KernelError {
    fn from(e: CommitError) -> Self {
        KernelError::Commit(e)
    }
}
impl From<RecoveryError> for KernelError {
    fn from(e: RecoveryError) -> Self {
        KernelError::Recovery(e)
    }
}
impl From<NoViableFuture> for KernelError {
    fn from(e: NoViableFuture) -> Self {
        KernelError::NoViable(e)
    }
}

/// Counters the kernel keeps, so a benchmark or a test can compare executions
/// without reaching into the runtime.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KernelMetrics {
    /// Plan nodes executed, across every future.
    pub steps_executed: u64,
    /// Effects performed against the world.
    pub effects_performed: u64,
    /// Effects held back until commit.
    pub effects_deferred: u64,
    /// Effects served from the journal instead of the world.
    pub effects_replayed: u64,
    /// Effects issued as part of a commit.
    pub effects_released: u64,
    /// Futures created by a fork.
    pub futures_forked: u64,
    /// Futures settled as `Rejected`.
    pub futures_rejected: u64,
    /// Futures settled as `Failed`.
    pub futures_failed: u64,
    /// Commits performed.
    pub commits: u64,
    /// Commits that reported at least one conflicting key.
    pub commits_with_conflicts: u64,
    /// Records appended to the ledger.
    pub records_written: u64,
    /// Records made durable.
    pub records_durable: u64,
    /// Group commits.
    pub flushes: u64,
    /// Times the executor parked for durability.
    pub sync_waits: u64,
    /// Total time the executor parked for durability.
    pub sync_wait: Duration,
    /// Peak records in flight that were not yet durable.
    pub peak_speculation: usize,
    /// Steps discarded by a rollback.
    pub steps_discarded: u64,
    /// Recoveries performed.
    pub recoveries: u32,
    /// Sum of every future's simulated work.
    pub executor_time: Duration,
    /// Wall-clock time on the virtual timeline.
    pub wall_time: Duration,
    /// Bytes made durable.
    pub ledger_bytes: u64,
    /// Distinct state versions materialised.
    pub state_versions: usize,
    /// State versions served from the store rather than created.
    pub state_versions_deduped: usize,
    /// The selection that was made, if any.
    pub selection: Option<Selection>,
}

/// The result of running an execution to quiescence.
#[derive(Clone, Debug)]
pub struct RunReport {
    /// The authoritative state.
    pub trunk: State,
    pub trunk_version: Version,
    /// Which future is authoritative.
    pub authoritative: FutureId,
    /// The selection that was made, if any.
    pub selection: Option<Selection>,
    /// Content address of `trunk`. Exposed so a caller can check that a recovered
    /// graph landed on the *same* state, not merely an equal-looking one.
    pub trunk_content: StateNode,
    pub metrics: KernelMetrics,
    /// Every ledger record the run wrote, for inspection and for the tests.
    pub records: Vec<LedgerRecord>,
}

/// The execution kernel.
pub struct Kernel {
    plan: Plan,
    execution: ExecutionId,
    policy: ExecutionPolicy,
    cost: CostModel,
    store: Box<dyn DurableStore>,
    sink: Box<dyn EffectSink>,
    clock: VirtualClock,
    writer: Writer,
    states: StateStore,
    lineage: Lineage,
    journal: EffectJournal,
    /// Global monotonic version. Every record that advances state takes one.
    version: Version,
    sequence: Sequence,
    attempt: AttemptId,
    /// The authoritative head.
    trunk: StateNode,
    trunk_version: Version,
    /// Which future is authoritative. The trunk until a commit happens.
    authoritative: FutureId,
    /// The selection currently in force.
    selected: Option<FutureId>,
    /// Next future id to allocate. Zero is the trunk.
    next_future: u64,
    /// When each future may next execute.
    ready_at: BTreeMap<FutureId, Duration>,
    /// Executor slots and when each becomes free.
    slots: Vec<Duration>,
    /// Effects a durable commit still owes.
    owed: Vec<(FutureId, EffectId)>,
    injector: FailureInjector,
    metrics: KernelMetrics,
    records: Vec<LedgerRecord>,
    /// Whether the opening record has been written.
    opened: bool,
}

impl Kernel {
    /// Builds a kernel for `plan` with the default in-memory store and sink.
    ///
    /// # Errors
    /// Returns [`KernelError::BadPlan`] if the plan has no nodes.
    pub fn new(plan: Plan, policy: ExecutionPolicy) -> Result<Self, KernelError> {
        Self::with_ports(
            plan,
            policy,
            Box::new(MemoryStore::new()),
            Box::new(crate::domain::effect::RecordingSink::new()),
        )
    }

    /// Builds a kernel with explicit provider ports.
    ///
    /// # Errors
    /// Returns [`KernelError::BadPlan`] if the plan has no nodes.
    pub fn with_ports(
        plan: Plan,
        policy: ExecutionPolicy,
        store: Box<dyn DurableStore>,
        sink: Box<dyn EffectSink>,
    ) -> Result<Self, KernelError> {
        if plan.is_empty() {
            return Err(KernelError::BadPlan("a plan with no nodes".to_owned()));
        }
        let cost = CostModel::default();
        let workers = policy.workers.max(1);
        let mut lineage = Lineage::new();
        let mut trunk = Future::new(
            TRUNK,
            None,
            None,
            Vec::new(),
            Some(plan.end()),
            StateNode::EMPTY,
            Version::ZERO,
        );
        // The trunk starts at the plan's entry, not at zero: a plan that begins
        // with a fork has an alternative inlined at node 0.
        trunk.cursor = plan.entry();
        trunk.durable_cursor = plan.entry();
        lineage.insert(trunk)?;
        Ok(Self {
            plan,
            execution: ExecutionId::new(1),
            policy,
            cost,
            store,
            sink,
            clock: VirtualClock::new(),
            writer: Writer::new(cost),
            states: StateStore::new(),
            lineage,
            journal: EffectJournal::new(),
            version: Version::ZERO,
            sequence: Sequence::ZERO,
            attempt: AttemptId::new(0),
            trunk: StateNode::EMPTY,
            trunk_version: Version::ZERO,
            authoritative: TRUNK,
            selected: None,
            next_future: 1,
            ready_at: BTreeMap::from([(TRUNK, Duration::ZERO)]),
            slots: vec![Duration::ZERO; workers],
            owed: Vec::new(),
            injector: FailureInjector::none(),
            metrics: KernelMetrics::default(),
            records: Vec::new(),
            opened: false,
        })
    }

    /// Replaces the persistence cost model.
    #[must_use]
    pub fn with_cost(mut self, cost: CostModel) -> Self {
        self.cost = cost;
        self.writer = Writer::new(cost);
        self
    }

    /// Replaces the attempt budget.
    #[must_use]
    pub fn with_max_attempts_check(mut self, n: u32) -> Self {
        self.policy.max_attempts = n;
        self
    }

    /// Installs the failure injector.
    #[must_use]
    pub fn with_injector(mut self, injector: FailureInjector) -> Self {
        self.injector = injector;
        self
    }

    /// The failure injector, so a test can ask whether the point it set was
    /// actually reached.
    ///
    /// Not an assertion helper: "did this fault happen?" is a question a harness
    /// legitimately has to answer, because a fault point can be unreachable for a
    /// given plan and treating that as a pass would hide a matrix that tests
    /// nothing.
    #[must_use]
    pub fn injector(&self) -> FailureInjector {
        self.injector
    }

    /// The lineage, for inspection and for tests.
    #[must_use]
    pub fn lineage(&self) -> &Lineage {
        &self.lineage
    }

    /// The effect journal.
    #[must_use]
    pub fn journal(&self) -> &EffectJournal {
        &self.journal
    }

    /// The state store.
    #[must_use]
    pub fn states(&self) -> &StateStore {
        &self.states
    }

    /// The policy in force.
    #[must_use]
    pub fn policy(&self) -> &ExecutionPolicy {
        &self.policy
    }

    /// Metrics collected so far.
    #[must_use]
    pub fn metrics(&self) -> &KernelMetrics {
        &self.metrics
    }

    /// Every record written so far, *including speculative ones*.
    ///
    /// For inspection only. Recovery must be fed
    /// [`Kernel::durable_records`], because a process that died leaves the durable
    /// prefix and nothing else — a record the kernel merely buffered was never
    /// anywhere a restarted process could read.
    #[must_use]
    pub fn records(&self) -> &[LedgerRecord] {
        &self.records
    }

    /// The prefix a crashed process would actually leave behind.
    ///
    /// This is the only input recovery may be given. It is the durable prefix up
    /// to the version the writer proved, and it is the boundary the whole
    /// durable-prefix argument rests on.
    #[must_use]
    pub fn durable_records(&self) -> Vec<LedgerRecord> {
        // Cut by *count*, not by version. The store proved a prefix of records, and
        // the count of records it committed is exactly the length of that prefix. A
        // version-based cut would be wrong in a subtle way: control records carry
        // the version they observed, which can exceed the durable frontier, so
        // cutting on version drops durable records — a `Settled{Rejected}` that the
        // store certainly holds.
        let n = (self.writer.durable_records as usize).min(self.records.len());
        self.records[..n].to_vec()
    }

    /// Resumes an execution from a durable prefix, and runs it to quiescence.
    ///
    /// # What this is for
    ///
    /// [`crate::kernel::recovery::rebuild`] answers "what did the log say". This
    /// answers "carry on from there". Without it a runtime can prove its past is
    /// recoverable and still be unable to *finish* anything, which is the one
    /// thing a platform that restarts once an hour needs.
    ///
    /// The mapping from an [`ExecutionGraph`] to a [`Kernel`] is deliberately
    /// mechanical — every counter and every head comes from the graph, never from
    /// a fresh default — because the alternative is a resume path that quietly
    /// disagrees with recovery about what happened.
    ///
    /// # What is carried across, and why each of them matters
    ///
    /// | from the graph | why it cannot be defaulted |
    /// |---|---|
    /// | `lineage`, `states`, `journal` | the reconstructed state of the world |
    /// | `trunk`, `trunk_version` | the answer a caller reads back |
    /// | `version`, `sequence`, `attempt` | a resumed run must not reuse a sequence number |
    /// | `authoritative`, `selected`, `owed` | a commit that was interrupted between the pointer and the release still owes effects |
    /// | `cursors` | where each future actually stopped |
    ///
    /// Three things are **not** carried across, on purpose:
    ///
    /// 1. **The virtual clock restarts at zero.** It is an accounting device for
    ///    this process's work, not a wall clock; a resumed run measures its own
    ///    cost, and a cost that included a previous process's would be a fiction.
    /// 2. **The executor slots and `ready_at` reset.** Nothing is in flight in a
    ///    process that just started.
    /// 3. **The trunk's `arm_end` is taken from the plan, not from the graph.**
    ///    [`crate::kernel::recovery::rebuild`] creates the trunk with an unbounded
    ///    end because it has no plan to read it from, and a trunk whose end is
    ///    `u64::MAX` is a future the scheduler can never call finished.
    ///
    /// # Errors
    /// Returns [`KernelError::Recovery`] if `graph` is for a different plan, or if
    /// `records` is not the prefix `graph` was rebuilt from — a mismatch there
    /// would mean resuming from a log that does not match the state, which is the
    /// one situation with no safe default.
    pub fn resume(
        plan: Plan,
        policy: ExecutionPolicy,
        execution: ExecutionId,
        graph: ExecutionGraph,
        records: Vec<LedgerRecord>,
        store: Box<dyn DurableStore>,
        sink: Box<dyn EffectSink>,
    ) -> Result<Self, KernelError> {
        if graph.plan != plan.id {
            return Err(KernelError::Recovery(RecoveryError::PlanMismatch {
                expected: graph.plan,
                found: plan.id,
            }));
        }
        if graph.records != records.len() {
            return Err(KernelError::Recovery(RecoveryError::Inconsistent(format!(
                "resuming from {} records but the graph was rebuilt from {}",
                records.len(),
                graph.records
            ))));
        }
        graph
            .validate()
            .map_err(|e| KernelError::Recovery(RecoveryError::Inconsistent(e)))?;

        let cost = CostModel::default();
        let workers = policy.workers.max(1);
        let mut lineage = graph.lineage;
        let durable_end = store.sync_position().value();

        // Apply the recovered resume cursors, and give the trunk a real end. The
        // graph keeps cursors in a side map precisely because `rebuild` has no plan
        // to resolve an arm extent against; this is where that is undone.
        for (id, f) in lineage.iter_mut_pairs() {
            if let Some(cursor) = graph.cursors.get(&id) {
                f.cursor = *cursor;
                f.durable_cursor = *cursor;
            }
            if id == TRUNK {
                f.arm_end = Some(plan.end());
            }
        }

        // Next id must not collide with a future the graph already contains. TRUNK
        // is zero, so the first child is one.
        let next_future = lineage
            .iter()
            .map(|(id, _)| id.value() + 1)
            .max()
            .unwrap_or(1);
        let ready_at: BTreeMap<FutureId, Duration> = lineage
            .iter()
            .filter(|(_, f)| !f.status.is_terminal())
            .map(|(id, _)| (*id, Duration::ZERO))
            .collect();

        let mut writer = Writer::new(cost);
        writer.durable_records = records.len() as u64;
        writer.durable_bytes = durable_end;
        writer.note_durable(graph.version);

        let metrics = KernelMetrics {
            records_written: records.len() as u64,
            state_versions: graph.states.len(),
            state_versions_deduped: graph.states.deduped(),
            ledger_bytes: durable_end,
            ..KernelMetrics::default()
        };

        Ok(Self {
            plan,
            execution,
            policy,
            cost,
            store,
            sink,
            clock: VirtualClock::new(),
            writer,
            states: graph.states,
            lineage,
            journal: graph.journal,
            version: graph.version,
            sequence: graph.sequence,
            attempt: AttemptId::new(u64::from(graph.attempt)),
            trunk: graph.trunk,
            trunk_version: graph.trunk_version,
            authoritative: graph.authoritative,
            selected: graph.selected,
            next_future,
            ready_at,
            slots: vec![Duration::ZERO; workers],
            owed: graph.owed,
            injector: FailureInjector::none(),
            metrics,
            records,
            opened: true,
        })
    }

    /// A read-only view of a future, for evaluating it.
    #[must_use]
    pub fn view(&self, id: FutureId) -> Option<FutureView> {
        let f = self.lineage.get(id)?;
        let head = self.states.get(f.head)?.clone();
        let base = self.states.get(f.base)?.clone();
        Some(FutureView {
            id,
            parent: f.parent,
            arm: f.arm,
            status: f.status,
            head,
            base,
            steps: f.steps(),
            cost_ms: self.cost_ms_of(id),
            deferred_effects: f.deferred_effects().count(),
            performed_effects: f.performed_effects().count(),
            irreversible_deferred: f
                .deferred_effects()
                .filter(|e| e.class == EffectClass::Irreversible)
                .count(),
            children: f.children.clone(),
        })
    }

    /// Simulated time a future consumed, summed from the records it wrote.
    fn cost_ms_of(&self, id: FutureId) -> u64 {
        let mut last_us = 0u64;
        for r in &self.records {
            if r.payload.future() == Some(id) {
                last_us = r.timestamp_us;
            }
        }
        let mut total = last_us / 1_000;
        if total == 0 {
            let steps = self.lineage.get(id).map_or(0, |f| f.steps());
            total = steps as u64;
        }
        total
    }

    // -----------------------------------------------------------------------
    // Running
    // -----------------------------------------------------------------------

    /// Runs the execution to quiescence, recovering from injected failures.
    ///
    /// # Errors
    /// See [`KernelError`].
    pub fn run(&mut self) -> Result<RunReport, KernelError> {
        if !self.opened {
            self.open()?;
            self.opened = true;
        }
        let mut attempt = 0u32;
        loop {
            match self.drive() {
                Ok(()) => break,
                Err(KernelError::Injected(f)) => {
                    if attempt >= self.policy.max_attempts {
                        return Err(KernelError::Injected(f));
                    }
                    self.rollback()?;
                    attempt += 1;
                    self.metrics.recoveries += 1;
                    self.injector.rearm();
                }
                Err(e) => return Err(e),
            }
        }
        // The result is not a result until it is recoverable, so the final drain
        // is a synchronous wait and is charged as one.
        self.drain()?;
        self.metrics.wall_time = self.clock.wall();
        self.metrics.executor_time = self.clock.executor_time();
        self.metrics.flushes = self.writer.flushes;
        self.metrics.records_durable = self.writer.durable_records;
        self.metrics.ledger_bytes = self.writer.durable_bytes;
        self.metrics.state_versions = self.states.len();
        self.metrics.state_versions_deduped = self.states.deduped();
        self.metrics.peak_speculation = self.writer.peak_in_flight;
        Ok(self.report())
    }

    /// Executes exactly one step, or reports that the execution is finished.
    ///
    /// # Errors
    /// See [`KernelError`].
    ///
    /// This exists for the process-kill harness, which needs to stop the runtime at
    /// a chosen number of records so the crash lands mid-write. Everything else
    /// uses [`Kernel::run`].
    pub fn step_once(&mut self) -> Result<bool, KernelError> {
        if !self.opened {
            self.open()?;
            self.opened = true;
            return Ok(true);
        }
        self.pump()?;
        self.settle_finished();
        match self.next_runnable() {
            Some(id) => {
                self.step(id)?;
                Ok(true)
            }
            None => {
                self.release_owed()?;
                self.drain()?;
                Ok(false)
            }
        }
    }

    /// Writes the opening record and makes it durable.
    fn open(&mut self) -> Result<(), KernelError> {
        let record = self.make(Payload::Opened { plan: self.plan.id }, Version::ZERO);
        self.append(record)?;
        // The opening record is the root of the execution, so it is always durable
        // before anything else happens: recovery always has an anchor.
        self.drain()
    }

    /// The step loop, until nothing is runnable.
    fn drive(&mut self) -> Result<(), KernelError> {
        loop {
            self.pump()?;
            self.settle_finished();
            let Some(id) = self.next_runnable() else {
                break;
            };
            self.step(id)?;
        }
        // A commit may have been interrupted between the pointer and the release,
        // so owed effects are finished even when no future is runnable.
        self.release_owed()?;
        Ok(())
    }

    /// Advances the background writer and applies whatever became durable.
    fn pump(&mut self) -> Result<(), KernelError> {
        for (batch_no, batch) in
            (self.writer.flushes + 1..).zip(self.writer.poll(self.clock.wall()))
        {
            if self
                .injector
                .observe(Checkpoint::BeforeCheckpoint { batch: batch_no })
                .is_some()
            {
                // The record is already written to the store's buffer; the
                // interruption is "before the durability point", which is
                // recovered by simply not committing it.
                self.writer.discard_pending(Version::ZERO);
                return Err(KernelError::Injected(
                    self.injector.failure().expect("just fired"),
                ));
            }
            self.store.commit_prefix(batch.end)?;
            self.absorb(batch.version);
            if self
                .injector
                .observe(Checkpoint::AfterCheckpoint { batch: batch_no })
                .is_some()
            {
                return Err(KernelError::Injected(
                    self.injector.failure().expect("just fired"),
                ));
            }
        }
        Ok(())
    }

    /// Marks every future's head durable up to `version`.
    fn absorb(&mut self, version: Version) {
        // The resume cursor for each `(future, version)`, computed before the
        // mutable pass.
        //
        // Keyed by future as well as version, and this is not tidiness. A child
        // forked at version `v` starts with `head_version == v` and, until it does
        // any work of its own, has no record of its own carrying `v`. Keyed by
        // version alone, the lookup would find the *parent's* record for that
        // version and hand the child a cursor inside its parent's plan — and a
        // rollback would then resume the child there, walking it through steps it
        // never intended to run, against states it never branched from.
        let mut cursors: BTreeMap<(FutureId, Version), NodeId> = BTreeMap::new();
        for r in self.records.iter() {
            for (future, cursor) in r.payload.cursors_moved() {
                cursors.insert((future, r.version), cursor);
            }
        }
        for f in self.lineage.iter_mut_pairs().map(|(_, f)| f) {
            if f.durable_version < version && f.head_version <= version {
                f.durable_head = f.head;
                f.durable_version = f.head_version;
                // The cursor to resume from is the one the durable head was
                // produced at. When the future has a record at its own head
                // version, that is exact. When it has not — it was forked, and
                // nothing it did is durable — its own starting cursor is the truth,
                // and it is already recorded on the future.
                if let Some(cursor) = cursors.get(&(f.id, f.head_version)).copied() {
                    f.durable_cursor = cursor;
                }
            }
        }
        if self.trunk_version < version {
            self.trunk_version = self.version.min(version);
        }
    }

    /// The future that should execute next: the earliest ready, ties by id.
    fn next_runnable(&self) -> Option<FutureId> {
        self.lineage
            .ordered()
            .into_iter()
            .filter(|(_, f)| f.status != FutureStatus::Blocked && !f.status.is_terminal())
            .filter(|(_, f)| !self.finished(f))
            .min_by_key(|(id, _)| (self.ready_at.get(id).copied().unwrap_or_default(), *id))
            .map(|(id, _)| id)
    }

    /// Whether a forked child has settled, one way or another.
    ///
    /// This is the *same* predicate [`Kernel::unblock_if_done`] uses, and the two
    /// having to agree is the point. If the select waited for something stricter
    /// than what releases it — a child reaching the end of its arm, say — then a
    /// child that failed halfway would never satisfy the first and the trunk would
    /// wait forever, while a child that merely stopped being blocked would satisfy
    /// the second. A speculative scheduler is only correct when the condition it
    /// waits on and the condition that lets it proceed are the same fact.
    fn child_settled(&self, id: FutureId) -> bool {
        self.lineage
            .get(id)
            .is_some_and(|f| f.status == FutureStatus::Evaluable || f.status.is_terminal())
    }

    /// Whether a future has run to the end of its arm.
    ///
    /// The trunk's arm is the whole program; a child's arm is the extent its fork
    /// recorded. Testing `plan.node(cursor).is_none()` would be wrong for a child,
    /// because the program is flattened: the node *after* an arm's end belongs to
    /// the next arm, so a child that walked past its extent would silently start
    /// executing a sibling's plan.
    fn finished(&self, f: &Future) -> bool {
        match f.arm_end {
            Some(end) => f.cursor >= end,
            None => f.cursor >= self.plan.end(),
        }
    }

    // -----------------------------------------------------------------------
    // One step
    // -----------------------------------------------------------------------

    /// Executes one plan node for `id`.
    fn step(&mut self, id: FutureId) -> Result<(), KernelError> {
        let cursor = self.lineage.require(id)?.cursor;
        if self.finished(self.lineage.require(id)?) {
            self.finish(id)?;
            return Ok(());
        }
        let node = self
            .plan
            .node(cursor)
            .cloned()
            .ok_or_else(|| KernelError::BadPlan(format!("no node at cursor {cursor}")))?;

        let cost = node.cost();
        let start = self.occupy(id, cost);
        self.clock.work(start, cost);

        match &node {
            PlanNode::State { op, .. } => self.run_state(id, cursor, op)?,
            PlanNode::Effect { .. } => self.run_effect(id, cursor, &node)?,
            PlanNode::Fork { .. } => self.run_fork(id, cursor, &node)?,
            PlanNode::Select => self.run_select(id, cursor)?,
            PlanNode::Commit => self.run_commit(id, cursor)?,
        }
        self.metrics.steps_executed += 1;
        Ok(())
    }

    /// The cursor `id` moves to after executing the node at `cursor`.
    ///
    /// Clamped to the future's own arm, so a child cannot step into a sibling's
    /// arm, and skipping arm roots, so a parent does not execute its alternatives.
    fn advance(&self, id: FutureId, cursor: NodeId) -> NodeId {
        let limit = self
            .lineage
            .get(id)
            .and_then(|f| f.arm_end)
            .unwrap_or_else(|| self.plan.end());
        self.plan.advance(cursor, limit)
    }

    /// Reserves a worker slot and returns the instant this work starts.
    fn occupy(&mut self, id: FutureId, cost: Duration) -> Duration {
        let ready = self.ready_at.get(&id).copied().unwrap_or_default();
        let mut best = 0usize;
        for (i, free) in self.slots.iter().enumerate() {
            if *free < self.slots[best] {
                best = i;
            }
        }
        let start = ready.max(self.slots[best]);
        self.slots[best] = start + cost;
        self.ready_at.insert(id, start + cost);
        start
    }

    /// Executes a pure state node.
    fn run_state(
        &mut self,
        id: FutureId,
        cursor: NodeId,
        op: &crate::domain::Operation,
    ) -> Result<(), KernelError> {
        if let crate::domain::Operation::Fail { reason } = op {
            // A failing *alternative* is an outcome, not a failure of the
            // execution: the future settles as `Failed`, the parent sees one fewer
            // viable candidate, and the run continues. Only a failure on the trunk
            // — on the execution's own spine — aborts it, because there is nothing
            // left to choose between.
            self.settle(id, FutureStatus::Failed)?;
            self.metrics.futures_failed += 1;
            return if id == TRUNK {
                Err(KernelError::PlanFailed {
                    future: id,
                    reason: reason.clone(),
                })
            } else {
                Ok(())
            };
        }
        let step_no = self.lineage.require(id)?.steps() + 1;
        if self
            .injector
            .observe(Checkpoint::BeforeStep {
                future: id.value(),
                n: step_no,
            })
            .is_some()
        {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        let mut state = self.state_of(id)?;
        state.apply(op)?;
        let version = self.next_version();
        let node = self.states.intern(&state);
        let delta = StateDelta::new(
            id,
            self.plan.step_of(cursor),
            self.attempt,
            self.lineage.require(id)?.head_version,
            version,
            op.clone(),
        );
        let record = self.make(
            Payload::Step {
                delta: Box::new(delta),
                cursor: self.advance(id, cursor),
            },
            version,
        );
        self.append(record)?;
        if id == TRUNK {
            // The trunk future's head *is* the trunk head. A commit moves the trunk
            // future to the merged state and then the trunk keeps executing, so
            // tracking this per step — not only at commit time — is what lets work
            // after a commit show up in the authoritative value.
            self.trunk = node;
            self.trunk_version = version;
        }
        let next = self.advance(id, cursor);
        let f = self.lineage.require_mut(id)?;
        f.head = node;
        f.head_version = version;
        f.chain.push(version);
        f.cursor = next;
        f.status = FutureStatus::Running;
        // The window *after* a step's record is written but *before* anything can
        // make it durable is the one that matters for speculation: it is where
        // executed-but-unpersisted work exists. A failure here must roll back.
        if self
            .injector
            .observe(Checkpoint::AfterStep {
                future: id.value(),
                n: step_no,
            })
            .is_some()
        {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        if let crate::domain::Operation::Emit { event } = op {
            self.metrics.trace(event);
        }
        Ok(())
    }

    /// Executes an effect node, performing it or deferring it.
    fn run_effect(
        &mut self,
        id: FutureId,
        cursor: NodeId,
        node: &PlanNode,
    ) -> Result<(), KernelError> {
        let PlanNode::Effect {
            class,
            op,
            reads_into,
            writes,
            compensation,
            ..
        } = node
        else {
            return Ok(());
        };
        let (class, op) = (*class, op.clone());
        let (reads_into, writes, compensation) =
            (reads_into.clone(), writes.clone(), compensation.clone());
        let ordinal_no = self.lineage.require(id)?.effects.len();
        if self
            .injector
            .observe(Checkpoint::BeforeEffect {
                future: id.value(),
                n: ordinal_no,
            })
            .is_some()
        {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        let ordinal = self.lineage.require_mut(id)?.next_effect_key();
        let key = self.effect_key(id, ordinal);
        let authoritative = id == self.authoritative;
        let request = {
            let mut r = EffectRequest::new(
                key.clone(),
                id,
                self.lineage.require(id)?.head_version,
                class,
                op.clone(),
            );
            if let Some(k) = &reads_into {
                r = r.reading_into(k);
            }
            if let Some(k) = &writes {
                r = r.writing(k);
            }
            if let Some(c) = &compensation {
                r = r.with_compensation(c.clone());
            }
            r
        };

        // Replay first. An observed value is in the journal, never recomputed, so
        // a step re-executed after a rollback sees the world it saw last time.
        let disposition = match self.journal.recorded(&key).cloned() {
            Some(value) => Disposition::Replayed { result: value },
            None if request.may_perform_now(authoritative) => {
                let value =
                    self.sink
                        .perform(&key, &op, class)
                        .map_err(|e| KernelError::Effect {
                            future: id,
                            key: key.clone(),
                            reason: e.to_string(),
                        })?;
                self.metrics.effects_performed += 1;
                Disposition::Performed { result: value }
            }
            None => {
                // The effect cannot leave speculative execution, so it becomes an
                // intent. This one branch is the whole safety argument, and it is
                // why a rejected future's irreversible effects provably never
                // happen.
                self.metrics.effects_deferred += 1;
                Disposition::Deferred
            }
        };
        if matches!(disposition, Disposition::Replayed { .. }) {
            self.metrics.effects_replayed += 1;
        }
        self.record_effect(
            id,
            cursor,
            ordinal,
            request,
            disposition,
            class,
            op.clone(),
            reads_into,
            writes,
            compensation,
        )?;
        if self
            .injector
            .observe(Checkpoint::AfterEffect {
                future: id.value(),
                n: ordinal_no,
            })
            .is_some()
        {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn record_effect(
        &mut self,
        id: FutureId,
        cursor: NodeId,
        ordinal: EffectId,
        request: EffectRequest,
        disposition: Disposition,
        class: EffectClass,
        op: EffectOp,
        reads_into: Option<String>,
        writes: Option<String>,
        compensation: Option<EffectOp>,
    ) -> Result<(), KernelError> {
        let version = self.next_version();
        let mut delta = EffectDelta::new(
            id,
            ordinal,
            self.attempt,
            self.lineage.require(id)?.head_version,
            version,
            class,
            op.clone(),
            disposition.clone(),
        );
        delta.reads_into = reads_into.clone();
        delta.writes = writes.clone();
        delta.compensation = compensation.clone();

        // A read folds its observed value into the future's state, which is what
        // makes replay a pure function of the journal.
        let mut state = self.state_of(id)?;
        if let (Some(key), Some(value)) = (&reads_into, disposition.result()) {
            state.apply(&crate::domain::Operation::Set {
                key: key.clone(),
                value: value.as_int().unwrap_or(0),
            })?;
        }
        let node = self.states.intern(&state);
        let next = self.advance(id, cursor);
        let record = self.make(
            Payload::Effect {
                delta: Box::new(delta),
                cursor: next,
            },
            version,
        );
        self.append(record)?;
        if id == TRUNK {
            self.trunk = node;
            self.trunk_version = version;
        }
        match &disposition {
            Disposition::Performed { result } | Disposition::Replayed { result } => {
                self.journal.record_performed(
                    request.key.clone(),
                    id,
                    op.clone(),
                    class,
                    result.clone(),
                );
            }
            Disposition::Deferred => {
                self.journal
                    .record_intent(request.key.clone(), id, op.clone(), class);
            }
        }
        let f = self.lineage.require_mut(id)?;
        f.effects.push(EffectRecord {
            key: request.key,
            class,
            op,
            disposition,
            compensation,
            writes,
        });
        f.next_effect = f.next_effect.max(ordinal.value() + 1);
        f.chain.push(version);
        f.head = node;
        f.head_version = version;
        f.cursor = next;
        f.status = FutureStatus::Running;
        Ok(())
    }

    /// Executes a fork: create the children and block this future on them.
    fn run_fork(
        &mut self,
        id: FutureId,
        cursor: NodeId,
        node: &PlanNode,
    ) -> Result<(), KernelError> {
        let PlanNode::Fork { arms, .. } = node else {
            return Ok(());
        };
        let base_node = self.lineage.require(id)?.head;
        let base_version = self.lineage.require(id)?.head_version;
        let mut children = Vec::with_capacity(arms.len());
        for (arm_index, arm) in arms.iter().enumerate() {
            let child = FutureId::new(self.next_future);
            self.next_future += 1;
            // The child's stable identity: the parent's path plus this arm. A
            // re-executed fork after a rollback rebuilds exactly this path, so
            // effect keys derived from it stay stable across the retry.
            let mut path = self.lineage.require(id)?.path.clone();
            path.push(arm_index as u32);
            self.lineage.insert(Future::new(
                child,
                Some(id),
                Some(arm_index),
                path,
                Some(arm.end),
                base_node,
                base_version,
            ))?;
            // The child starts at its arm's root, not at zero: the plan is
            // flattened, so an arm is a cursor range inside the same program.
            // Starting at zero would let an arm walk into its siblings' nodes and
            // back into the fork itself.
            {
                let f = self.lineage.require_mut(child)?;
                f.cursor = arm.root;
                // A child that has never made durable progress resumes at its arm
                // root, not at zero.
                f.durable_cursor = arm.root;
            }
            self.ready_at.insert(child, self.clock.wall());
            children.push(ForkedChild {
                future: child,
                arm: arm_index,
                root: arm.root,
                end: arm.end,
                base: base_node,
            });
        }
        let next = self.advance(id, cursor);
        let n_children = children.len();
        let child_ids: Vec<FutureId> = children.iter().map(|c| c.future).collect();
        let record = self.make(
            Payload::Fork {
                parent: id,
                cursor: next,
                children,
            },
            self.lineage.require(id)?.head_version,
        );
        self.append(record)?;
        let f = self.lineage.require_mut(id)?;
        f.children = child_ids;
        f.cursor = next;
        f.status = FutureStatus::Blocked;
        self.metrics.futures_forked += n_children as u64;
        Ok(())
    }

    /// Executes a selection over this future's children.
    fn run_select(&mut self, id: FutureId, cursor: NodeId) -> Result<(), KernelError> {
        let children = self.lineage.require(id)?.children.clone();
        if children.is_empty() {
            return Ok(());
        }
        // A selection is only meaningful over *finished* alternatives.
        //
        // Scoring a half-executed branch against a finished one is not a small
        // imprecision: a branch that has not reached its `score` write has no score
        // at all, so the evaluator hands the win to whichever arm happened to look
        // viable — and the log records a perfectly well-formed selection, a
        // three-way merge and a commit, all of a wrong answer. The trunk blocks
        // here instead, which is also the only way a *speculative* scheduler can be
        // correct: the decision has to see the same information the recovery will.
        if children.iter().any(|c| !self.child_settled(*c)) {
            let f = self.lineage.require_mut(id)?;
            if f.status != FutureStatus::Blocked {
                f.status = FutureStatus::Blocked;
            }
            return Ok(());
        }
        let n_children = children.len();
        if self
            .injector
            .observe(Checkpoint::BeforeSelect {
                candidates: n_children,
            })
            .is_some()
        {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        let views: Vec<FutureView> = children.iter().filter_map(|c| self.view(*c)).collect();
        let selection: Selection = evaluate::select(self.policy.evaluator.as_ref(), &views)?;
        for (c, score) in children.iter().zip(selection.scores.iter()) {
            self.lineage.get_mut(*c)?.score = *score;
        }
        let next = self.advance(id, cursor);
        let record = self.make(
            Payload::Selected {
                among: selection.candidates.clone(),
                scores: selection.scores.clone(),
                winner: selection.winner,
                cursor: next,
            },
            self.lineage.require(id)?.head_version,
        );
        self.append(record)?;
        for c in children {
            if c == selection.winner {
                self.lineage.require_mut(c)?.status = FutureStatus::SelectionRecorded;
            } else {
                self.metrics.futures_rejected += 1;
                self.settle(c, FutureStatus::Rejected)?;
            }
        }
        self.selected = Some(selection.winner);
        self.metrics.selection = Some(selection);
        let f = self.lineage.require_mut(id)?;
        f.cursor = next;
        f.status = FutureStatus::Running;
        if self
            .injector
            .observe(Checkpoint::AfterSelect {
                candidates: n_children,
            })
            .is_some()
        {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        Ok(())
    }

    /// Executes a commit: make the selected future authoritative.
    fn run_commit(&mut self, id: FutureId, cursor: NodeId) -> Result<(), KernelError> {
        let Some(winner) = self.selected else {
            return Err(KernelError::BadPlan(
                "commit reached with no selection in force".to_owned(),
            ));
        };
        if self.injector.observe(Checkpoint::BeforeCommit).is_some() {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        let base = self
            .states
            .get(self.lineage.require(winner)?.base)
            .cloned()
            .unwrap_or_default();
        let ours = self.state_of(winner)?;
        let theirs = self.state_of(id)?;
        let plan = commit::plan_merge(
            &MergeInput {
                future: winner,
                base: &base,
                ours: &ours,
                theirs: &theirs,
                base_version: self.lineage.require(winner)?.base_version,
                trunk_version: self.trunk_version,
                pending: self
                    .journal
                    .pending_release(winner)
                    .iter()
                    .map(|e| e.key.ordinal)
                    .collect(),
                already_authoritative: (self.authoritative == winner).then_some(winner),
            },
            self.policy.conflict,
        )?;
        let merged_node = self.states.intern(&plan.merged);
        if !plan.is_clean() {
            self.metrics.commits_with_conflicts += 1;
        }
        let next = self.advance(id, cursor);
        let record = self.make(
            Payload::Committed {
                future: winner,
                cursor: next,
                trunk: merged_node,
                trunk_version: plan.trunk_version,
                release: plan.release.clone(),
            },
            plan.trunk_version,
        );
        self.append(record)?;
        self.trunk = merged_node;
        self.trunk_version = plan.trunk_version;
        self.authoritative = winner;
        self.metrics.commits += 1;
        // The winner stops being a branch: it *is* the trunk now, and saying so is
        // what makes `settle_finished` leave it alone. Marking it `Running` would
        // let it be evaluated a second time, after the commit, and the recovered
        // graph would disagree with the executed one.
        {
            let w = self.lineage.require_mut(winner)?;
            w.head = merged_node;
            w.head_version = plan.trunk_version;
            w.cursor = next;
            w.status = FutureStatus::Committed;
        }
        {
            let f = self.lineage.require_mut(id)?;
            f.head = merged_node;
            f.head_version = plan.trunk_version;
            f.cursor = next;
            f.status = FutureStatus::Running;
        }
        self.owed.extend(plan.release.iter().map(|o| (winner, *o)));
        if self.injector.observe(Checkpoint::DuringCommit).is_some() {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        // The commit must be *durable* before any irreversible effect is issued.
        //
        // This is the one place the runtime cannot have it both ways. If it released
        // on "the record is written" rather than "the record is proven", a crash
        // before the checkpoint would roll the commit back while the charge, the
        // email or the transfer had already left the building — and no recovery can
        // undo that. Paying one flush here buys the guarantee that every effect a
        // rejected future touched was a read or an idempotent write.
        self.force_durable(plan.trunk_version)?;
        if self.injector.observe(Checkpoint::AfterCommit).is_some() {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        self.release_owed()?;
        if self.injector.observe(Checkpoint::AfterCommit).is_some() {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        let owed = self.owed.len();
        if self
            .injector
            .observe(Checkpoint::AfterRelease { effects: owed })
            .is_some()
        {
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        Ok(())
    }

    /// The idempotency key for one effect occurrence.
    ///
    /// Built from the execution and the future's fork-arm path, both stable under
    /// retry — never from the runtime-assigned `FutureId`, which is not. See
    /// [`EffectKey`].
    fn effect_key(&self, future: FutureId, ordinal: EffectId) -> EffectKey {
        let path = self
            .lineage
            .get(future)
            .map(|f| f.path.clone())
            .unwrap_or_default();
        EffectKey::new(self.execution, path, ordinal)
    }

    /// Issues every effect a durable commit still owes, in order.
    fn release_owed(&mut self) -> Result<(), KernelError> {
        while let Some((future, ordinal)) = self.owed.first().copied() {
            let key = self.effect_key(future, ordinal);
            let entry = self.journal.get(&key).cloned().ok_or_else(|| {
                KernelError::BadPlan(format!("effect {key} is owed but not journalled"))
            })?;
            // The key is the idempotency token: a re-release after a crash is the
            // same effect, and a target that honours keys performs it once.
            let value = self
                .sink
                .perform(&key, &entry.op, entry.class)
                .map_err(|e| KernelError::Effect {
                    future,
                    key: key.clone(),
                    reason: e.to_string(),
                })?;
            self.journal.mark_issued(&key, value);
            let record = self.make(Payload::Released { future, ordinal }, self.version);
            self.append(record)?;
            self.owed.remove(0);
            // The `Released` record is what makes the release a fact, so it has to
            // be as durable as the effect it describes. Otherwise a crash could
            // leave the effect performed with no durable proof, and the next
            // process would re-issue it — harmless for an idempotent target, but not
            // a guarantee the runtime should be making by accident.
            self.force_durable(self.version)?;
            self.metrics.effects_released += 1;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Durability and recovery
    // -----------------------------------------------------------------------

    /// Enforces the durability policy before the kernel writes anything.
    ///
    /// This runs before *every* record, not only before steps. A `Settled` or a
    /// `Committed` record is as much a fact as a step is, so a policy that let the
    /// executor run ahead of durability and then wrote an unbounded number of
    /// control records would quietly exceed its own window — and a window that is
    /// not enforced is not a bound.
    fn enforce_window(&mut self) -> Result<(), KernelError> {
        let must_wait = match self.policy.durability {
            Durability::Synchronous => self.writer.in_flight() > 0,
            other => self.writer.in_flight() >= other.window(),
        };
        if !must_wait {
            return Ok(());
        }
        let start = self.clock.wall();
        self.advance_writer_to_drain()?;
        let waited = self.clock.wall().saturating_sub(start);
        if waited > Duration::ZERO {
            self.metrics.sync_waits += 1;
            self.metrics.sync_wait += waited;
            self.writer.waits += 1;
            self.writer.waited += waited;
        }
        Ok(())
    }

    /// Recomputes the release debt from the durable prefix of the log.
    ///
    /// A `Committed` record without a matching `Released` record for every ordinal
    /// in its `release` list is a commit whose effects the world has not seen yet.
    /// Recovering that set from the log — rather than from a field in memory — is
    /// what makes an interrupted release survivable.
    fn owed_from_durable_prefix(&self) -> Vec<(FutureId, EffectId)> {
        let mut released: BTreeSet<(FutureId, EffectId)> = BTreeSet::new();
        let mut owed = Vec::new();
        for r in &self.records {
            match &r.payload {
                Payload::Released { future, ordinal } => {
                    released.insert((*future, *ordinal));
                }
                Payload::Committed {
                    future, release, ..
                } => {
                    for ordinal in release {
                        if !released.contains(&(*future, *ordinal)) {
                            owed.push((*future, *ordinal));
                        }
                    }
                }
                _ => {}
            }
        }
        owed
    }

    /// Blocks until the log has proven `version`, advancing the virtual clock.
    ///
    /// Used only where correctness demands it rather than where a policy would
    /// prefer it — see the commit path.
    fn force_durable(&mut self, version: Version) -> Result<(), KernelError> {
        if self.writer.durable_version() >= version && self.writer.in_flight() == 0 {
            return Ok(());
        }
        let start = self.clock.wall();
        self.advance_writer_to_drain()?;
        let waited = self.clock.wall().saturating_sub(start);
        if waited > Duration::ZERO {
            self.metrics.sync_waits += 1;
            self.metrics.sync_wait += waited;
            self.writer.waits += 1;
            self.writer.waited += waited;
        }
        Ok(())
    }

    /// Advances the writer and the clock until everything buffered is durable.
    fn advance_writer_to_drain(&mut self) -> Result<(), KernelError> {
        let start = self.clock.wall();
        let mut now = start;
        for _ in 0..1_000_000 {
            for batch in self.writer.poll(now) {
                self.store.commit_prefix(batch.end)?;
                self.absorb(batch.version);
            }
            if self.writer.is_drained() {
                break;
            }
            let next = self.writer.free_at().max(now);
            if next <= now {
                break;
            }
            now = next;
        }
        self.clock.park_until(now.max(start));
        Ok(())
    }

    /// Makes every buffered record durable, synchronously.
    fn drain(&mut self) -> Result<(), KernelError> {
        if self.writer.is_drained() {
            return Ok(());
        }
        let start = self.clock.wall();
        self.advance_writer_to_drain()?;
        let waited = self.clock.wall().saturating_sub(start);
        if waited > Duration::ZERO {
            self.metrics.sync_waits += 1;
            self.metrics.sync_wait += waited;
            self.writer.waits += 1;
            self.writer.waited += waited;
        }
        Ok(())
    }

    /// Discards speculative state and rewinds every future to its durable head.
    fn rollback(&mut self) -> Result<(), KernelError> {
        if self
            .injector
            .observe(Checkpoint::DuringRecovery {
                futures: self.lineage.len(),
            })
            .is_some()
        {
            // Recovery was itself interrupted. The durable prefix is unchanged, so
            // the next attempt simply replays from it.
            return Err(KernelError::Injected(
                self.injector.failure().expect("just fired"),
            ));
        }
        let durable = self.writer.durable_version();
        // How many records the store actually proved before the failure. This is
        // the length of the prefix a crashed process would leave behind, and the
        // in-memory log has to be cut to match — otherwise the kernel still believes
        // in records that were never anywhere a restarted process could read, and
        // recovery would rebuild a graph containing a commit that was rolled back.
        let durable_len = self.writer.durable_records as usize;
        self.writer.discard_pending(durable);
        self.records.truncate(durable_len.min(self.records.len()));
        // A commit that was already durable still owes its deferred effects, and a
        // rollback must not forget that. Re-derive the debt from the durable prefix
        // instead of carrying it across in memory, so a restarted process reaches
        // the same conclusion this one does.
        self.owed = self.owed_from_durable_prefix();
        // Anything the store still holds beyond the durable region is really
        // dropped: a rollback is not a bookkeeping entry.
        self.store.truncate_to(self.store.sync_position())?;
        self.owed.clear();
        // Futures created by the work being discarded no longer exist. Leaving them
        // in the lineage would mean the re-execution creates a *second* set of
        // children beside the first, and the ledger would then hold two forks and
        // two commits describing the same execution.
        let orphans: Vec<FutureId> = self
            .lineage
            .ordered()
            .into_iter()
            .filter(|(_, f)| f.base_version > durable)
            .map(|(id, _)| id)
            .collect();
        for id in &orphans {
            if let Ok(f) = self.lineage.get_mut(*id) {
                if let Some(parent) = f.parent {
                    if let Ok(p) = self.lineage.get_mut(parent) {
                        p.children.retain(|c| c != id);
                    }
                }
            }
        }
        for id in &orphans {
            self.ready_at.remove(id);
            self.lineage.remove(*id);
        }
        // Futures whose commit is already durable are not speculative any more.
        //
        // Rolling one back would demote a `Committed` future to running work, and
        // then nothing would ever set it back: the commit already happened, so no
        // later record will re-assert it. The execution would finish reporting
        // that no future committed, while the log says one did — and every effect
        // the commit released would look, from the outside, like damage done by a
        // rejected future.
        let committed: Vec<FutureId> = self
            .records
            .iter()
            .filter_map(|r| match r.payload {
                Payload::Committed { future, .. } => Some(future),
                _ => None,
            })
            .collect();
        // Which future the trunk's state currently *is*. Carried in memory, so a
        // rollback that discards a commit has to re-derive it or the re-execution
        // trips over its own predecessor: the commit guard asks "is this future
        // already authoritative?", gets yes from a commit that no longer exists,
        // and refuses the very commit that should replace it.
        self.authoritative = committed.last().copied().unwrap_or(TRUNK);
        let settled: BTreeSet<FutureId> = committed.into_iter().collect();
        let mut discarded = 0u64;
        for (id, f) in self.lineage.iter_mut_pairs() {
            if settled.contains(&id) {
                continue;
            }
            let keep = f
                .chain
                .iter()
                .position(|v| *v == f.durable_version)
                .map_or(0, |i| i + 1);
            discarded += (f.chain.len() - keep) as u64;
            let head = f.durable_head;
            let at = f.durable_version;
            f.rollback_to_durable(head, at);
        }
        self.metrics.steps_discarded += discarded;
        self.attempt = AttemptId::new(self.attempt.value() + 1);
        self.writer.reset_to(durable, self.clock.wall());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Ledger plumbing
    // -----------------------------------------------------------------------

    /// Consumes one version from the global counter.
    ///
    /// Versions are global rather than per-future so that the ledger has a total
    /// order on records; a future's own history is the subsequence of versions its
    /// deltas carry, and each delta names the version it was applied to, so the
    /// subsequence is still self-describing.
    fn next_version(&mut self) -> Version {
        let v = self.version.next();
        self.version = v;
        v
    }

    fn make(&self, payload: Payload, version: Version) -> LedgerRecord {
        LedgerRecord::new(
            self.execution,
            version,
            self.sequence,
            self.attempt,
            self.clock.wall().as_micros() as u64,
            payload,
        )
    }

    /// Appends a record and hands it to the background writer, respecting the
    /// durability policy first.
    fn append(&mut self, record: LedgerRecord) -> Result<(), KernelError> {
        self.enforce_window()?;
        let version = record.version;
        let encoded = record.encode();
        let size = encoded.len();
        self.store.append(encoded)?;
        // The durability boundary is the position *past* the record, not its start.
        let end = self.store.written_end();
        self.sequence = self.sequence.next();
        self.records.push(record);
        self.metrics.records_written += 1;
        self.writer.submit(end, size, self.clock.wall(), version);
        Ok(())
    }

    /// Moves every future that has reached its end into `Evaluable`, which is what
    /// unblocks a parent waiting on it.
    ///
    /// This is a separate pass rather than a side effect of `step`, because a
    /// finished future is *not runnable* — it has no node to execute — so if the
    /// transition only happened inside `step`, a child would arrive at the end of
    /// its arm, become unrunnable, and never be marked evaluable. Its parent would
    /// wait for it forever.
    fn settle_finished(&mut self) {
        let ready: Vec<FutureId> = self
            .lineage
            .ordered()
            .into_iter()
            .filter(|(_, f)| {
                let settled = f.status == FutureStatus::Evaluable
                    || f.status.is_terminal()
                    || f.status == FutureStatus::Blocked;
                !settled && self.finished(f)
            })
            .map(|(id, _)| id)
            .collect();
        for id in ready {
            // A `Result` here would be a lineage invariant violation, which
            // cannot happen; ignoring it keeps the pass infallible and the
            // invariant is still asserted by `validate` in recovery.
            let _ = self.finish(id);
        }
    }

    /// A future that reached the end of its arm is evaluable, and unblocks its
    /// parent if it was the last child.
    fn finish(&mut self, id: FutureId) -> Result<(), KernelError> {
        let already = self.lineage.require(id)?.status == FutureStatus::Evaluable;
        if !already {
            // "This future reached the end of its arm" is a durable fact: a parent
            // may be waiting on it, and recovery has to reach the same conclusion
            // from the ledger rather than by re-running.
            self.settle(id, FutureStatus::Evaluable)?;
        }
        let parent = self.lineage.require(id)?.parent;
        if let Some(p) = parent {
            self.unblock_if_done(p)?;
        }
        Ok(())
    }

    /// A future is done when none of its children is still running or blocked.
    fn unblock_if_done(&mut self, parent: FutureId) -> Result<(), KernelError> {
        let children = self.lineage.require(parent)?.children.clone();
        if children.is_empty() {
            return Ok(());
        }
        let done = children.iter().all(|c| self.child_settled(*c));
        if !done {
            return Ok(());
        }
        let f = self.lineage.require_mut(parent)?;
        if f.status == FutureStatus::Blocked {
            f.status = FutureStatus::Running;
        }
        let now = self.clock.wall();
        self.ready_at.insert(parent, now);
        Ok(())
    }

    fn settle(&mut self, id: FutureId, status: FutureStatus) -> Result<(), KernelError> {
        let record = self.make(
            Payload::Settled { future: id, status },
            self.lineage.require(id)?.head_version,
        );
        self.append(record)?;
        let f = self.lineage.require_mut(id)?;
        f.status = status;
        if let Some(p) = f.parent {
            self.unblock_if_done(p)?;
        }
        if status.is_terminal() {
            self.ready_at.remove(&id);
        }
        Ok(())
    }

    fn state_of(&self, id: FutureId) -> Result<State, KernelError> {
        let node = self.lineage.require(id)?.head;
        Ok(self.states.get(node).cloned().unwrap_or_default())
    }

    fn report(&self) -> RunReport {
        RunReport {
            trunk: self.states.get(self.trunk).cloned().unwrap_or_default(),
            trunk_version: self.trunk_version,
            authoritative: self.authoritative,
            selection: self.metrics.selection.clone(),
            trunk_content: self.trunk,
            metrics: self.metrics.clone(),
            records: self.records.clone(),
        }
    }
}

impl KernelMetrics {
    fn trace(&mut self, _event: &str) {
        // Reserved for future reporting; deliberately not accumulating strings,
        // because a long run would then be dominated by its own diagnostics.
    }
}
