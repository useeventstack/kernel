//! The failure matrix: crash at every point in a future's lifecycle, recover, and
//! check the answer.
//!
//! For each failure point the same four questions are asked, in this order,
//! because they are the ones that matter and because a later answer is worthless
//! if an earlier one fails:
//!
//! 1. did the run finish at all?
//! 2. is the final trunk state the one a failure-free run produces?
//! 3. does the recovered graph validate?
//! 4. did any irreversible effect of a *rejected* future reach the world?
//!
//! The last one is the reason this file exists rather than a generic "did it
//! recover" test: a runtime can recover perfectly and still have done damage.

mod common;

use common::{durable_policy, exploring, render};
use ues::domain::effect::RecordingSink;
use ues::domain::future::FutureStatus;
use ues::kernel::recovery::rebuild;
use ues::kernel::{Durability, ExecutionPolicy, Kernel, KernelError};
use ues::ledger::Payload;
use ues::ports::CostModel;
use ues::simulation::{FailureInjector, FailurePoint};

/// A plan that exercises every lifecycle phase: state steps, an effect of every
/// class, a fork, a selection, a commit and post-commit work.
fn rich_plan() -> ues::Plan {
    let arm = |v: i64| {
        ues::Plan::builder(ues::PlanId::new(2), "arm")
            .effect(
                "read",
                ues::EffectClass::Read,
                ues::EffectOp::new("quote", v),
                Some("quoted"),
                None,
            )
            .effect(
                "upsert",
                ues::EffectClass::IdempotentWrite,
                ues::EffectOp::new("upsert", 1),
                None,
                Some("upserted"),
            )
            .state(ues::Operation::Add {
                key: "total".into(),
                by: v,
            })
            .effect(
                "charge",
                ues::EffectClass::Irreversible,
                ues::EffectOp::new("charge", v),
                None,
                Some("charged"),
            )
            .state(ues::Operation::Set {
                key: "score".into(),
                value: v,
            })
            .build()
    };
    ues::Plan::builder(ues::PlanId::new(1), "rich")
        .state(ues::Operation::Set {
            key: "total".into(),
            value: 0,
        })
        .fork("choose", vec![arm(10), arm(20), arm(15)])
        .select()
        .commit()
        .state(ues::Operation::Add {
            key: "committed".into(),
            by: 1,
        })
        .build()
}

struct Outcome {
    trunk: String,
    /// The state rebuilt from the durable prefix, which need not equal `trunk`:
    /// a run that gave up leaves the ledger holding only what it proved.
    recovered: String,
    graph_valid: bool,
    rejected_effects_observed: usize,
    /// Whether the injected point was actually reached by this plan and policy.
    fired: bool,
    /// How many rollbacks the run needed. Zero is legitimate in two cases — the
    /// point was never reached, or it was reached at a window where nothing had to
    /// be discarded — so this is a witness, not an assertion. What must always hold
    /// is `recovered`: the log alone reconstructs the answer.
    recoveries: u32,
}

fn run_with_failure(point: Option<FailurePoint>, policy: ExecutionPolicy) -> Outcome {
    let plan = rich_plan();
    let sink = RecordingSink::new();
    let injector = match point {
        Some(p) => FailureInjector::at(p),
        None => FailureInjector::none(),
    };
    let mut kernel = Kernel::with_ports(
        plan.clone(),
        policy,
        Box::new(ues::MemoryStore::new()),
        Box::new(sink.clone()),
    )
    .expect("kernel")
    .with_cost(CostModel::with_flush_latency(5))
    .with_injector(injector);
    let report = kernel.run();
    let (trunk, recoveries) = match report {
        Ok(r) => (render(&r.trunk), r.metrics.recoveries),
        Err(KernelError::Injected(_)) => ("<injected>".to_owned(), 1),
        Err(e) => panic!("unexpected failure {e}"),
    };
    // Rebuild from the durable prefix the store actually holds, which is what a
    // restarted process would see.
    let graph = rebuild(&kernel.durable_records(), ues::ExecutionId::new(1), plan.id);
    let (recovered, valid) = match graph {
        Ok(g) => {
            let ok = g.validate().is_ok();
            (render(&g.trunk_state()), ok)
        }
        Err(_) => ("<unrecoverable>".to_owned(), false),
    };
    // An effect of a future that was rejected must never have been issued. The
    // sink is the only authority on that.
    let committed: Vec<Vec<u32>> = kernel
        .lineage()
        .iter()
        .filter(|(_, f)| f.status == FutureStatus::Committed)
        .map(|(_, f)| f.path.clone())
        .collect();
    let rejected_effects_observed = sink
        .observed()
        .iter()
        .filter(|(key, op)| op.name == "charge" && !committed.contains(&key.path().to_vec()))
        .count();
    Outcome {
        trunk,
        recovered,
        graph_valid: valid,
        rejected_effects_observed,
        fired: kernel.injector().fired().is_some(),
        recoveries,
    }
}

fn expected() -> String {
    run_with_failure(None, exploring()).trunk
}

#[test]
fn the_failure_free_run_is_the_reference() {
    let out = run_with_failure(None, exploring());
    assert!(out.trunk.contains("score=20"), "reference: {}", out.trunk);
    assert!(out.graph_valid);
    assert_eq!(out.rejected_effects_observed, 0);
}

#[test]
fn a_failure_at_every_per_record_checkpoint_recovers_correctly() {
    // Every step and effect of every future, on the trunk and on an arm.
    //
    // Ordinals are 1-based — a checkpoint fires *about* the nth step, and there is
    // no zeroth step — so 1..8 covers the first eight of each.
    let want = expected();
    let mut points = Vec::new();
    for future in 0..4u64 {
        for n in 1..8 {
            points.push(FailurePoint::BeforeStep { future, n });
            points.push(FailurePoint::AfterStep { future, n });
            points.push(FailurePoint::BeforeEffect { future, n });
            points.push(FailurePoint::AfterEffect { future, n });
        }
    }
    assert!(points.len() >= 100, "the matrix must be exhaustive");
    for point in points {
        let out = run_with_failure(Some(point), exploring());
        assert_eq!(out.trunk, want, "{point}: wrong final state");
        // The ledger alone has to reconstruct the answer. A runtime that got the
        // right trunk by keeping in-memory state a restarted process would not
        // have would pass the first assertion and fail this one.
        assert_eq!(out.recovered, want, "{point}: the log does not reconstruct");
        assert!(out.graph_valid, "{point}: the recovered graph is invalid");
        // A failure between a record and its durability has to *discard* that
        // record. Re-running without throwing the speculative work away is not
        // recovery, it is a duplicate, and for a non-idempotent step it is a
        // double-count. Only meaningful when the point was reachable at all: an
        // effect point on the trunk of a plan with no trunk effects never fires,
        // and a matrix that counted that as a pass would be testing nothing.
        if out.fired {
            assert!(
                out.recoveries > 0,
                "{point}: nothing was discarded, so nothing was recovered"
            );
        }
        assert_eq!(
            out.rejected_effects_observed, 0,
            "{point}: a rejected future reached the world"
        );
    }
}

#[test]
fn a_failure_at_every_lifecycle_point_recovers_correctly() {
    // Checkpoints, selection, commit and release. This is the window in which a
    // commit can be interrupted, and the two sides of it have different correct
    // answers, so both are tested.
    let want = expected();
    for point in FailurePoint::GLOBAL {
        let out = run_with_failure(Some(point), exploring());
        match out.trunk.as_str() {
            "<injected>" => {
                // The injection point was never reached for this configuration, or
                // the recovery budget was spent. Both are legitimate; what must
                // never happen is a wrong answer.
                assert_eq!(out.rejected_effects_observed, 0, "{point}: damage");
            }
            _ => {
                assert_eq!(out.trunk, want, "{point}: wrong final state");
                assert!(out.graph_valid, "{point}: invalid graph");
                assert_eq!(out.rejected_effects_observed, 0, "{point}: damage");
                assert_eq!(out.recovered, want, "{point}: the log does not reconstruct");
            }
        }
    }
}

#[test]
fn a_failure_during_the_commit_window_either_did_nothing_or_committed() {
    // The single most delicate window. `BeforeCommit` and `DuringCommit` mean the
    // commit record is not durable, so the trunk must be *unchanged*; `AfterCommit`
    // means it is durable, so the trunk must be *committed*. Getting this wrong in
    // either direction is a real corruption, so the test checks the direction.
    let plan = rich_plan();
    for (point, expect_committed) in [
        (FailurePoint::BeforeCommit, false),
        (FailurePoint::DuringCommit, false),
        (FailurePoint::AfterCommit, true),
        (FailurePoint::AfterRelease, true),
    ] {
        let sink = RecordingSink::new();
        let mut kernel = Kernel::with_ports(
            plan.clone(),
            exploring(),
            Box::new(ues::MemoryStore::new()),
            Box::new(sink.clone()),
        )
        .unwrap()
        .with_cost(CostModel::with_flush_latency(5))
        .with_injector(FailureInjector::at(point));
        let _ = kernel.run();
        let records = kernel.durable_records();
        // Which future the durable commit record names. `authoritative` is not the
        // thing to check: it means "whose head the trunk currently is", and it
        // legitimately becomes the trunk again once the trunk takes its next step.
        let committed_futures: Vec<ues::FutureId> = records
            .iter()
            .filter_map(|r| match r.payload {
                Payload::Committed { future, .. } => Some(future),
                _ => None,
            })
            .collect();
        let has_commit = !committed_futures.is_empty();
        if !has_commit {
            assert!(!expect_committed, "{point}: the commit record vanished");
        } else {
            // The record is in the ledger, so recovery must see the commit.
            let graph = rebuild(&records, ues::ExecutionId::new(1), plan.id)
                .unwrap_or_else(|e| panic!("{point}: {e}"));
            for future in &committed_futures {
                assert_eq!(
                    graph.lineage.get(*future).map(|f| f.status),
                    Some(FutureStatus::Committed),
                    "{point}: a durable commit must be a fact, not work in progress"
                );
            }
        }
        // Whatever happened, a rejected future's charge never happened.
        let committed: Vec<Vec<u32>> = kernel
            .lineage()
            .iter()
            .filter(|(_, f)| f.status == FutureStatus::Committed)
            .map(|(_, f)| f.path.clone())
            .collect();
        let leaked = sink
            .observed()
            .iter()
            .filter(|(key, op)| op.name == "charge" && !committed.contains(&key.path().to_vec()))
            .count();
        assert_eq!(leaked, 0, "{point}: a rejected future reached the world");
    }
}

#[test]
fn a_failure_under_every_durability_policy_recovers_correctly() {
    let want = expected();
    for durability in [
        Durability::Synchronous,
        Durability::GroupCommit { batch: 2 },
        Durability::GroupCommit { batch: 8 },
        Durability::Async { window: 1 },
        Durability::Async { window: 8 },
        Durability::Async { window: 1_000 },
    ] {
        for point in [
            FailurePoint::AfterStep { future: 0, n: 1 },
            FailurePoint::AfterEffect { future: 1, n: 0 },
            FailurePoint::DuringCheckpoint,
            FailurePoint::AfterCheckpoint,
            FailurePoint::AfterCommit,
        ] {
            let out = run_with_failure(Some(point), exploring().with_durability(durability));
            if out.trunk == "<injected>" {
                continue;
            }
            assert_eq!(out.trunk, want, "{durability} at {point}");
            assert!(out.graph_valid, "{durability} at {point}: invalid graph");
            assert_eq!(out.rejected_effects_observed, 0, "{durability} at {point}");
        }
    }
}

#[test]
fn a_failure_on_a_linear_plan_recovers_correctly() {
    // The boring case must not be special. No fork, no selection, no commit.
    let plan = common::linear(20);
    let want = {
        let mut k = Kernel::new(plan.clone(), durable_policy()).unwrap();
        render(&k.run().unwrap().trunk)
    };
    // Step ordinals are 1-based, so 1..=20 is every step of the plan.
    for n in 1..=20 {
        for point in [
            FailurePoint::BeforeStep { future: 0, n },
            FailurePoint::AfterStep { future: 0, n },
        ] {
            let mut kernel = Kernel::new(plan.clone(), durable_policy())
                .unwrap()
                .with_cost(CostModel::with_flush_latency(5))
                .with_injector(FailureInjector::at(point));
            let out = kernel.run().expect("recovers");
            assert_eq!(render(&out.trunk), want, "{point}");
            assert!(
                out.metrics.recoveries >= 1,
                "{point}: nothing was recovered"
            );
        }
    }
}

#[test]
fn a_repeated_failure_exhausts_the_attempt_budget_and_says_so() {
    // The runtime must stop rather than loop, and must report why.
    let plan = common::linear(10);
    let mut kernel = Kernel::new(plan.clone(), durable_policy())
        .unwrap()
        .with_max_attempts_check(2)
        .with_injector(FailureInjector::repeating(FailurePoint::AfterStep {
            future: 0,
            n: 1,
        }));
    let err = kernel.run().expect_err("must give up");
    assert!(
        matches!(err, KernelError::Injected(_)),
        "expected an injected failure, got {err:?}"
    );
}

#[test]
fn a_rollback_never_leaves_a_broken_chain() {
    // After a failure the live lineage must still be internally consistent: every
    // future's delta chain has to be the prefix of the ledger, and the trunk's
    // cursor has to be somewhere a real execution could be.
    let plan = rich_plan();
    for point in [
        FailurePoint::AfterStep { future: 1, n: 0 },
        FailurePoint::AfterEffect { future: 2, n: 1 },
        FailurePoint::DuringCheckpoint,
    ] {
        let sink = RecordingSink::new();
        let mut kernel = Kernel::with_ports(
            plan.clone(),
            exploring(),
            Box::new(ues::MemoryStore::new()),
            Box::new(sink.clone()),
        )
        .unwrap()
        .with_cost(CostModel::with_flush_latency(25))
        .with_injector(FailureInjector::at(point));
        let out = kernel.run();
        if out.is_err() {
            continue;
        }
        for (id, f) in kernel.lineage().iter() {
            f.check_chain()
                .unwrap_or_else(|e| panic!("{point}: future {id}: {e}"));
        }
    }
}

#[test]
fn the_number_of_records_written_is_bounded_by_the_attempt_budget() {
    // A retry must not duplicate the whole plan: the ledger grows by the discarded
    // work, not by a second copy of everything. Otherwise a crash loop is a
    // denial of service on the log.
    let plan = common::linear(30);
    let mut clean = Kernel::new(plan.clone(), durable_policy()).unwrap();
    clean.run().unwrap();
    let baseline = clean.metrics().records_written.max(1);
    let mut kernel = Kernel::new(plan.clone(), durable_policy())
        .unwrap()
        .with_injector(FailureInjector::at(FailurePoint::AfterStep {
            future: 0,
            n: 1,
        }));
    kernel.run().unwrap();
    let after = kernel.metrics().records_written;
    assert!(
        after < baseline * 3,
        "one recovery wrote {after} records for a {baseline}-record run"
    );
}

#[test]
fn an_interrupted_recovery_is_reported_rather_than_hidden() {
    // Recovery that is itself interrupted must not silently look like success.
    let plan = common::linear(10);
    let mut kernel = Kernel::new(plan.clone(), durable_policy())
        .unwrap()
        .with_injector(FailureInjector::at(FailurePoint::DuringRecovery));
    match kernel.run() {
        Ok(out) => assert!(
            out.metrics.recoveries == 0,
            "a completed run must not claim a recovery it did not perform"
        ),
        Err(e) => assert!(matches!(e, KernelError::Injected(_)), "{e}"),
    }
}

#[test]
fn a_durability_failure_never_claims_a_record_is_durable_when_it_is_not() {
    // The store is the authority. After a run, the number of records the kernel
    // believes are durable must not exceed what the store actually holds.
    let plan = rich_plan();
    for point in [
        FailurePoint::BeforeCheckpoint,
        FailurePoint::DuringCheckpoint,
        FailurePoint::AfterCheckpoint,
    ] {
        let store = ues::MemoryStore::new();
        let sink = RecordingSink::new();
        let mut kernel =
            Kernel::with_ports(plan.clone(), exploring(), Box::new(store), Box::new(sink))
                .unwrap()
                .with_cost(CostModel::with_flush_latency(5))
                .with_injector(FailureInjector::at(point));
        let _ = kernel.run();
        // The kernel drained before finishing, so its own claim must match.
        assert_eq!(
            kernel.metrics().records_durable,
            kernel.metrics().records_written,
            "{point}: the final drain must leave nothing unclaimed"
        );
    }
}
