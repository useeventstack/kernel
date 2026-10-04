//! Shared helpers for the integration tests.
//!
//! The tests drive the public API the same way the CLI does, so they exercise the
//! shipped code paths rather than a parallel test-only implementation.
#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

use ues::domain::effect::{EffectClass, EffectOp, RecordingSink};
use ues::domain::ids::PlanId;
use ues::domain::plan::Plan;
use ues::domain::Operation;
use ues::kernel::policy::ExecutionPolicy;
use ues::kernel::{Kernel, RunReport};
use ues::ledger::store::FileStore;
use ues::ports::CostModel;
use ues::simulation::{FailureInjector, FailurePoint};

pub const STEP_COST_MS: u64 = 1;

/// A linear plan: `n` increments of `counter`.
pub fn linear(n: usize) -> Plan {
    Plan::linear(
        PlanId::new(1),
        "linear",
        (0..n)
            .map(|_| Operation::Add {
                key: "counter".into(),
                by: 1,
            })
            .collect(),
    )
}

/// A linear plan whose steps are configurable, for latency sweeps.
pub fn linear_with_cost(n: usize, cost_ms: u64) -> Plan {
    Plan::builder(PlanId::new(1), "linear")
        .state_named(
            "step",
            Operation::Add {
                key: "counter".into(),
                by: 1,
            },
            Duration::from_millis(cost_ms),
        )
        .repeated(n)
        .build()
}

/// The canonical branching plan: three arms that differ *logically*, not just in
/// a parameter, then a selection and a commit.
///
/// ```text
///   seed = 0
///   ├── arm A: total += 10; score = total
///   ├── arm B: quote 5 -> total += 10; total *= 2; score = total
///   └── arm C: total += 20; score = total / 2
///   select the highest score
///   commit
/// ```
pub fn branching() -> Plan {
    let arm = |value: i64| {
        Plan::builder(PlanId::new(2), "arm")
            .state(Operation::Add {
                key: "total".into(),
                by: value,
            })
            .state(Operation::Set {
                key: "score".into(),
                value,
            })
            .build()
    };
    Plan::builder(PlanId::new(1), "branching")
        .state(Operation::Set {
            key: "total".into(),
            value: 0,
        })
        .fork("choose", vec![arm(10), arm(20), arm(15)])
        .select()
        .commit()
        .state(Operation::Add {
            key: "committed".into(),
            by: 1,
        })
        .build()
}

/// The same plan, but every arm performs an irreversible write.
///
/// This is the effect-safety demonstration: the losing arms must never charge
/// anything.
pub fn branching_with_irreversible_effects() -> Plan {
    let arm = |value: i64| {
        Plan::builder(PlanId::new(2), "arm")
            .effect(
                "charge",
                EffectClass::Irreversible,
                EffectOp::new("charge_card", value),
                None,
                Some("charged"),
            )
            .state(Operation::Add {
                key: "score".into(),
                by: value,
            })
            .build()
    };
    Plan::builder(PlanId::new(1), "branching-effects")
        .fork("choose", vec![arm(10), arm(20), arm(15)])
        .select()
        .commit()
        .build()
}

/// A plan that reads the world, so replay determinism can be tested.
pub fn observing() -> Plan {
    Plan::builder(PlanId::new(1), "observing")
        .effect(
            "quote",
            EffectClass::Read,
            EffectOp::new("quote", 10),
            Some("quoted"),
            None,
        )
        .state(Operation::Add {
            key: "counter".into(),
            by: 1,
        })
        .build()
}

/// A plan whose second arm fails, to exercise rejection of a broken future.
pub fn branching_with_a_failing_arm() -> Plan {
    let good = Plan::builder(PlanId::new(2), "good")
        .state(Operation::Set {
            key: "score".into(),
            value: 5,
        })
        .build();
    let bad = Plan::builder(PlanId::new(2), "bad")
        .state(Operation::Fail {
            reason: "arm b is broken".into(),
        })
        .state(Operation::Set {
            key: "score".into(),
            value: 99,
        })
        .build();
    Plan::builder(PlanId::new(1), "branching-failure")
        .fork("choose", vec![good, bad])
        .select()
        .commit()
        .build()
}

/// Runs a plan with the given policy, in memory, with no injected failure.
pub fn run(plan: Plan, policy: ExecutionPolicy) -> Result<RunReport, ues::KernelError> {
    run_with(plan, policy, CostModel::ZERO, FailureInjector::none())
}

/// Runs a plan with everything spelled out.
pub fn run_with(
    plan: Plan,
    policy: ExecutionPolicy,
    cost: CostModel,
    injector: FailureInjector,
) -> Result<RunReport, ues::KernelError> {
    let mut kernel = Kernel::new(plan, policy)?
        .with_cost(cost)
        .with_injector(injector);
    kernel.run()
}

/// Runs a plan against a filesystem ledger, returning the kernel so the caller can
/// inspect the lineage afterwards.
pub fn run_on_disk(
    plan: Plan,
    policy: ExecutionPolicy,
    path: &Path,
) -> Result<(RunReport, Kernel), ues::KernelError> {
    let store = FileStore::create(path).map_err(ues::KernelError::Ledger)?;
    let mut kernel = Kernel::with_ports(
        plan,
        policy,
        Box::new(store),
        Box::new(RecordingSink::new()),
    )?;
    let report = kernel.run()?;
    Ok((report, kernel))
}

/// The default "a normal developer should get this" policy.
pub fn durable_policy() -> ExecutionPolicy {
    ExecutionPolicy::durable()
}

/// An explicitly speculative policy: evaluate alternatives and commit the best.
pub fn exploring() -> ExecutionPolicy {
    ExecutionPolicy::exploring(Box::new(ues::HighestScore::new("score")), 8).with_workers(3)
}

/// A key/value rendering of a state, for failure messages.
pub fn render(state: &ues::State) -> String {
    state
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Every failure point that does not name a future.
pub fn global_failure_points() -> Vec<FailurePoint> {
    FailurePoint::GLOBAL.to_vec()
}

/// Every failure point for one future, up to `n` records.
pub fn per_record_failure_points(future: u64, n: usize) -> Vec<FailurePoint> {
    (0..n)
        .flat_map(|i| FailurePoint::per_record(future, i))
        .collect()
}

/// A unique temporary directory for a test that needs real files.
pub fn temp_dir(prefix: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("ues-strategy-{prefix}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).expect("create temp dir");
    path
}
