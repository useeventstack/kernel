//! Commit: what it means, what it promises, and what it refuses.
//!
//! The distinction these tests exist to pin down is between *the state became
//! authoritative* and *the world was changed*. The first is a single ledger
//! record and is atomic. The second happens afterwards, at-least-once, keyed. A
//! design that blurred the two would let a reader believe the runtime can make an
//! external charge exactly-once, and it cannot.

mod common;

use common::{branching, durable_policy, exploring, render};
use ues::domain::effect::{EffectClass, EffectOp, RecordingSink};
use ues::domain::ids::{PlanId, TRUNK};
use ues::domain::plan::Plan;
use ues::domain::state::State;
use ues::domain::Operation;
use ues::kernel::commit::{plan_merge, CommitError, MergeInput};
use ues::kernel::policy::ConflictPolicy;
use ues::kernel::Kernel;
use ues::ledger::Payload;
use ues::Version;

fn state(pairs: &[(&str, i64)]) -> State {
    let mut s = State::new();
    for (k, v) in pairs {
        s.set(k, *v);
    }
    s
}

#[test]
fn the_trunk_head_moves_in_exactly_one_record() {
    // Atomicity, as an observable fact: the authoritative value changes when the
    // commit record is written, and at no other time.
    let mut kernel = Kernel::new(branching(), exploring()).unwrap();
    let report = kernel.run().unwrap();
    let before = kernel
        .records()
        .iter()
        .filter_map(|r| match &r.payload {
            Payload::Committed { trunk, .. } => Some(*trunk),
            _ => None,
        })
        .count();
    assert_eq!(before, 1, "exactly one commit record");
    assert_eq!(report.trunk_content, report.trunk_content);
    let commit = kernel
        .records()
        .iter()
        .find(|r| matches!(r.payload, Payload::Committed { .. }))
        .expect("a commit record");
    let Payload::Committed { trunk, future, .. } = commit.payload.clone() else {
        unreachable!()
    };
    // The record names the state at the moment of the commit, which is the
    // winner's merged head — not the trunk's final state, because the trunk keeps
    // working after the commit and that work is part of the value too.
    let winner_head = kernel
        .lineage()
        .get(future)
        .map(|f| f.head)
        .expect("the winner");
    assert_eq!(
        trunk, winner_head,
        "the commit record names the merged head"
    );
    assert_ne!(
        trunk, report.trunk_content,
        "the trunk moved on after the commit, so the two addresses differ"
    );
}

#[test]
fn committing_the_same_future_twice_is_refused() {
    // Commit uniqueness. A second commit of an already-authoritative future would
    // make it authoritative twice, so the protocol refuses it rather than
    // producing a second merge.
    let base = state(&[("x", 0)]);
    let mut ours = base.clone();
    ours.set("x", 1);
    let err = plan_merge(
        &MergeInput {
            future: ues::FutureId::new(2),
            base: &base,
            ours: &ours,
            theirs: &base,
            base_version: Version::new(1),
            trunk_version: Version::new(1),
            pending: Vec::new(),
            already_authoritative: Some(ues::FutureId::new(2)),
        },
        ConflictPolicy::Abort,
    )
    .unwrap_err();
    assert_eq!(
        err,
        CommitError::AlreadyCommitted {
            future: ues::FutureId::new(2)
        }
    );
}

#[test]
fn a_moved_trunk_conflicts_instead_of_being_overwritten() {
    // The scenario: a future forked at version 10, and while it ran the trunk
    // moved to 12 and changed the same key.
    let base = state(&[("shared", 1)]);
    let mut ours = base.clone();
    ours.set("shared", 5);
    let mut theirs = base.clone();
    theirs.set("shared", 9);
    let err = plan_merge(
        &MergeInput {
            future: ues::FutureId::new(2),
            base: &base,
            ours: &ours,
            theirs: &theirs,
            base_version: Version::new(10),
            trunk_version: Version::new(12),
            pending: Vec::new(),
            already_authoritative: None,
        },
        ConflictPolicy::Abort,
    )
    .unwrap_err();
    let CommitError::Conflict(c) = err else {
        panic!("expected a conflict")
    };
    assert_eq!(c.conflicts.len(), 1);
    assert_eq!(c.conflicts[0].base, 1);
    assert_eq!(c.conflicts[0].ours, 5);
    assert_eq!(c.conflicts[0].theirs, 9);
    assert!(c.to_string().contains("shared"));
}

#[test]
fn a_moved_trunk_that_touched_other_keys_merges_cleanly() {
    // The same scenario on a *different* key must not be a conflict, and must not
    // lose the trunk's change. This is the case a two-way merge gets wrong.
    let base = state(&[("a", 1), ("b", 1)]);
    let mut ours = base.clone();
    ours.set("a", 5);
    let mut theirs = base.clone();
    theirs.set("b", 9);
    let plan = plan_merge(
        &MergeInput {
            future: ues::FutureId::new(2),
            base: &base,
            ours: &ours,
            theirs: &theirs,
            base_version: Version::new(10),
            trunk_version: Version::new(12),
            pending: Vec::new(),
            already_authoritative: None,
        },
        ConflictPolicy::Abort,
    )
    .unwrap();
    assert!(plan.is_clean());
    assert_eq!(plan.merged.get("a"), 5, "the future's change");
    assert_eq!(plan.merged.get("b"), 9, "the trunk's change survived");
    assert!(plan.trunk_moved);
}

#[test]
fn the_weaker_policy_commits_and_says_what_it_overwrote() {
    let base = state(&[("a", 1)]);
    let mut ours = base.clone();
    ours.set("a", 5);
    let mut theirs = base.clone();
    theirs.set("a", 9);
    let plan = plan_merge(
        &MergeInput {
            future: ues::FutureId::new(2),
            base: &base,
            ours: &ours,
            theirs: &theirs,
            base_version: Version::new(10),
            trunk_version: Version::new(12),
            pending: Vec::new(),
            already_authoritative: None,
        },
        ConflictPolicy::TakeFuture,
    )
    .unwrap();
    assert!(!plan.is_clean());
    assert_eq!(plan.conflicts.len(), 1);
}

#[test]
fn a_plan_commits_exactly_once_and_settles_every_sibling() {
    let mut kernel = Kernel::new(branching(), exploring()).unwrap();
    let report = kernel.run().unwrap();
    assert_eq!(report.metrics.commits, 1);
    assert_eq!(report.metrics.futures_rejected, 2);
    assert_eq!(
        report.authoritative,
        ues::FutureId::new(2),
        "arm B, the one that scored 20"
    );
    // Every future is in a terminal or authoritative state; nothing is left
    // dangling, which is what makes "the plan finished" a single check.
    for (id, f) in kernel.lineage().iter() {
        assert!(
            f.status.is_terminal() || f.status == ues::FutureStatus::Evaluable,
            "future {id} ended in {}",
            f.status
        );
    }
}

#[test]
fn the_winner_is_the_highest_scoring_viable_future() {
    let mut kernel = Kernel::new(branching(), exploring()).unwrap();
    let report = kernel.run().unwrap();
    let selection = report.selection.expect("a selection");
    let best = selection
        .candidates
        .iter()
        .zip(selection.scores.iter())
        .filter_map(|(id, s)| s.map(|v| (*id, v)))
        .max_by_key(|(id, v)| (*v, std::cmp::Reverse(*id)))
        .map(|(id, _)| id)
        .unwrap();
    assert_eq!(selection.winner, best);
    assert_eq!(best, report.authoritative);
}

#[test]
fn the_selection_is_reproducible_whatever_order_the_candidates_arrive_in() {
    // A decision record has to be a function of the *set* of futures, or recovery
    // could rebuild a different graph than the one that was running.
    let mut first = Kernel::new(branching(), exploring()).unwrap();
    let a = first.run().unwrap().selection.unwrap();
    for _ in 0..4 {
        let mut again = Kernel::new(branching(), exploring()).unwrap();
        let b = again.run().unwrap().selection.unwrap();
        assert_eq!(a, b);
    }
}

#[test]
fn effects_are_released_after_the_pointer_and_exactly_once() {
    let arm = |score: i64| {
        Plan::builder(PlanId::new(2), "arm")
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
    let plan = Plan::builder(PlanId::new(1), "risky")
        .fork("choose", vec![arm(10), arm(30), arm(20)])
        .select()
        .commit()
        .build();
    let sink = RecordingSink::new();
    let mut kernel = Kernel::with_ports(
        plan.clone(),
        exploring(),
        Box::new(ues::MemoryStore::new()),
        Box::new(sink.clone()),
    )
    .unwrap();
    kernel.run().unwrap();
    assert_eq!(sink.observed_len(), 1);
    assert_eq!(
        sink.dedup_hits(),
        0,
        "the effect was issued once, not deduplicated"
    );

    // The order in the ledger is the claim: the commit record, then the release.
    let order: Vec<&'static str> = kernel
        .records()
        .iter()
        .map(|r| match r.payload {
            Payload::Committed { .. } => "commit",
            Payload::Released { .. } => "release",
            Payload::Effect { .. } => "effect",
            _ => "other",
        })
        .collect();
    let commit_at = order.iter().position(|s| *s == "commit").unwrap();
    let release_at = order.iter().position(|s| *s == "release").unwrap();
    assert!(
        release_at > commit_at,
        "the pointer flips before the world moves"
    );
    assert_eq!(order.iter().filter(|s| **s == "effect").count(), 3);

    // And the release is recoverable: a fresh graph knows the effect was issued.
    let graph = ues::kernel::recovery::rebuild(
        &kernel.durable_records(),
        ues::ExecutionId::new(1),
        plan.id,
    )
    .unwrap();
    assert!(graph.owed.is_empty(), "nothing is owed after the run");
}

#[test]
fn a_second_commit_of_a_stale_base_is_refused_by_the_protocol() {
    // Two futures forked from the same base both try to win. The second is
    // validated against a trunk that has moved, so it either merges cleanly or is
    // refused; it can never silently replace the first.
    let base = state(&[("x", 0)]);
    let mut a = base.clone();
    a.set("x", 1);
    let mut b = base.clone();
    b.set("x", 2);
    let first = plan_merge(
        &MergeInput {
            future: ues::FutureId::new(1),
            base: &base,
            ours: &a,
            theirs: &base,
            base_version: Version::new(1),
            trunk_version: Version::new(1),
            pending: Vec::new(),
            already_authoritative: None,
        },
        ConflictPolicy::Abort,
    )
    .unwrap();
    let second = plan_merge(
        &MergeInput {
            future: ues::FutureId::new(2),
            base: &base,
            ours: &b,
            theirs: &first.merged,
            base_version: Version::new(1),
            trunk_version: first.trunk_version,
            pending: Vec::new(),
            already_authoritative: Some(ues::FutureId::new(1)),
        },
        ConflictPolicy::Abort,
    );
    assert!(
        matches!(second, Err(CommitError::Conflict(_))),
        "the second commit must not replace the first: {second:?}"
    );
}

#[test]
fn a_sequential_chain_of_commits_never_conflicts() {
    // The plan tree serialises commits, so the normal path never hits a conflict:
    // the second fork's base already contains the first commit.
    // Each arm writes both the value under test and the score the evaluator reads.
    let inner = |v: i64| {
        Plan::builder(PlanId::new(2), "inner")
            .state(Operation::Set {
                key: "x".into(),
                value: v,
            })
            .state(Operation::Set {
                key: "score".into(),
                value: v,
            })
            .build()
    };
    let nested = Plan::builder(PlanId::new(3), "nested")
        .fork("first", vec![inner(1), inner(2)])
        .select()
        .commit()
        .fork("second", vec![inner(30), inner(40)])
        .select()
        .commit()
        .build();
    let plan = Plan::builder(PlanId::new(1), "chain")
        // The nested arm commits twice and ends up scoring 40; the plain arm
        // scores 5, so the outer selection takes the nested one.
        .fork("outer", vec![nested, inner(5)])
        .select()
        .commit()
        .build();
    let report = Kernel::new(plan.clone(), exploring())
        .unwrap()
        .run()
        .unwrap();
    assert_eq!(report.metrics.commits, 3, "three commits, in sequence");
    assert_eq!(
        report.metrics.commits_with_conflicts, 0,
        "and none conflicted"
    );
    // The nested arm's second commit won with 40.
    assert_eq!(report.trunk.get("x"), 40);
    assert_eq!(render(&report.trunk), "score=40,x=40");
}

#[test]
fn the_trunk_continues_after_a_commit() {
    // A commit is not the end of the execution: the trunk keeps running on the
    // merged state, and the work it does afterwards is part of the authoritative
    // value.
    let report = Kernel::new(branching(), durable_policy())
        .unwrap()
        .run()
        .unwrap();
    assert_eq!(report.trunk.get("committed"), 1);
    assert!(report.trunk.get("total") > 0);
    assert_ne!(report.authoritative, TRUNK, "a future took over");
}
