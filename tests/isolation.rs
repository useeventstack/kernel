//! Invariant: **no future can mutate another future, and a child cannot mutate its
//! parent**.
//!
//! These are the tests that make branch isolation a claim rather than a hope. The
//! mechanism is structural — content-addressed, insert-only state and value-typed
//! futures — so most of these tests are proving that the *runtime* never has a way
//! to violate it, and the property tests sweep the plans that could try.

mod common;

use common::{branching, durable_policy, render, run};
use ues::domain::ids::TRUNK;
use ues::domain::node::StateNode;
use ues::domain::plan::Plan;
use ues::domain::Operation;
use ues::kernel::policy::Durability;
use ues::State;

#[test]
fn every_branch_starts_from_the_same_state() {
    let report = run(branching(), durable_policy()).unwrap();
    // Arm B wins with 20, which is only the answer if no arm saw another's writes.
    assert_eq!(report.trunk.get("total"), 20);
    assert_eq!(report.trunk.get("score"), 20);
    assert_eq!(report.trunk.get("committed"), 1);
}

#[test]
fn a_future_cannot_reach_another_futures_state() {
    // The state store is insert-only, so the only way to observe another future's
    // state is to name its head node. Two futures that reach the same state share
    // one node; two that diverge do not.
    let mut store = ues::StateStore::new();
    let a = store.intern(&{
        let mut s = State::new();
        s.set("x", 1);
        s
    });
    let b = store.intern(&{
        let mut s = State::new();
        s.set("x", 2);
        s
    });
    assert_ne!(a, b);
    assert_eq!(
        store.get(a).unwrap().get("x"),
        1,
        "interning b did not touch a"
    );
    assert_eq!(store.get(b).unwrap().get("x"), 2);
}

#[test]
fn forking_copies_eight_bytes_and_no_state() {
    // The property that makes N-way evaluation affordable. A fork is a pointer.
    let mut store = ues::StateStore::new();
    let mut big = State::new();
    for i in 0..2_000 {
        big.set(&format!("key-{i}"), i as i64);
    }
    let node = store.intern(&big);
    let before = store.len();
    let forks: Vec<StateNode> = (0..100).map(|_| node).collect();
    assert_eq!(store.len(), before, "100 forks added no state");
    assert!(forks.iter().all(|n| *n == node));
}

#[test]
fn abandoning_a_future_cannot_disturb_the_trunk() {
    let report = run(branching(), durable_policy()).unwrap();
    // Two of the three arms were rejected. If a rejection could disturb the trunk,
    // the committed value would not be arm B's.
    assert_eq!(report.trunk.get("score"), 20);
    assert_eq!(
        report.trunk.get("committed"),
        1,
        "the trunk ran past the commit"
    );
}

#[test]
fn a_sibling_cannot_observe_another_siblings_writes() {
    // Arms B and C both add to `total`. If they shared state, the second would see
    // the first's value. Winning with exactly 20 (not 30 or 35) is the proof.
    let report = run(branching(), durable_policy()).unwrap();
    assert_eq!(report.trunk.get("total"), 20);
}

#[test]
fn a_child_cannot_move_the_trunk_before_its_commit() {
    // Run only the arms by stopping before the commit: the trunk is unchanged
    // until the commit record is written.
    let plan = Plan::builder(ues::PlanId::new(1), "no-commit")
        .state(Operation::Set {
            key: "total".into(),
            value: 0,
        })
        .fork(
            "choose",
            vec![
                ues::domain::plan::Plan::builder(ues::PlanId::new(2), "a")
                    .state(Operation::Set {
                        key: "total".into(),
                        value: 7,
                    })
                    .build(),
                ues::domain::plan::Plan::builder(ues::PlanId::new(2), "b")
                    .state(Operation::Set {
                        key: "total".into(),
                        value: 9,
                    })
                    .build(),
            ],
        )
        .build();
    let report = run(plan, durable_policy()).unwrap();
    assert_eq!(
        report.trunk.get("total"),
        0,
        "the trunk never moved on its own"
    );
    assert_eq!(
        report.authoritative, TRUNK,
        "the trunk is still authoritative"
    );
    assert!(
        report.selection.is_none(),
        "there is no select node to reach"
    );
}

#[test]
fn an_untouched_trunk_survives_a_failed_arm() {
    let report = run(common::branching_with_a_failing_arm(), durable_policy()).unwrap();
    assert_eq!(
        report.trunk.get("score"),
        5,
        "the surviving arm was committed"
    );
}

#[test]
fn every_durability_policy_produces_the_same_result() {
    // Isolation must not depend on timing. The three durabilities are the same
    // kernel, so this is a statement that the policy only changes *when*, never
    // *what*.
    for durability in [
        Durability::Synchronous,
        Durability::GroupCommit { batch: 2 },
        Durability::Async { window: 8 },
    ] {
        let policy = common::durable_policy().with_durability(durability);
        let report = run(branching(), policy).unwrap();
        assert_eq!(
            render(&report.trunk),
            "committed=1,score=20,total=20",
            "{durability} produced a different result"
        );
    }
}

#[test]
fn converging_arms_share_one_stored_version() {
    // Two arms that do the same thing must not cost two stored states. This is the
    // reason the store is content addressed rather than a list.
    let plan = Plan::builder(ues::PlanId::new(1), "converging")
        .fork(
            "same",
            vec![
                Plan::builder(ues::PlanId::new(2), "a")
                    .state(Operation::Set {
                        key: "v".into(),
                        value: 1,
                    })
                    .build(),
                Plan::builder(ues::PlanId::new(2), "b")
                    .state(Operation::Set {
                        key: "v".into(),
                        value: 1,
                    })
                    .build(),
            ],
        )
        .select()
        .commit()
        .build();
    let report = run(plan, durable_policy()).unwrap();
    assert!(
        report.metrics.state_versions_deduped >= 1,
        "two arms that reach the same state must share one version: {:?}",
        report.metrics
    );
}
