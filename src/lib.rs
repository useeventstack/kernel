//! `useEventStack` — a durable execution kernel with first-class execution futures.
//!
//! # What this is
//!
//! A runtime that can represent **multiple alternative executions from a known
//! state**, run them in isolation, record their lineage and their effects, evaluate
//! them, select one, commit it so it becomes authoritative, and recover all of
//! that correctly after a crash at any point in the lifecycle.
//!
//! # The one idea it rests on
//!
//! An execution is a **DAG of futures over content-addressed state**, not a single
//! irreversible line.
//!
//! * a *future* is a durable alternative continuation: `(base state, cursor, delta
//!   chain)`;
//! * the state store is insert-only and content addressed, so forking copies eight
//!   bytes and **isolation is a property of the type system**, not a runtime check;
//! * every change to the trunk, the lineage or the effect journal goes through a
//!   ledger record first, so recovery is a *replay* rather than a reconstruction;
//! * the *effect model* is a total function of an effect's class, so an
//!   irreversible effect provably cannot escape speculative execution — there is
//!   no setting that lets it.
//!
//! # The four things worth reading
//!
//! | question | where |
//! |---|---|
//! | why is branching affordable? | [`domain::node`] |
//! | what may touch the world, and when? | [`domain::effect`] |
//! | what does "commit" actually promise? | [`kernel::commit`] |
//! | what is guaranteed after a crash? | [`kernel::recovery`] |
//!
//! # What is not claimed
//!
//! * **exactly-once external effects.** The runtime delivers at-least-once and
//!   deduplicates by an effect key it derives from the future's identity. That is
//!   effectively-once *if and only if* the target honours keys, and no runtime can
//!   make a third-party payment processor honour one.
//! * **distribut** execution. One process, one ledger, one writer. The ports in
//!   [`ports`] are where a provider boundary goes; there is no replicated log here.
//! * **novelty.** See `docs/research.md`, which argues the honest position: the
//!   primitives are all prior art, the composition is not something we found in a
//!   product, and that is a weaker claim than invention.
//! * **throughput on real hardware.** The benchmark runs on a virtual clock, which
//!   measures the *shape* of the cost rather than this machine's disk. See
//!   `docs/experiments.md`.
//!
//! # Status
//!
//! Research prototype. Single machine, simulated execution clock. See
//! `docs/architecture.md` for the design, `docs/invariants.md` for what is
//! enforced where and which test proves it, `docs/experiments.md` for the
//! numbers this implementation actually produced, and `docs/research.md` for the
//! state of the art.
//!
//! # A run
//!
//! ```
//! use ues::domain::effect::{EffectClass, EffectOp};
//! use ues::{ExecutionPolicy, HighestScore, Kernel, Operation, Plan, PlanId};
//!
//! // One alternative. Its base state is its parent's head, so it starts from
//! // exactly where the fork happened and cannot see its siblings.
//! let arm = |price: i64| {
//!     Plan::builder(PlanId::new(2), "arm")
//!         .effect(
//!             "quote",
//!             EffectClass::Read,
//!             EffectOp::new("quote", price),
//!             Some("quoted"),
//!             None,
//!         )
//!         .state(Operation::Set { key: "total".into(), value: price })
//!         .build()
//! };
//!
//! let plan = Plan::builder(PlanId::new(1), "quote")
//!     .state(Operation::Set { key: "total".into(), value: 0 })
//!     .fork("choose", vec![arm(10), arm(20), arm(15)])
//!     .select()
//!     .commit()
//!     .state(Operation::Add { key: "committed".into(), by: 1 })
//!     .build();
//!
//! // `durable()` alone picks the first viable future. Naming the evaluator is how
//! // a plan says *why* one alternative should win.
//! let policy = ExecutionPolicy::durable()
//!     .with_evaluator(Box::new(HighestScore::new("total")));
//! let mut kernel = Kernel::new(plan, policy)?;
//! let report = kernel.run()?;
//!
//! // Three alternatives ran, the evaluator picked the best, and it was merged
//! // into the trunk by a single atomic record.
//! assert_eq!(report.metrics.futures_forked, 3);
//! assert_eq!(report.trunk.get("total"), 20);
//! assert_eq!(report.trunk.get("committed"), 1);
//! # Ok::<(), ues::KernelError>(())
//! ```

pub mod benchmark;
pub mod cli;
pub mod domain;
pub mod kernel;
pub mod ledger;
pub mod ports;
pub mod simulation;
pub mod strategy;

pub use domain::{
    AttemptId, Cursor, Disposition, EffectClass, EffectKey, EffectOp, EffectSink, EffectValue,
    ExecutionId, FutureId, FutureStatus, Lineage, NodeId, Operation, Plan, PlanId, PlanNode,
    RecordingSink, State, StateNode, StateStore, StepId, Version, TRUNK,
};
pub use kernel::{
    ConflictPolicy, Durability, Evaluator, ExecutionPolicy, HighestScore, Kernel, KernelError,
    KernelMetrics, LowestScore, RunReport,
};
pub use ledger::EffectJournal;
pub use ledger::{FileStore, LedgerRecord, MemoryStore, Payload};
pub use ports::ScanReport;
pub use ports::{Clock, CostModel, DurableStore, StoreError, VirtualClock};
pub use simulation::{Checkpoint, FailureInjector, FailurePoint};
pub use strategy::{
    contract_events, Agreement, ContractEvent, ExecutionStrategy, ReplayEvidence, StrategyContext,
    StrategyError, StrategyRun,
};
