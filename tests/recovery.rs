//! Recovery: a durable ledger prefix must rebuild a valid execution graph, and
//! the graph must be the one the run actually produced.
//!
//! The property is deliberately strong — *the same graph*, not merely a plausible
//! one — because "recovery produced something sensible" is exactly the kind of
//! claim that hides a bug. Every test here compares content addresses, statuses
//! and cursors, not summaries.

mod common;

use common::{branching, durable_policy, exploring, render};
use ues::domain::future::FutureStatus;
use ues::domain::ids::{PlanId, TRUNK};
use ues::domain::plan::Plan;
use ues::domain::Operation;
use ues::kernel::recovery::rebuild;
use ues::kernel::{ExecutionPolicy, Kernel};
use ues::ledger::Payload;
use ues::ports::{CostModel, DurableStore};
use ues::LedgerRecord;

/// Runs a plan and returns its records plus the rebuilt graph.
fn run_and_rebuild(
    plan: Plan,
    policy: ExecutionPolicy,
    cost: CostModel,
) -> (
    ues::kernel::RunReport,
    Vec<LedgerRecord>,
    ues::kernel::ExecutionGraph,
) {
    let mut kernel = Kernel::new(plan.clone(), policy).unwrap().with_cost(cost);
    let report = kernel.run().expect("run");
    let records = kernel.durable_records();
    let graph = rebuild(&records, ues::ExecutionId::new(1), plan.id).expect("rebuild");
    (report, records, graph)
}

#[test]
fn a_clean_run_rebuilds_to_the_same_trunk_state() {
    for (name, plan) in [
        ("linear", common::linear(20)),
        ("branching", branching()),
        ("effects", common::branching_with_irreversible_effects()),
    ] {
        let (report, _records, graph) = run_and_rebuild(plan, durable_policy(), CostModel::ZERO);
        assert!(
            graph.trunk_state().logical_eq(&report.trunk),
            "{name}: recovered {} but committed {}",
            render(&graph.trunk_state()),
            render(&report.trunk)
        );
        assert_eq!(
            graph.trunk, report.trunk_content,
            "{name}: trunk head address"
        );
        graph.validate().expect("a recovered graph must validate");
    }
}

#[test]
fn the_recovered_lineage_matches_the_executed_lineage() {
    let plan = branching();
    let mut kernel = Kernel::new(plan.clone(), exploring()).unwrap();
    let report = kernel.run().unwrap();
    let graph = rebuild(&kernel.durable_records(), ues::ExecutionId::new(1), plan.id).unwrap();
    assert_eq!(graph.lineage.len(), kernel.lineage().len());
    for (id, live) in kernel.lineage().iter() {
        let recovered = graph.lineage.get(*id).expect("every future is recovered");
        assert_eq!(recovered.status, live.status, "future {id} status");
        assert_eq!(recovered.head, live.head, "future {id} head");
        assert_eq!(recovered.base, live.base, "future {id} base");
        assert_eq!(recovered.parent, live.parent, "future {id} parent");
        assert_eq!(recovered.arm, live.arm, "future {id} arm");
        assert_eq!(recovered.steps(), live.steps(), "future {id} step count");
    }
    assert_eq!(graph.authoritative, report.authoritative);
    assert!(graph.owed.is_empty(), "a completed run owes nothing");
}

#[test]
fn every_recovered_cursor_matches_the_cursor_that_was_executed() {
    // `rebuild` has always been checked for statuses, heads and bases — never for
    // cursors. That is the gap a resume walked into: a recovered graph reported
    // the right *state* while pointing every future at the wrong *place*, so
    // continuing it re-ran a node the log had already decided.
    //
    // The property is the same one the rest of this file states: not "recovery
    // produced something sensible", but "recovery produced the same execution".
    for (name, plan) in [
        ("branching", branching()),
        ("effects", common::branching_with_irreversible_effects()),
    ] {
        for policy in [durable_policy(), exploring()] {
            let mut kernel = Kernel::new(plan.clone(), policy).unwrap();
            kernel.run().expect("run");
            let graph = rebuild(&kernel.durable_records(), ues::ExecutionId::new(1), plan.id)
                .expect("rebuild");
            for (id, live) in kernel.lineage().iter() {
                let recovered = graph.cursors.get(id).copied();
                let want = Some(live.cursor);
                assert_eq!(
                    recovered, want,
                    "{name}: future {id} recovered at {:?} but was executed at {:?}",
                    recovered, live.cursor
                );
            }
            // And a future with no record of its own must still resolve: a
            // recovered cursor map missing an entry is a resume that would start
            // that future at nothing.
            for (id, _) in kernel.lineage().iter() {
                assert!(
                    graph.cursors.contains_key(id),
                    "{name}: future {id} has no recovered cursor"
                );
            }
        }
    }
}

#[test]
fn the_recovered_selection_matches_the_recorded_one() {
    let plan = branching();
    let mut kernel = Kernel::new(plan.clone(), exploring()).unwrap();
    let report = kernel.run().unwrap();
    let graph = rebuild(&kernel.durable_records(), ues::ExecutionId::new(1), plan.id).unwrap();
    let selection = report.selection.expect("a selection was made");
    assert_eq!(graph.selected, Some(selection.winner));
    let recovered = graph
        .lineage
        .iter()
        .find(|(id, _)| **id == selection.winner)
        .expect("the winner");
    assert_eq!(recovered.1.status, FutureStatus::Committed);
    for (id, f) in graph.lineage.iter() {
        if *id != TRUNK && *id != selection.winner {
            assert_eq!(
                f.status,
                FutureStatus::Rejected,
                "future {id} must be rejected"
            );
        }
    }
}

#[test]
fn recovery_never_invents_records() {
    // Every prefix of the ledger must rebuild to a graph, and the record count can
    // only grow with the prefix. This is the "no impossible state" property at the
    // coarsest level.
    let plan = branching();
    let mut kernel = Kernel::new(plan.clone(), durable_policy()).unwrap();
    kernel.run().unwrap();
    let records = kernel.durable_records();
    for cut in 0..=records.len() {
        let prefix = &records[..cut];
        match rebuild(prefix, ues::ExecutionId::new(1), plan.id) {
            Ok(graph) => {
                assert!(graph.records <= cut);
                graph
                    .validate()
                    .unwrap_or_else(|e| panic!("cut {cut}: {e}"));
                for (id, f) in graph.lineage.iter() {
                    assert!(
                        graph.states.get(f.head).is_some(),
                        "cut {cut}: future {id} head is not in the store"
                    );
                }
            }
            Err(e) => {
                // The only legitimate refusal is "this is not an execution", i.e. an
                // empty prefix.
                assert_eq!(cut, 0, "cut {cut} was refused: {e}");
            }
        }
    }
}

#[test]
fn an_empty_prefix_is_not_an_execution() {
    let err = rebuild(&[], ues::ExecutionId::new(1), PlanId::new(1)).unwrap_err();
    assert!(err.to_string().contains("not an execution"), "{err}");
}

#[test]
fn a_ledger_for_another_plan_is_refused() {
    let mut kernel = Kernel::new(branching(), durable_policy()).unwrap();
    kernel.run().unwrap();
    let records = kernel.durable_records();
    let err = rebuild(&records, ues::ExecutionId::new(1), PlanId::new(99)).unwrap_err();
    assert!(err.to_string().contains("not plan PlanId(99)"), "{err}");
}

#[test]
fn a_ledger_for_another_execution_is_refused() {
    let mut kernel = Kernel::new(branching(), durable_policy()).unwrap();
    kernel.run().unwrap();
    let records = kernel.durable_records();
    let err = rebuild(&records, ues::ExecutionId::new(7), PlanId::new(1)).unwrap_err();
    assert!(
        err.to_string()
            .contains("expected records for ExecutionId(7)"),
        "{err}"
    );
}

#[test]
fn a_torn_tail_is_dropped_and_the_prefix_still_rebuilds() {
    // The store is the authority on what survived a crash; recovery is only ever
    // handed the durable prefix.
    let plan = common::linear(12);
    let mut kernel = Kernel::new(plan.clone(), durable_policy()).unwrap();
    let report = kernel.run().unwrap();
    let records = kernel.durable_records();
    for cut in 1..records.len() {
        let graph = rebuild(&records[..cut], ues::ExecutionId::new(1), plan.id).unwrap();
        let state = graph.trunk_state();
        assert!(
            state.get("counter") <= report.trunk.get("counter"),
            "a prefix cannot be ahead of the run"
        );
    }
}

#[test]
fn a_gap_in_a_delta_chain_stops_that_future_instead_of_guessing() {
    // Build a ledger, then corrupt one delta's `parent` so the chain no longer
    // continues. Recovery must truncate that future rather than apply a value on
    // top of a state it cannot justify.
    let plan = common::linear(6);
    let mut kernel = Kernel::new(plan.clone(), durable_policy()).unwrap();
    kernel.run().unwrap();
    let mut records = kernel.records().to_vec();
    let target = records
        .iter()
        .position(|r| matches!(r.payload, Payload::Step { .. }))
        .expect("there are steps");
    // A parent far in the past: the chain cannot continue from it.
    if let Payload::Step { delta, .. } = &mut records[target].payload {
        delta.parent = ues::Version::new(999);
    }
    // Either the store rejected the frame, or the future stopped early. Both are
    // correct; producing a *wrong* state is not.
    if let Ok(g) = rebuild(&records, ues::ExecutionId::new(1), plan.id) {
        g.validate()
            .expect("whatever was recovered must be consistent");
        let trunk = g.trunk_state();
        assert!(
            trunk.get("counter") < 6,
            "the corrupted record must not have been applied: {trunk:?}"
        );
    }
}

#[test]
fn recovery_is_idempotent() {
    // Rebuilding the same ledger twice must give the same graph. Recovery runs
    // during every attempt, so a non-deterministic recovery would make the whole
    // system non-deterministic.
    let plan = branching();
    let mut kernel = Kernel::new(plan.clone(), exploring()).unwrap();
    kernel.run().unwrap();
    let records = kernel.durable_records();
    let a = rebuild(&records, ues::ExecutionId::new(1), plan.id).unwrap();
    let b = rebuild(&records, ues::ExecutionId::new(1), plan.id).unwrap();
    assert_eq!(a.trunk, b.trunk);
    assert_eq!(a.trunk_version, b.trunk_version);
    assert_eq!(a.authoritative, b.authoritative);
    assert_eq!(a.lineage.len(), b.lineage.len());
    for (id, f) in a.lineage.iter() {
        let other = b.lineage.get(*id).unwrap();
        assert_eq!(
            (f.head, f.status),
            (other.head, other.status),
            "future {id}"
        );
    }
}

#[test]
fn the_durable_store_and_the_graph_agree_after_a_reopen() {
    // The end-to-end storage story: write to a file, close it, reopen, recover.
    let dir = ues::ledger::store::temp_ledger_dir("graph");
    let path = dir.join("run.ledger");
    let plan = branching();
    {
        let store = ues::FileStore::create(&path).unwrap();
        let mut kernel = Kernel::with_ports(
            plan.clone(),
            durable_policy(),
            Box::new(store),
            Box::new(ues::RecordingSink::new()),
        )
        .unwrap();
        let report = kernel.run().unwrap();
        std::fs::write(path.with_extension("expected"), render(&report.trunk)).unwrap();
    }
    let mut store = ues::FileStore::open(&path).unwrap();
    let scan = store.recover().unwrap();
    assert_eq!(scan.torn_bytes_dropped, 0);
    let raw = store
        .read_from(ues::domain::version::LogPosition::START)
        .unwrap();
    let records: Vec<LedgerRecord> = raw
        .iter()
        .map(|b| LedgerRecord::decode(b).unwrap())
        .collect();
    let graph = rebuild(&records, ues::ExecutionId::new(1), plan.id).unwrap();
    let expected = std::fs::read_to_string(path.with_extension("expected")).unwrap();
    assert_eq!(render(&graph.trunk_state()), expected);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn every_durability_policy_recovers_to_the_same_state() {
    // If the policies change *what* is durable, they change the answer. They must
    // only change *when*.
    let plan = branching();
    let mut seen = Vec::new();
    for durability in [
        ues::Durability::Synchronous,
        ues::Durability::GroupCommit { batch: 1 },
        ues::Durability::GroupCommit { batch: 4 },
        ues::Durability::Async { window: 1 },
        ues::Durability::Async { window: 64 },
    ] {
        let mut kernel = Kernel::new(plan.clone(), exploring().with_durability(durability))
            .unwrap()
            .with_cost(CostModel::with_flush_latency(10));
        let report = kernel.run().unwrap();
        let graph = rebuild(&kernel.durable_records(), ues::ExecutionId::new(1), plan.id).unwrap();
        assert_eq!(
            render(&graph.trunk_state()),
            render(&report.trunk),
            "{durability}"
        );
        seen.push(render(&report.trunk));
    }
    assert!(
        seen.windows(2).all(|w| w[0] == w[1]),
        "every policy must produce the same result: {seen:?}"
    );
}

#[test]
fn a_plan_with_no_fork_recovers_as_a_single_future() {
    let plan = common::linear(5);
    let (_report, _records, graph) = run_and_rebuild(plan, durable_policy(), CostModel::ZERO);
    assert_eq!(graph.lineage.len(), 1);
    assert_eq!(graph.authoritative, TRUNK);
    assert!(graph.alternatives().next().is_none());
    assert_eq!(graph.trunk_state().get("counter"), 5);
}

#[test]
fn a_plan_that_does_nothing_recovers_as_an_empty_execution() {
    // A plan whose only node is a fork over two empty arms still produces a valid
    // graph: two futures, both evaluable, a selection, and a commit.
    let empty = |name: &str| Plan::builder(PlanId::new(2), name).build();
    let plan = Plan::builder(PlanId::new(1), "empty-arms")
        .fork("nothing", vec![empty("a"), empty("b")])
        .select()
        .commit()
        .build();
    let (report, _records, graph) = run_and_rebuild(plan, exploring(), CostModel::ZERO);
    assert_eq!(graph.lineage.len(), 3, "the trunk and two arms");
    assert!(graph.trunk_state().logical_eq(&report.trunk));
    assert_eq!(graph.owed, Vec::new());
}

#[test]
fn a_plan_that_only_commits_nothing_is_still_consistent() {
    let only = |name: &str| Plan::builder(PlanId::new(2), name).build();
    let plan = Plan::builder(PlanId::new(1), "p")
        .state(Operation::Set {
            key: "k".into(),
            value: 3,
        })
        .fork("nothing", vec![only("a"), only("b")])
        .select()
        .commit()
        .build();
    let (report, _records, graph) = run_and_rebuild(plan, exploring(), CostModel::ZERO);
    assert_eq!(graph.trunk_state().get("k"), 3);
    assert!(graph.trunk_state().logical_eq(&report.trunk));
}
