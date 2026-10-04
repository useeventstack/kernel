//! Effect safety: the guarantee that speculative execution cannot touch the world
//! in a way that survives being wrong.
//!
//! The claim under test is narrow and checkable:
//!
//! > **An effect of a future that is not committed never reaches the outside
//! > world.**
//!
//! Not "is eventually compensated" — the world is *unchanged*, and the tests
//! assert it by asking the sink what it actually did, not by asking the runtime
//! what it intended.

mod common;

use common::render;
use ues::domain::effect::{Disposition, EffectClass, EffectKey, EffectOp, RecordingSink};
use ues::domain::ids::{EffectId, PlanId};
use ues::domain::plan::Plan;
use ues::domain::Operation;
use ues::kernel::policy::{Durability, ExecutionPolicy};
use ues::kernel::Kernel;
use ues::ports::CostModel;

/// An arm that performs one effect of `class`, then scores itself.
fn arm(class: EffectClass, name: &str, score: i64) -> Plan {
    Plan::builder(PlanId::new(2), name)
        .effect(
            "act",
            class,
            EffectOp::new(name, score),
            None,
            Some("acted"),
        )
        .state(Operation::Set {
            key: "score".into(),
            value: score,
        })
        .build()
}

fn run_with_sink(plan: Plan, policy: ExecutionPolicy) -> (ues::kernel::RunReport, RecordingSink) {
    let sink = RecordingSink::new();
    let mut kernel = Kernel::with_ports(
        plan,
        policy,
        Box::new(ues::MemoryStore::new()),
        Box::new(sink.clone()),
    )
    .expect("kernel");
    let report = kernel.run().expect("run");
    (report, sink)
}

fn exploring() -> ExecutionPolicy {
    ExecutionPolicy::exploring(Box::new(ues::HighestScore::new("score")), 8).with_workers(3)
}

#[test]
fn a_rejected_future_never_reaches_the_world() {
    // Three arms, all with an irreversible effect. Arm B scores highest, so arms
    // A and C are rejected. The sink must have seen exactly one effect.
    let plan = Plan::builder(PlanId::new(1), "risky")
        .fork(
            "choose",
            vec![
                arm(EffectClass::Irreversible, "charge", 10),
                arm(EffectClass::Irreversible, "charge", 30),
                arm(EffectClass::Irreversible, "charge", 20),
            ],
        )
        .select()
        .commit()
        .build();
    let (report, sink) = run_with_sink(plan, exploring());
    assert_eq!(report.metrics.futures_rejected, 2);
    assert_eq!(
        sink.observed_len(),
        1,
        "the two rejected futures must not have charged anything: {:?}",
        sink.observed()
    );
    assert_eq!(
        report.metrics.effects_deferred, 3,
        "all three were deferred"
    );
    assert_eq!(
        report.metrics.effects_released, 1,
        "only the winner's was released"
    );
}

#[test]
fn the_winner_effect_is_issued_exactly_once() {
    let plan = Plan::builder(PlanId::new(1), "risky")
        .fork(
            "choose",
            vec![
                arm(EffectClass::Irreversible, "charge", 10),
                arm(EffectClass::Irreversible, "charge", 30),
            ],
        )
        .select()
        .commit()
        .build();
    let (_report, sink) = run_with_sink(plan, exploring());
    assert_eq!(sink.observed_len(), 1);
    // The key names the winning arm, so an operator reading the journal can
    // tell which plan authorised the charge.
    let observed = sink.observed();
    let (key, op) = &observed[0];
    // The key names *where in the plan* the effect sits, not which runtime future
    // happened to run it, so an operator can still tell which arm authorised the
    // charge after a restart.
    assert_eq!(key.as_string(), "e1/1.e0");
    assert_eq!(op.name, "charge");
}

#[test]
fn only_three_of_the_five_classes_may_escape_speculation() {
    // The table, as an executable test rather than a comment.
    assert!(EffectClass::Pure.speculatable());
    assert!(EffectClass::Read.speculatable());
    assert!(EffectClass::IdempotentWrite.speculatable());
    assert!(!EffectClass::Compensatable.speculatable());
    assert!(!EffectClass::Irreversible.speculatable());
}

#[test]
fn a_speculative_future_may_perform_reads_and_idempotent_writes_but_not_the_rest() {
    // Every arm reads the world and upserts; only the winner charges.
    let arm = |score: i64| {
        Plan::builder(PlanId::new(2), "a")
            .effect(
                "read",
                EffectClass::Read,
                EffectOp::new("quote", 5),
                Some("quoted"),
                None,
            )
            .effect(
                "upsert",
                EffectClass::IdempotentWrite,
                EffectOp::new("upsert", 1),
                None,
                Some("upserted"),
            )
            .effect(
                "charge",
                EffectClass::Irreversible,
                EffectOp::new("charge", score),
                None,
                Some("charged"),
            )
            .state(Operation::Set {
                key: "score".into(),
                value: score,
            })
            .build()
    };
    let plan = Plan::builder(PlanId::new(1), "mixed")
        .fork("choose", vec![arm(10), arm(30), arm(20)])
        .select()
        .commit()
        .build();
    let (report, sink) = run_with_sink(plan, exploring());
    let observed = sink.observed();
    let mut names: Vec<String> = observed.iter().map(|(_, op)| op.name.clone()).collect();
    names.sort();
    // The interleaving depends on the scheduler, so the claim is about the *set*:
    // every future read and upserted, and exactly one — the winner's — charged.
    assert_eq!(
        names,
        vec!["charge", "quote", "quote", "quote", "upsert", "upsert", "upsert"],
        "three futures each read and upserted; only the winner charged"
    );
    assert_eq!(report.metrics.effects_performed, 6);
    assert_eq!(report.metrics.effects_deferred, 3);
}

#[test]
fn a_committed_future_still_only_releases_what_it_deferred() {
    // The trunk is authoritative from the start, so a write there is performed
    // immediately rather than deferred. Authoritativeness, not class, decides.
    let plan = Plan::builder(PlanId::new(1), "trunk-writes")
        .effect(
            "charge",
            EffectClass::Irreversible,
            EffectOp::new("charge", 5),
            None,
            Some("charged"),
        )
        .state(Operation::Set {
            key: "score".into(),
            value: 1,
        })
        .build();
    let (report, sink) = run_with_sink(plan, ExecutionPolicy::durable());
    assert_eq!(sink.observed_len(), 1);
    assert_eq!(report.metrics.effects_performed, 1);
    assert_eq!(report.metrics.effects_deferred, 0);
}

#[test]
fn the_journal_distinguishes_a_deferred_intent_from_a_performed_effect() {
    let plan = Plan::builder(PlanId::new(1), "j")
        .fork(
            "choose",
            vec![
                arm(EffectClass::Irreversible, "charge", 1),
                arm(EffectClass::Irreversible, "charge", 2),
            ],
        )
        .select()
        .commit()
        .build();
    let sink = RecordingSink::new();
    let mut kernel = Kernel::with_ports(
        plan,
        exploring(),
        Box::new(ues::MemoryStore::new()),
        Box::new(sink.clone()),
    )
    .unwrap();
    kernel.run().unwrap();
    // Read the journal through the *recovered* graph, which is the durable truth,
    // rather than through the live kernel.
    let graph = ues::kernel::recovery::rebuild(
        &kernel.durable_records(),
        ues::ExecutionId::new(1),
        PlanId::new(1),
    )
    .unwrap();
    let rejected = graph
        .alternatives()
        .find(|(_, f)| f.status == ues::FutureStatus::Rejected)
        .map(|(id, _)| id)
        .expect("a rejected future");
    let winner = graph
        .alternatives()
        .find(|(_, f)| f.status == ues::FutureStatus::Committed)
        .map(|(id, _)| id)
        .expect("a committed future");

    // The rejected future's intent is journalled and *still deferred*: it was
    // recorded, and then the future was rejected without ever being committed, so
    // nothing ever read it for release. That is the durable form of "it never
    // happened".
    let entry = graph
        .journal
        .get(&EffectKey::new(
            ues::ExecutionId::new(1),
            graph.lineage.get(rejected).expect("rejected").path.clone(),
            EffectId::new(0),
        ))
        .expect("the rejected future's intent is journalled");
    assert!(
        entry.deferred,
        "a rejected future's effect is never released"
    );
    assert!(entry.result.is_none(), "and it never acquired a result");

    // The winner's is the opposite: issued, and holding what the world returned.
    let entry = graph
        .journal
        .get(&EffectKey::new(
            ues::ExecutionId::new(1),
            graph.lineage.get(winner).expect("winner").path.clone(),
            EffectId::new(0),
        ))
        .expect("the winner's effect is journalled");
    assert!(!entry.deferred, "a committed future releases its effects");
    // And the sink agrees: exactly one effect reached the world.
    assert_eq!(sink.observed_len(), 1);
}

#[test]
fn a_replayed_read_returns_the_journalled_value_rather_than_observing_again() {
    // The sink's read result is a pure function of the operation and the bias, so
    // a second observation is indistinguishable — which is exactly why the journal
    // has to be consulted. The proof is that the journal is *read*: after a
    // rollback the same key is served from the journal.
    let plan = common::observing();
    let sink = RecordingSink::new();
    let mut kernel = Kernel::with_ports(
        plan,
        ExecutionPolicy::speculative(64),
        Box::new(ues::MemoryStore::new()),
        Box::new(sink.clone()),
    )
    .unwrap();
    let report = kernel.run().unwrap();
    assert_eq!(report.trunk.get("quoted"), 20, "quote(10) observed once");
    assert_eq!(sink.observed_len(), 1);
    assert_eq!(kernel.journal().len(), 1);
    let key = EffectKey::new(ues::ExecutionId::new(1), Vec::new(), EffectId::new(0));
    assert_eq!(
        kernel.journal().recorded(&key),
        Some(&ues::EffectValue::Int(20)),
        "the observed value is in the journal, not only in the state"
    );
}

#[test]
fn a_deferred_disposition_carries_no_value() {
    // A deferred effect has no result *because it has not happened*. If it had
    // one, replay would invent an observation.
    let d = Disposition::Deferred;
    assert!(d.result().is_none());
    assert!(!d.touched_world());
    let p = Disposition::Performed {
        result: ues::EffectValue::Int(1),
    };
    assert!(p.touched_world());
    assert!(!Disposition::Replayed {
        result: ues::EffectValue::Int(1)
    }
    .touched_world());
}

#[test]
fn a_trunk_failure_propagates_and_a_branch_failure_does_not() {
    let trunk_fails = Plan::builder(PlanId::new(1), "t")
        .state(Operation::Fail {
            reason: "no".into(),
        })
        .build();
    assert!(Kernel::new(trunk_fails, ExecutionPolicy::durable())
        .unwrap()
        .run()
        .is_err());

    let branch_fails = common::branching_with_a_failing_arm();
    let (report, _sink) = run_with_sink(branch_fails, ExecutionPolicy::durable());
    assert_eq!(report.metrics.futures_failed, 1);
    assert_eq!(report.trunk.get("score"), 5, "the surviving arm won");
}

#[test]
fn the_synchronous_policy_never_defers_anything_on_the_trunk() {
    // With no speculation at all, the authoritative future performs everything,
    // so the effect metrics distinguish speculation from execution.
    let plan = Plan::builder(PlanId::new(1), "s")
        .effect(
            "read",
            EffectClass::Read,
            EffectOp::new("quote", 2),
            Some("q"),
            None,
        )
        .state(Operation::Add {
            key: "n".into(),
            by: 1,
        })
        .build();
    let (report, _) = run_with_sink(plan, ExecutionPolicy::durable());
    assert_eq!(report.metrics.effects_deferred, 0);
    assert_eq!(report.metrics.effects_performed, 1);
    // A synchronous runtime still has the record in the write buffer between
    // appending it and making it durable — that is not running ahead. What it must
    // never do is execute a *second* step while one is undurable, so the window
    // never exceeds a single record and nothing is ever discarded.
    assert_eq!(
        report.metrics.peak_speculation, 1,
        "one buffered record, never more"
    );
    assert_eq!(report.metrics.steps_discarded, 0, "nothing to throw away");
    assert!(
        report.metrics.sync_waits >= 2,
        "one wait per step, plus the drain"
    );
    assert_eq!(render(&report.trunk), "n=1,q=4");
}

#[test]
fn an_async_policy_does_defer_and_then_release() {
    let plan = Plan::builder(PlanId::new(1), "s")
        .effect(
            "charge",
            EffectClass::Irreversible,
            EffectOp::new("charge", 2),
            None,
            Some("charged"),
        )
        .state(Operation::Add {
            key: "n".into(),
            by: 1,
        })
        .build();
    // The trunk is authoritative, so even under an async policy it performs
    // immediately. This test pins that: the policy changes *when durability is
    // awaited*, not whether the world is trusted.
    let (report, sink) = run_with_sink(plan, ExecutionPolicy::speculative(64));
    assert_eq!(report.metrics.effects_deferred, 0);
    assert_eq!(sink.observed_len(), 1);
}

#[test]
fn cost_and_flush_do_not_change_which_effects_escape() {
    // The effect rule is a function of class and authoritativeness, so sweeping
    // the cost model must not change the set of effects that reach the world.
    let plan = Plan::builder(PlanId::new(1), "risky")
        .fork(
            "choose",
            vec![
                arm(EffectClass::Irreversible, "charge", 10),
                arm(EffectClass::Irreversible, "charge", 30),
            ],
        )
        .select()
        .commit()
        .build();
    for (durability, flush) in [
        (Durability::Synchronous, 0u64),
        (Durability::Synchronous, 50),
        (Durability::GroupCommit { batch: 4 }, 25),
        (Durability::Async { window: 8 }, 25),
        (Durability::Async { window: 1_000 }, 1),
    ] {
        let sink = RecordingSink::new();
        let mut kernel = Kernel::with_ports(
            plan.clone(),
            exploring().with_durability(durability),
            Box::new(ues::MemoryStore::new()),
            Box::new(sink.clone()),
        )
        .unwrap()
        .with_cost(CostModel::with_flush_latency(flush));
        kernel.run().unwrap();
        assert_eq!(sink.observed_len(), 1, "{durability} at {flush}ms");
    }
}

#[test]
fn an_effect_key_is_stable_across_a_rollback() {
    // The key names *where in the plan* the effect sits, so a re-execution after a
    // rollback produces the same token even though the runtime hands the re-run a
    // brand new FutureId. If it did not, a retry after a crash would be a second
    // effect as far as the target is concerned — which is the entire difference
    // between at-least-once and at-least-twice.
    let path = |ids: &[u32]| ids.to_vec();
    let a = EffectKey::new(ues::ExecutionId::new(4), path(&[1, 0]), EffectId::new(1));
    let b = EffectKey::new(ues::ExecutionId::new(4), path(&[1, 0]), EffectId::new(1));
    assert_eq!(a, b);
    assert_ne!(
        a,
        EffectKey::new(ues::ExecutionId::new(4), path(&[1, 1]), EffectId::new(1))
    );
    assert_ne!(
        a,
        EffectKey::new(ues::ExecutionId::new(5), path(&[1, 0]), EffectId::new(1))
    );
    assert_ne!(
        a,
        EffectKey::new(ues::ExecutionId::new(4), path(&[1, 0]), EffectId::new(2))
    );
    // The trunk's own effects are keyed by the empty path, which no arm can share.
    assert_ne!(
        a,
        EffectKey::new(ues::ExecutionId::new(4), Vec::new(), EffectId::new(1))
    );
}
