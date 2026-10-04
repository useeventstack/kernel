//! [`ExecutionStrategy`]: how a workload is executed, as a value.
//!
//! # What this is
//!
//! Four ways to run the same [`Plan`], chosen by whoever declared it, agreeing
//! on the answer. A strategy is a **total description of a kernel
//! configuration** — it picks a [`Durability`], an effect policy, and, for one
//! variant, a source to rebuild from. It never changes the plan, and it never
//! changes the evaluator.
//!
//! | variant | durability | effects | replays from |
//! |---|---|---|---|
//! | [`Deterministic`](ExecutionStrategy::Deterministic) | `Synchronous` | issued | — |
//! | [`Speculative`](ExecutionStrategy::Speculative) | `Async { window }` | issued | — |
//! | [`Replay`](ExecutionStrategy::Replay) | `Synchronous` | **suppressed** | a durable prefix |
//! | [`Observe`](ExecutionStrategy::Observe) | `Synchronous` | **suppressed** | — |
//!
//! # The property that justifies the type
//!
//! **For one plan, all four strategies produce the same durable record stream
//! and the same trunk.** That is [`StrategyRun::events`] and
//! [`StrategyRun::trunk_content`], and it is what
//! `tests/strategy_agreement.rs` asserts. Two strategies that disagreed about
//! the outcome would make the abstraction a lie, so the property is a named
//! test rather than a comment.
//!
//! What the strategies do *not* share is their effect policy, and they must not:
//!
//! * [`Deterministic`](ExecutionStrategy::Deterministic) and
//!   [`Speculative`](ExecutionStrategy::Speculative) issue **exactly the same**
//!   effects. Same keys, same order.
//! * [`Replay`](ExecutionStrategy::Replay) and [`Observe`](ExecutionStrategy::Observe)
//!   issue **none** — that is what they are for, and it is asserted rather than
//!   assumed.
//!
//! The two properties are separate because a single test over "same events"
//! would be satisfied by a sink that dropped everything on the floor, and a
//! runtime that loses effects is worse than one that never had any.
//!
//! # Why "events" is a projection of the ledger and not of the sink
//!
//! The world is not the log. A strategy changes what reaches the world, and the
//! log must not change with it — otherwise a `replay` run could "agree" with a
//! `deterministic` run simply by having issued nothing and recorded nothing.
//! [`StrategyRun::events`] is therefore derived from the run's durable
//! records, excluding the simulated clock, which is the only field allowed to
//! differ between a synchronous and a run-ahead execution of the same plan.

use std::fmt;

use crate::domain::effect::{
    EffectClass, EffectKey, EffectOp, ObservationOnlySink, RecordingSink, SuppressedAttempt,
    SuppressedSink,
};
use crate::domain::ids::{ExecutionId, FutureId};
use crate::domain::node::StateNode;
use crate::domain::plan::Plan;
use crate::domain::state::State;
use crate::domain::version::{Sequence, Version};
use crate::kernel::evaluate::{Evaluator, Selection};
use crate::kernel::policy::{Durability, ExecutionPolicy};
use crate::kernel::recovery::rebuild;
use crate::kernel::{Kernel, KernelError, KernelMetrics, RunReport};
use crate::ledger::record::LedgerRecord;
use crate::ledger::MemoryStore;
use crate::ports::{DurableStore, ScanReport, StoreError};

/// The execution id a strategy run belongs to.
///
/// Fixed rather than allocated, because a replay has to resume *the same*
/// execution the log it is reading belongs to, and a random id would make that
/// impossible to state. It matches the default the kernel allocates in
/// [`Kernel::with_ports`], which is what makes a replay of a fresh run possible
/// at all.
const EXECUTION: ExecutionId = ExecutionId::new(1);

/// Why a strategy could not run the workload it was given.
#[derive(Debug)]
pub enum StrategyError {
    /// A strategy parameter that disagrees with the plan.
    Mismatch {
        strategy: &'static str,
        expected: String,
        found: String,
    },
    /// `Replay` needs a durable prefix to rebuild from and none was supplied.
    NoReplaySource,
    /// Reading the replay source failed.
    Source(StoreError),
    /// The kernel refused the run.
    Kernel(KernelError),
}

impl fmt::Display for StrategyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StrategyError::Mismatch {
                strategy,
                expected,
                found,
            } => write!(
                f,
                "strategy `{strategy}` expects {expected}, but the workload has {found}"
            ),
            StrategyError::NoReplaySource => write!(
                f,
                "the `replay` strategy rebuilds from a durable prefix and none was supplied"
            ),
            StrategyError::Source(e) => write!(f, "replay source unreadable: {e}"),
            StrategyError::Kernel(e) => write!(f, "kernel: {e}"),
        }
    }
}

impl std::error::Error for StrategyError {}

impl From<KernelError> for StrategyError {
    fn from(e: KernelError) -> Self {
        StrategyError::Kernel(e)
    }
}

impl From<crate::kernel::recovery::RecoveryError> for StrategyError {
    fn from(e: crate::kernel::recovery::RecoveryError) -> Self {
        StrategyError::Kernel(KernelError::Recovery(e))
    }
}

impl From<StoreError> for StrategyError {
    fn from(e: StoreError) -> Self {
        StrategyError::Source(e)
    }
}

/// How a workload is executed.
///
/// The names are the four strings a product stores in a contract's `execution`
/// field, and [`Self::from_name`] is the parser for that field — a value that
/// round-trips is a value that cannot be stored unrecognised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionStrategy {
    /// One arm at a time, every record durable before the next step.
    ///
    /// The boring, correct, default choice. Not because the others are worse,
    /// but because a customer who has heard of none of this should get the
    /// answer they can reason about.
    Deterministic,
    /// Fork `arms` alternatives, run them with run-ahead, commit the best.
    ///
    /// `arms` is checked against the plan rather than trusted, so a contract
    /// cannot ask for four alternatives from a plan that has three.
    Speculative { arms: usize, window: usize },
    /// Rebuild a past run from its durable log alone, contacting nothing.
    Replay,
    /// Evaluate the workload and take no action of its own.
    Observe,
}

impl ExecutionStrategy {
    /// The stable name, as stored and as displayed.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ExecutionStrategy::Deterministic => "deterministic",
            ExecutionStrategy::Speculative { .. } => "speculative",
            ExecutionStrategy::Replay => "replay",
            ExecutionStrategy::Observe => "observe",
        }
    }

    /// Parses a stored name. `arms` and `window` come from the workload, so a
    /// parsed strategy is a *shape*; [`Self::with_arms`] fills in the numbers.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "deterministic" => Some(ExecutionStrategy::Deterministic),
            "speculative" => Some(ExecutionStrategy::Speculative { arms: 0, window: 0 }),
            "replay" => Some(ExecutionStrategy::Replay),
            "observe" => Some(ExecutionStrategy::Observe),
            _ => None,
        }
    }

    /// The names, for a validation message that lists the options.
    #[must_use]
    pub fn names() -> [&'static str; 4] {
        ["deterministic", "speculative", "replay", "observe"]
    }

    /// Fills in the alternative count from the plan, for a parsed strategy.
    #[must_use]
    pub fn with_arms(self, arms: usize) -> Self {
        match self {
            ExecutionStrategy::Speculative { window, .. } => {
                ExecutionStrategy::Speculative { arms, window }
            }
            other => other,
        }
    }

    /// Whether this strategy rebuilds a past run rather than executing a fresh
    /// one.
    #[must_use]
    pub fn is_replay(self) -> bool {
        matches!(self, ExecutionStrategy::Replay)
    }

    /// Whether this strategy lets **no write** reach the world.
    ///
    /// Writes, not effects: a read is not an action. `observe` evaluates the
    /// workload and changes nothing, and a strategy that also refused to look
    /// would not be observing — it would be computing a different answer and
    /// calling it the same one.
    #[must_use]
    pub fn suppresses_writes(self) -> bool {
        matches!(self, ExecutionStrategy::Replay | ExecutionStrategy::Observe)
    }

    /// Whether this strategy contacts the world at all, reads included.
    #[must_use]
    pub fn contacts_the_world(self) -> bool {
        !matches!(self, ExecutionStrategy::Replay)
    }

    /// Whether this strategy runs ahead of durability.
    #[must_use]
    pub fn is_speculative(self) -> bool {
        matches!(self, ExecutionStrategy::Speculative { .. })
    }

    /// The policy this strategy runs under, given the caller's evaluator.
    ///
    /// The evaluator is *not* part of the strategy on purpose. Choosing how
    /// alternatives are judged is a property of the workload; choosing how they
    /// are timed and whether they may touch the world is a property of the
    /// strategy. A strategy that also chose the evaluator could make two
    /// strategies agree by scoring them the same way, which would make the
    /// agreement test prove nothing.
    #[must_use]
    pub fn policy(self, evaluator: Box<dyn Evaluator>) -> ExecutionPolicy {
        let durability = match self {
            ExecutionStrategy::Speculative { window, .. } => Durability::Async { window },
            _ => Durability::Synchronous,
        };
        ExecutionPolicy {
            durability,
            evaluator,
            ..ExecutionPolicy::default()
        }
    }

    /// Checks the strategy against the workload it was handed.
    ///
    /// # Errors
    /// Returns [`StrategyError::Mismatch`] if `Speculative { arms }` names a
    /// different number of alternatives than the plan contains.
    pub fn check(&self, plan: &Plan) -> Result<(), StrategyError> {
        if let ExecutionStrategy::Speculative { arms, .. } = *self {
            let found = plan
                .nodes()
                .iter()
                .filter_map(|n| match n {
                    crate::domain::plan::PlanNode::Fork { arms, .. } => Some(arms.len()),
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            if found != arms {
                return Err(StrategyError::Mismatch {
                    strategy: self.name(),
                    expected: format!("a fork with {arms} arms"),
                    found: if found == 0 {
                        "no fork at all".to_owned()
                    } else {
                        format!("a fork with {found} arms")
                    },
                });
            }
        }
        Ok(())
    }

    /// Runs `plan` under this strategy.
    ///
    /// # Errors
    /// Returns [`StrategyError`] if the strategy does not fit the plan, if a
    /// replay has no source, or if the kernel refuses the run.
    pub fn run(self, plan: Plan, ctx: StrategyContext) -> Result<StrategyRun, StrategyError> {
        self.check(&plan)?;
        let policy = self.policy(ctx.evaluator);
        let durability = policy.durability;
        let sink = match self {
            // Replay takes nothing from the world: read results come from the
            // journal it rebuilt, which is consulted before the sink.
            ExecutionStrategy::Replay => RunSink::Suppressed(SuppressedSink::new()),
            // Observe reads and writes nothing.
            ExecutionStrategy::Observe => RunSink::Observation(ObservationOnlySink::new()),
            _ => RunSink::Recording(RecordingSink::new()),
        };
        if self.is_replay() {
            let mut source = ctx.source.ok_or(StrategyError::NoReplaySource)?;
            // The source is recovered first: a crash leaves a torn tail, and a
            // replay that read one would rebuild from bytes no process ever durably
            // wrote. `ScanReport` records what was dropped, so "this came from the
            // log" is checkable rather than asserted.
            let scan = source.recover()?;
            let payloads = source.read_from(crate::domain::version::LogPosition::START)?;
            let records: Vec<LedgerRecord> = payloads
                .iter()
                .map(|b| LedgerRecord::decode(b))
                .collect::<Result<Vec<_>, _>>()?;

            let graph = rebuild(&records, EXECUTION, plan.id)?;
            let mut store = MemoryStore::new();
            for payload in payloads {
                store.append(payload)?;
            }
            store.commit_all()?;

            let mut kernel = Kernel::resume(
                plan,
                policy,
                EXECUTION,
                graph,
                records.clone(),
                Box::new(store),
                Box::new(sink.clone()),
            )?;
            let report = kernel.run()?;
            let mut run = StrategyRun::from(self, &report, durability, sink);
            // A replay's events are the events it *read*, not the events it wrote.
            // A completed run writes nothing and an interrupted one writes only the
            // remainder; either way the stream a caller must see is the one the log
            // holds, and that is the property the agreement test is about.
            run.events = contract_events(&records);
            run.replay = Some(ReplayEvidence {
                records_replayed: records.len(),
                records_written: report.records.len() - records.len(),
                scan,
            });
            return Ok(run);
        }
        let mut kernel = Kernel::with_ports(plan, policy, ctx.store, Box::new(sink.clone()))?;
        let report = kernel.run()?;
        Ok(StrategyRun::from(self, &report, durability, sink))
    }
}

/// The sink a strategy run uses, held so the run can report what the world saw.
///
/// Both inner types are clone-and-share: a clone observes the same log as the
/// copy the kernel owns, so a strategy can be interrogated after the kernel has
/// taken its sink. That is the only reason this enum exists — an adapter would
/// have hidden the sink's type and with it the ability to read its log.
#[derive(Clone)]
enum RunSink {
    Recording(RecordingSink),
    Observation(ObservationOnlySink),
    Suppressed(SuppressedSink),
}

impl crate::domain::effect::EffectSink for RunSink {
    fn perform(
        &mut self,
        key: &EffectKey,
        op: &EffectOp,
        class: EffectClass,
    ) -> Result<crate::domain::effect::EffectValue, crate::domain::effect::EffectError> {
        match self {
            RunSink::Recording(s) => s.perform(key, op, class),
            RunSink::Observation(s) => s.perform(key, op, class),
            RunSink::Suppressed(s) => s.perform(key, op, class),
        }
    }
}

/// What a `replay` run was built from, kept so a test can assert it was in fact
/// a rebuild rather than a second execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayEvidence {
    /// Durable records read from the source store.
    pub records_replayed: usize,
    /// Records the replay had to append to finish. Zero when the source was
    /// already a complete run; positive when the source was cut short, which is
    /// the case that proves a replay can *continue* rather than merely re-read.
    pub records_written: usize,
    /// What the source store found when it was opened: the longest valid prefix,
    /// and how much of a torn tail was dropped.
    pub scan: ScanReport,
}

/// One event a strategy's execution produced, projected from the durable log.
///
/// The projection deliberately omits `timestamp_us`. That field is the run's
/// simulated clock, and the simulated clock is exactly what a run-ahead
/// execution and a synchronous execution are allowed to disagree about. Every
/// other part of a record is a decision, and decisions must not vary with
/// timing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContractEvent {
    pub sequence: Sequence,
    /// The record's own rendering: what happened, to which future, with which
    /// disposition.
    pub summary: String,
}

/// Projects a run's records into the strategy-independent event list.
#[must_use]
pub fn contract_events(records: &[LedgerRecord]) -> Vec<ContractEvent> {
    records
        .iter()
        .map(|r| ContractEvent {
            sequence: r.sequence,
            summary: r.payload.summary(),
        })
        .collect()
}

/// The result of running a workload under one strategy.
#[derive(Clone, Debug)]
pub struct StrategyRun {
    pub strategy: ExecutionStrategy,
    /// The authoritative state.
    pub trunk: State,
    /// Content address of `trunk`. Compared rather than the state itself, so a
    /// strategy cannot pass by producing an equal-looking but differently
    /// addressed state.
    pub trunk_content: StateNode,
    pub trunk_version: Version,
    pub authoritative: FutureId,
    pub selection: Option<Selection>,
    /// The strategy-independent event list. Equal across all four strategies.
    pub events: Vec<ContractEvent>,
    /// Effects that reached the world, in order. Empty for a suppressing
    /// strategy.
    pub issued: Vec<(EffectKey, EffectOp)>,
    /// Every effect the sink was *offered*, deduplicated ones included.
    ///
    /// The difference from `issued` is the load-bearing one: an effect that was
    /// never offered was never issued, which is a stronger statement than "was
    /// not committed" and is the assertion a rejected future has to pass.
    pub offered: Vec<(EffectKey, EffectOp)>,
    /// Writes the runtime was willing to perform and did not. Empty for a
    /// strategy that changes the world.
    pub suppressed: Vec<SuppressedAttempt>,
    pub metrics: KernelMetrics,
    /// The durability the run was performed under. Recorded so a dashboard can
    /// show how a contract actually executed, not merely which name it carried.
    pub durability: Durability,
    /// Records the run wrote.
    pub durable_records: usize,
    /// Present only for `replay`.
    pub replay: Option<ReplayEvidence>,
}

impl StrategyRun {
    /// Assembles a run from its report and the sink that saw the world.
    fn from(
        strategy: ExecutionStrategy,
        report: &RunReport,
        report_durability: Durability,
        sink: RunSink,
    ) -> Self {
        let (issued, offered, suppressed) = match &sink {
            RunSink::Recording(s) => (s.observed(), s.attempts(), Vec::new()),
            RunSink::Observation(s) => {
                let refused = s.suppressed();
                let offered: Vec<_> = s
                    .observed()
                    .into_iter()
                    .chain(refused.iter().map(|a| (a.key.clone(), a.op.clone())))
                    .collect();
                (s.issued(), offered, refused)
            }
            RunSink::Suppressed(s) => {
                let attempts = s.attempted();
                let offered = attempts
                    .iter()
                    .map(|a| (a.key.clone(), a.op.clone()))
                    .collect();
                (s.issued(), offered, attempts)
            }
        };
        Self {
            strategy,
            trunk: report.trunk.clone(),
            trunk_content: report.trunk_content,
            trunk_version: report.trunk_version,
            authoritative: report.authoritative,
            selection: report.selection.clone(),
            events: contract_events(&report.records),
            issued,
            offered,
            suppressed,
            metrics: report.metrics.clone(),
            durability: report_durability,
            durable_records: report.records.len(),
            replay: None,
        }
    }

    /// The part of the outcome every strategy must agree on.
    #[must_use]
    pub fn agreement(&self) -> Agreement<'_> {
        Agreement {
            trunk_content: self.trunk_content,
            events: &self.events,
            issued: &self.issued,
        }
    }

    /// A stable one-line rendering, for a failure message in the agreement test.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "trunk={} events={} issued={} suppressed={} flushes={} wall={:?}",
            self.trunk_content,
            self.events.len(),
            self.issued.len(),
            self.suppressed.len(),
            self.metrics.flushes,
            self.metrics.wall_time
        );
        if let Some(replay) = &self.replay {
            out.push_str(&format!(
                " replayed_from={} replay_wrote={} torn_dropped={}",
                replay.records_replayed, replay.records_written, replay.scan.torn_bytes_dropped
            ));
        }
        out
    }
}

/// The slice of an outcome that is identical for every strategy.
///
/// Comparing this and nothing else would let a strategy that issued no effects
/// pass; [`Agreement::issued_equal`] is the second half of the assertion and
/// only the suppressing strategies are excused from it.
#[derive(Clone, Copy, Debug)]
pub struct Agreement<'a> {
    pub trunk_content: StateNode,
    pub events: &'a [ContractEvent],
    pub issued: &'a [(EffectKey, EffectOp)],
}

impl Agreement<'_> {
    /// Whether two strategies let exactly the same effects reach the world.
    #[must_use]
    pub fn issued_equal(&self, other: &Self) -> bool {
        self.issued == other.issued
    }
}

/// The ports and the choices a strategy needs to run.
pub struct StrategyContext {
    /// How alternatives are judged. The caller's, never the strategy's.
    pub evaluator: Box<dyn Evaluator>,
    /// Where a fresh execution writes. Unused by `replay`, which rebuilds.
    pub store: Box<dyn DurableStore>,
    /// The durable prefix a `replay` rebuilds from.
    pub source: Option<Box<dyn DurableStore>>,
}

impl StrategyContext {
    /// A context for a fresh execution.
    #[must_use]
    pub fn new(evaluator: Box<dyn Evaluator>, store: Box<dyn DurableStore>) -> Self {
        Self {
            evaluator,
            store,
            source: None,
        }
    }

    /// Adds the durable prefix a `replay` rebuilds from.
    #[must_use]
    pub fn with_source(mut self, source: Box<dyn DurableStore>) -> Self {
        self.source = Some(source);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::PlanId;
    use crate::domain::Operation;
    use crate::kernel::evaluate::HighestScore;

    fn arm(value: i64) -> Plan {
        Plan::builder(PlanId::new(2), "arm")
            .state(Operation::Set {
                key: "score".into(),
                value,
            })
            .build()
    }

    fn branching() -> Plan {
        Plan::builder(PlanId::new(1), "branching")
            .state(Operation::Set {
                key: "score".into(),
                value: 0,
            })
            .fork("choose", vec![arm(10), arm(20), arm(15)])
            .select()
            .commit()
            .build()
    }

    fn deterministic() -> ExecutionStrategy {
        ExecutionStrategy::Deterministic
    }

    fn speculative(arms: usize) -> ExecutionStrategy {
        ExecutionStrategy::Speculative { arms, window: 4 }
    }

    fn run(strategy: ExecutionStrategy, plan: &Plan) -> StrategyRun {
        strategy
            .run(
                plan.clone(),
                StrategyContext::new(
                    Box::new(HighestScore::new("score")),
                    Box::new(MemoryStore::new()),
                ),
            )
            .expect("strategy runs")
    }

    #[test]
    fn a_deterministic_run_of_a_three_arm_plan_produces_the_events_a_caller_reads() {
        let run = run(deterministic(), &branching());
        assert_eq!(run.trunk.get("score"), 20);
        assert!(!run.events.is_empty());
        assert_eq!(
            run.events.len(),
            run.durable_records,
            "every record the run wrote is an event a caller can see"
        );
    }

    #[test]
    fn names_round_trip() {
        for name in ExecutionStrategy::names() {
            let parsed = ExecutionStrategy::from_name(name).expect("a known name");
            assert_eq!(parsed.name(), name);
        }
        assert_eq!(ExecutionStrategy::from_name("nope"), None);
    }

    #[test]
    fn only_speculative_runs_ahead_and_two_of_the_four_write_nothing() {
        assert!(!ExecutionStrategy::Deterministic.is_speculative());
        assert!(speculative(3).is_speculative());
        assert!(!ExecutionStrategy::Replay.is_speculative());
        assert!(!ExecutionStrategy::Observe.is_speculative());
        assert!(!ExecutionStrategy::Deterministic.suppresses_writes());
        assert!(!speculative(3).suppresses_writes());
        assert!(ExecutionStrategy::Replay.suppresses_writes());
        assert!(ExecutionStrategy::Observe.suppresses_writes());
        assert!(ExecutionStrategy::Replay.is_replay());
        assert!(!ExecutionStrategy::Replay.contacts_the_world());
        assert!(ExecutionStrategy::Observe.contacts_the_world());
    }

    #[test]
    fn the_policy_follows_the_strategy_and_nothing_else() {
        let p = ExecutionStrategy::Speculative { arms: 3, window: 9 }
            .policy(Box::new(HighestScore::new("score")));
        assert_eq!(p.durability, Durability::Async { window: 9 });
        assert_eq!(p.workers, 1, "the kernel's default is not overridden");
        assert_eq!(p.max_attempts, 4, "not the strategy's business");
        assert_eq!(
            ExecutionStrategy::Observe
                .policy(Box::new(HighestScore::new("score")))
                .durability,
            Durability::Synchronous
        );
    }

    #[test]
    fn a_speculative_arm_count_that_disagrees_with_the_plan_is_refused() {
        let plan = branching();
        assert!(speculative(3).check(&plan).is_ok());
        let err = speculative(2).check(&plan).unwrap_err();
        assert!(
            err.to_string().contains("expects a fork with 2 arms"),
            "{err}"
        );
        assert!(
            err.to_string()
                .contains("the workload has a fork with 3 arms"),
            "{err}"
        );
        assert!(
            deterministic().check(&plan).is_ok(),
            "only speculative names an arm count"
        );
    }

    #[test]
    fn a_speculative_strategy_against_a_plan_with_no_fork_is_refused() {
        let linear = Plan::linear(
            PlanId::new(1),
            "linear",
            vec![Operation::Add {
                key: "n".into(),
                by: 1,
            }],
        );
        let err = speculative(2).check(&linear).unwrap_err();
        assert!(err.to_string().contains("no fork at all"), "{err}");
    }

    #[test]
    fn a_replay_without_a_source_is_an_error_naming_what_it_wanted() {
        let err = ExecutionStrategy::Replay
            .run(
                branching(),
                StrategyContext::new(
                    Box::new(HighestScore::new("score")),
                    Box::new(MemoryStore::new()),
                ),
            )
            .unwrap_err();
        assert!(matches!(err, StrategyError::NoReplaySource), "{err}");
        assert!(err.to_string().contains("durable prefix"), "{err}");
    }

    #[test]
    fn a_parsed_strategy_takes_its_arm_count_from_the_workload() {
        let parsed = ExecutionStrategy::from_name("speculative").unwrap();
        assert_eq!(
            parsed,
            ExecutionStrategy::Speculative { arms: 0, window: 0 }
        );
        assert_eq!(
            parsed.with_arms(3),
            ExecutionStrategy::Speculative { arms: 3, window: 0 },
            "a parsed strategy carries no window until one is chosen"
        );
        // A window of zero is still a run-ahead window: `Durability` clamps it, so
        // a caller cannot store a strategy that silently stops speculating.
        assert_eq!(
            parsed
                .with_arms(3)
                .policy(Box::new(HighestScore::new("score")))
                .durability
                .window(),
            1
        );
        assert_eq!(parsed.with_arms(3).name(), speculative(3).name());
        // And a name that is not speculative is unaffected by the fill-in.
        assert_eq!(
            ExecutionStrategy::Observe.with_arms(9),
            ExecutionStrategy::Observe
        );
    }
}
