//! The product's central claim, as a test.
//!
//! **For one workload, every execution strategy produces the same durable
//! record stream and the same trunk.** If `deterministic` and `speculative`
//! disagreed about the outcome, the strategy axis would be a lie and the
//! platform would be dishonest with every customer who picked the non-default.
//!
//! The test is deliberately in two halves, because one half alone would be
//! satisfied by a broken system:
//!
//! 1. **Agreement** — same events, same trunk, same selection, for all four
//!    strategies. This is what a customer depends on.
//! 2. **Effect policy** — `deterministic` and `speculative` issue exactly the
//!    same effects; `replay` and `observe` issue none; and no `Compensatable` or
//!    `Irreversible` effect of a *rejected* future was ever offered to the world
//!    at all. Agreement alone would pass if every strategy dropped its effects
//!    on the floor, which is the failure mode a durability shim has.
//!
//! A third test asserts the strategies are genuinely different runtimes — if
//! the timings matched, the first test would be running the same configuration
//! four times and would prove nothing.

mod common;

use common::temp_dir;
use ues::domain::effect::{EffectClass, EffectOp};
use ues::domain::ids::PlanId;
use ues::kernel::evaluate::HighestScore;
use ues::ledger::FileStore;
use ues::ports::DurableStore as _;
use ues::{ExecutionStrategy, MemoryStore, Operation, Plan, StrategyContext, StrategyRun};

/// The effect name of each class in [`order_to_delivery`]. The assertion about
/// escaped effects is written in terms of these names rather than of effect
/// keys, because "a second charge for order X" is the sentence a customer would
/// use, and a test nobody can phrase that way is a test nobody reads.
mod effect {
    /// `Read` — speculatable, journalled.
    pub const QUOTE: &str = "quote_inventory";
    /// `IdempotentWrite` — speculatable, deduplicated by the target.
    pub const RESERVE: &str = "reserve_inventory";
    /// `Compensatable` — deferred until commit, released with its undo.
    pub const NOTIFY: &str = "notify_warehouse";
    /// `Irreversible` — deferred until commit, and the commit is durable first.
    pub const CAPTURE: &str = "capture_payment";
}

/// A contract-shaped workload.
///
/// Three alternative ways to fulfil one order, an evaluator that picks between
/// them, and a commit. It is the day-one contract of ADR-009 expressed in the
/// kernel's vocabulary, because the kernel does not know the word "contract"
/// (ADR-012 §4) and the product maps one onto this shape.
///
/// Every arm performs a read, an idempotent write and — differently — either an
/// irreversible capture or a compensatable notification. That mix is the point:
/// it is what makes "no effect of a rejected future escaped" a real question
/// rather than a formality.
fn order_to_delivery(order: i64) -> Plan {
    let arm = |label: &str, score: i64| {
        Plan::builder(PlanId::new(2), "fulfil")
            .effect(
                "quote inventory",
                EffectClass::Read,
                EffectOp::new(effect::QUOTE, score),
                Some("quoted"),
                None,
            )
            .effect(
                "reserve stock",
                EffectClass::IdempotentWrite,
                EffectOp::new(effect::RESERVE, order),
                None,
                Some("reserved"),
            )
            .effect_named(
                label,
                if label == effect::CAPTURE {
                    EffectClass::Irreversible
                } else {
                    EffectClass::Compensatable
                },
                EffectOp::new(label, order),
                None,
                Some("fulfilled"),
                Some(EffectOp::new(&format!("undo_{label}"), order)),
                ues::domain::plan::Plan::DEFAULT_COST,
            )
            .state(Operation::Set {
                key: "score".into(),
                value: score,
            })
            .build()
    };

    Plan::builder(PlanId::new(1), "order-to-delivery")
        .state(Operation::Set {
            key: "order".into(),
            value: order,
        })
        .fork(
            "fulfil the order",
            vec![
                arm("ship_standard", 10),
                arm(effect::CAPTURE, 20),
                arm(effect::NOTIFY, 15),
            ],
        )
        .select()
        .commit()
        .state(Operation::Add {
            key: "fulfilled".into(),
            by: 1,
        })
        .build()
}

fn evaluator() -> Box<dyn ues::Evaluator> {
    Box::new(HighestScore::new("score"))
}

/// Writes a real, durable run of `plan` and returns the file its log is in.
///
/// Every strategy under test either performs this run or replays it, so all four
/// are measured against the same bytes on a real disk. A strategy compared
/// against a log nobody wrote would be compared against nothing.
fn reference(plan: &Plan, dir: &std::path::Path) -> (StrategyRun, std::path::PathBuf) {
    let path = dir.join("reference.log");
    let run = ExecutionStrategy::Deterministic
        .run(
            plan.clone(),
            StrategyContext::new(
                evaluator(),
                Box::new(FileStore::create(&path).expect("create")),
            ),
        )
        .expect("the reference run");
    (run, path)
}

/// Runs `strategy` on its own store, with `source` as the log a replay rebuilds
/// from.
fn execute(
    strategy: ExecutionStrategy,
    plan: &Plan,
    source: Option<&std::path::Path>,
) -> StrategyRun {
    let mut ctx = StrategyContext::new(evaluator(), Box::new(MemoryStore::new()));
    if let Some(path) = source {
        ctx = ctx.with_source(Box::new(FileStore::open(path).expect("reopen the source")));
    }
    strategy
        .run(plan.clone(), ctx)
        .unwrap_or_else(|e| panic!("{} failed: {e}", strategy.name()))
}

/// The four strategies, with `speculative`'s arm count taken from the plan.
fn all_strategies(arms: usize) -> Vec<ExecutionStrategy> {
    vec![
        ExecutionStrategy::Deterministic,
        ExecutionStrategy::Speculative { arms, window: 8 },
        ExecutionStrategy::Replay,
        ExecutionStrategy::Observe,
    ]
}

/// Names of every effect that reached the world, in order, as `name(args)`.
fn issued(run: &StrategyRun) -> Vec<String> {
    run.issued
        .iter()
        .map(|(_, op)| format!("{}({})", op.name, op.args))
        .collect()
}

fn offered(run: &StrategyRun) -> Vec<String> {
    run.offered
        .iter()
        .map(|(_, op)| format!("{}({})", op.name, op.args))
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Agreement
// ---------------------------------------------------------------------------

#[test]
fn every_strategy_produces_the_same_events_and_the_same_trunk() {
    let dir = temp_dir("agreement");
    let plan = order_to_delivery(8812);

    // The reference: a deterministic run on a real file, with a real fsync. Its
    // log is what a replay will read.
    let (reference, log) = reference(&plan, &dir);

    let runs: Vec<(&str, StrategyRun)> = all_strategies(3)
        .iter()
        .map(|s| {
            (
                s.name(),
                execute(*s, &plan, s.is_replay().then_some(log.as_path())),
            )
        })
        .collect();

    for (name, run) in &runs {
        assert_eq!(
            run.trunk_content,
            reference.trunk_content,
            "{name} reached a different trunk than deterministic: {}",
            run.render()
        );
        assert_eq!(
            run.events,
            reference.events,
            "{name} produced a different durable event stream: {}",
            run.render()
        );
        assert_eq!(
            run.trunk,
            reference.trunk,
            "{name} produced a different state: {}",
            run.render()
        );
        assert_eq!(
            run.trunk_version,
            reference.trunk_version,
            "{name} produced a different version: {}",
            run.render()
        );
        assert_eq!(
            run.durable_records,
            reference.durable_records,
            "{name} wrote a different number of records: {}",
            run.render()
        );
    }

    // And the reference is not vacuous: it decided something.
    assert_eq!(reference.trunk.get("order"), 8812);
    assert_eq!(reference.trunk.get("fulfilled"), 1);
    assert_eq!(reference.trunk.get("score"), 20, "the best arm won");
    let selection = reference.selection.as_ref().expect("a selection was made");
    assert_eq!(selection.scores, vec![Some(10), Some(20), Some(15)]);

    for (name, run) in &runs {
        // A replay of a *finished* run does not select again — the decision is in
        // the log it read. Asserting a winner where there is none would be
        // asserting that the strategy forgot what it already knew.
        let Some(other) = run.selection.as_ref() else {
            assert_eq!(run.replay.as_ref().map(|r| r.records_written), Some(0));
            continue;
        };
        assert_eq!(
            other.winner, selection.winner,
            "{name} chose a different alternative"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_replay_is_built_from_a_durable_log_and_reaches_the_same_trunk() {
    let dir = temp_dir("replay");
    let plan = order_to_delivery(8813);

    let (original, log) = reference(&plan, &dir);
    assert!(original.durable_records > 0);

    let replay = execute(ExecutionStrategy::Replay, &plan, Some(&log));

    let evidence = replay.replay.as_ref().expect("a replay was built");
    assert_eq!(
        evidence.records_replayed, original.durable_records,
        "the replay must start from the whole durable prefix, not a part of it"
    );
    assert_eq!(evidence.scan.torn_bytes_dropped, 0);
    assert_eq!(
        evidence.records_written, 0,
        "the run was already complete, so a replay has nothing left to do"
    );
    assert_eq!(replay.trunk_content, original.trunk_content);
    assert_eq!(replay.events, original.events);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_replay_ignores_a_torn_tail_and_still_reaches_the_same_trunk() {
    use std::io::Write;

    let dir = temp_dir("replay-torn");
    let plan = order_to_delivery(8814);
    let (original, path) = reference(&plan, &dir);

    // A half-written record at the end of the file: the shape a crash mid-write
    // leaves behind, and the thing a replay must not treat as history.
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append to the source");
        f.write_all(&[64, 0, 0, 0, 1, 2, 3, 4, 7, 7, 7])
            .expect("write a torn record");
    }

    let replay = execute(ExecutionStrategy::Replay, &plan, Some(&path));

    let evidence = replay.replay.as_ref().expect("a replay was built");
    assert!(
        evidence.scan.torn_bytes_dropped > 0,
        "the torn tail must be detected, not absorbed: {}",
        replay.render()
    );
    assert_eq!(
        replay.trunk_content,
        original.trunk_content,
        "a torn tail must not change the answer: {}",
        replay.render()
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_replay_lands_on_the_same_outcome_exactly_when_the_log_is_sufficient() {
    // The "log alone is sufficient" property, asked about the `replay` strategy
    // instead of about a restart. It has a precise boundary and this test derives
    // it from the log rather than asserting a number somebody typed:
    //
    //   a replay reproduces the execution **iff every read the winning future made
    //   is already in the journal**.
    //
    // Before that boundary a read of the committed future has no journalled value
    // and a replay is not permitted to go and get one, so the run cannot match —
    // and it says so, rather than folding a fabricated zero into the state.
    let dir = temp_dir("replay-resume");
    let plan = order_to_delivery(8821);
    let (reference, log) = reference(&plan, &dir);

    let payloads = FileStore::open(&log)
        .expect("reopen")
        .read_from(ues::domain::version::LogPosition::START)
        .expect("read the durable prefix");
    let records: Vec<ues::LedgerRecord> = payloads
        .iter()
        .map(|b| ues::LedgerRecord::decode(b).expect("decode"))
        .collect();
    let winner = reference.selection.as_ref().expect("a selection").winner;

    // The cut after which the winner's last observation is durable.
    let boundary = records
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            matches!(
                &r.payload,
                ues::ledger::Payload::Effect { delta, .. }
                    if delta.class == EffectClass::Read && delta.future == winner
            )
        })
        .map(|(i, _)| i + 1)
        .max()
        .expect("the winner observed the world at least once");
    assert!(
        boundary > 1 && boundary < records.len(),
        "cut at {boundary}"
    );

    let mut matched = 0;
    let mut reported = 0;
    for cut in 1..records.len() {
        let run = replay_from(&plan, &payloads[..cut]);
        assert_eq!(run.replay.as_ref().map(|r| r.records_replayed), Some(cut));
        assert!(
            run.issued.is_empty(),
            "replay from {cut} records reached the world: {:?}",
            run.issued
        );
        if cut >= boundary {
            assert_eq!(
                run.trunk_content,
                reference.trunk_content,
                "the log was sufficient at {cut} and the replay disagreed: {}",
                run.render()
            );
            matched += 1;
        } else {
            assert_ne!(
                run.trunk_content,
                reference.trunk_content,
                "the log was NOT sufficient at {cut}, so the replay must not \
                 silently agree: {}",
                run.render()
            );
            assert!(
                run.suppressed.iter().any(|a| a.class == EffectClass::Read),
                "and it must name the read it could not answer: {:?}",
                run.suppressed
            );
            reported += 1;
        }
    }
    assert!(
        matched > 10 && reported >= 3,
        "{matched} matched, {reported} reported"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Rebuilds and continues from an exact prefix of a log, as a crash would leave it.
fn replay_from(plan: &Plan, payloads: &[Vec<u8>]) -> StrategyRun {
    let mut prefix = MemoryStore::new();
    for payload in payloads {
        prefix.append(payload.clone()).expect("append");
    }
    prefix.commit_all().expect("commit the prefix");
    ExecutionStrategy::Replay
        .run(
            plan.clone(),
            StrategyContext::new(evaluator(), Box::new(MemoryStore::new()))
                .with_source(Box::new(prefix)),
        )
        .unwrap_or_else(|e| panic!("replay from {} records failed: {e}", payloads.len()))
}

// ---------------------------------------------------------------------------
// 2. Effect policy
// ---------------------------------------------------------------------------

#[test]
fn the_two_issuing_strategies_issue_exactly_the_same_effects() {
    let dir = temp_dir("issued");
    let plan = order_to_delivery(8815);
    let deterministic = execute(ExecutionStrategy::Deterministic, &plan, None);
    let speculative = execute(
        ExecutionStrategy::Speculative { arms: 3, window: 8 },
        &plan,
        None,
    );

    assert_eq!(
        issued(&deterministic),
        issued(&speculative),
        "deterministic and speculative must let out the same effects"
    );
    assert!(
        !issued(&deterministic).is_empty(),
        "and that set must not be empty, or the test above is vacuous"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_effect_of_a_rejected_future_was_ever_offered_to_the_world() {
    let dir = temp_dir("escaped");
    let plan = order_to_delivery(8816);
    let arms = 3;

    for strategy in [
        ExecutionStrategy::Deterministic,
        ExecutionStrategy::Speculative { arms, window: 8 },
    ] {
        let run = execute(strategy, &plan, None);
        let out = issued(&run);
        let offered = offered(&run);

        // Every arm performs the `IdempotentWrite` reservation, and all three
        // reach the world. That is the effect model's documented position: an
        // idempotent write may be issued while speculative because the *target*
        // deduplicates by key. Each arm has its own key, so in truth a real system
        // would see three reservations — the guarantee the kernel makes is about
        // `Compensatable` and `Irreversible` effects, and this test says so rather
        // than implying more than the kernel claims.
        //
        // Two arms perform the irreversible capture; one wins. Exactly one
        // capture may ever have been offered, and it must be the winner's.
        let captures: Vec<_> = out
            .iter()
            .filter(|s| s.starts_with(effect::CAPTURE))
            .collect();
        assert_eq!(
            captures.len(),
            1,
            "{}: {} captures reached the world: {out:?}",
            strategy.name(),
            captures.len()
        );
        assert_eq!(
            captures[0].as_str(),
            format!("{}(8816)", effect::CAPTURE),
            "{}: the capture that escaped is not the winner's",
            strategy.name()
        );

        // The compensatable notification belonged to an arm that *lost*, so it was
        // never released at all. Zero, not one: a compensation is only owed to the
        // future that won.
        let notifications: Vec<&String> = out
            .iter()
            .filter(|s| s.starts_with(effect::NOTIFY))
            .collect();
        assert!(
            notifications.is_empty(),
            "{}: a rejected future's compensatable effect reached the world: {out:?}",
            strategy.name()
        );

        // The stronger form: a rejected future's deferred effect was never even
        // *offered*. `offered` is every call to `perform`, deduplicated ones
        // included, so this is "never issued", not "never committed".
        let offered_notifications: Vec<&String> = offered
            .iter()
            .filter(|s| s.starts_with(effect::NOTIFY))
            .collect();
        assert!(
            offered_notifications.is_empty(),
            "{}: a rejected future's compensatable effect was offered to the world",
            strategy.name()
        );

        let offered_captures: Vec<String> = offered
            .iter()
            .filter(|s| s.starts_with(effect::CAPTURE))
            .cloned()
            .collect();
        assert_eq!(
            offered_captures,
            vec![format!("{}(8816)", effect::CAPTURE)],
            "{}: a rejected future's irreversible effect was offered to the world",
            strategy.name()
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn replay_and_observe_contact_nothing_but_still_decide_the_same_thing() {
    let dir = temp_dir("suppressed");
    let plan = order_to_delivery(8817);
    let (reference, log) = reference(&plan, &dir);

    for strategy in [ExecutionStrategy::Replay, ExecutionStrategy::Observe] {
        let run = execute(
            strategy,
            &plan,
            strategy.is_replay().then_some(log.as_path()),
        );
        assert!(
            run.issued.iter().all(|(_, op)| op.name == effect::QUOTE),
            "{} issued something that is not a read: {:?}",
            strategy.name(),
            run.issued
        );
        assert!(
            run.suppressed.iter().all(|a| a.class.is_write()),
            "{} refused something that is not a write: {:?}",
            strategy.name(),
            run.suppressed
        );
        assert_eq!(
            run.trunk_content,
            reference.trunk_content,
            "{} changed the answer while changing nothing else: {}",
            strategy.name(),
            run.render()
        );
        assert_eq!(run.events, reference.events, "{}", strategy.name());
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_observe_run_evaluates_the_workload_and_changes_nothing() {
    let dir = temp_dir("observe");
    let plan = order_to_delivery(8818);
    let run = execute(ExecutionStrategy::Observe, &plan, None);
    let offered = offered(&run);

    // The winner's deferred capture *is* released to the sink once its commit is
    // durable — the runtime is doing its job. What makes this an `observe` run is
    // that the sink lets none of it out.
    assert!(
        run.suppressed.iter().any(|a| a.op.name == effect::CAPTURE),
        "the committed arm's write should still be released to a sink: {:?}",
        run.suppressed
    );

    // No write of any class reached the world.
    assert!(
        run.issued.iter().all(|(_, op)| op.name == effect::QUOTE),
        "observe wrote to the world: {:?}",
        run.issued
    );
    assert!(
        run.suppressed.iter().all(|a| a.class.is_write()),
        "observe refused a read, which is not what observe means: {:?}",
        run.suppressed
    );

    // The read *was* performed, and returned the real value. An `observe` run that
    // fabricated its reads would compute a different answer and could not be
    // compared with any other strategy.
    assert!(
        run.issued.iter().any(|(_, op)| op.name == effect::QUOTE),
        "the read was not evaluated: {offered:?}"
    );
    // The value is the one a deterministic run observed, not a fabricated zero. A
    // suppressed read folds in as `Unit`, which reads as 0, so "not zero" and
    // "equal to the other strategy" together say what a comment cannot.
    let deterministic = execute(ExecutionStrategy::Deterministic, &plan, None);
    assert_ne!(
        run.trunk.get("quoted"),
        0,
        "a suppressed read folded in as zero"
    );
    assert_eq!(
        run.trunk.get("quoted"),
        deterministic.trunk.get("quoted"),
        "observe did not observe what the world actually returned"
    );

    // And the arm that lost is untouched: its compensatable write was never even
    // offered, let alone issued.
    assert!(
        !offered.iter().any(|s| s.starts_with(effect::NOTIFY)),
        "a rejected future's compensatable effect was offered: {offered:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// 3. The strategies are genuinely different
// ---------------------------------------------------------------------------

#[test]
fn deterministic_and_speculative_agree_on_the_answer_and_differ_on_the_cost() {
    let dir = temp_dir("cost");
    let plan = order_to_delivery(8819);
    let deterministic = execute(ExecutionStrategy::Deterministic, &plan, None);
    let speculative = execute(
        ExecutionStrategy::Speculative { arms: 3, window: 8 },
        &plan,
        None,
    );

    assert_eq!(deterministic.trunk_content, speculative.trunk_content);
    assert_eq!(deterministic.events, speculative.events);

    assert!(
        deterministic.metrics.records_durable >= speculative.metrics.records_durable,
        "a synchronous run makes every record durable; run-ahead makes them durable later"
    );
    assert_ne!(
        deterministic.metrics.flushes, speculative.metrics.flushes,
        "if the two strategies flushed the same number of times, the agreement \
         above would be four runs of one configuration and would prove nothing"
    );
    assert_eq!(
        deterministic.durability,
        ues::Durability::Synchronous,
        "the default is the boring one"
    );
    assert_eq!(speculative.durability, ues::Durability::Async { window: 8 });
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_strategy_chosen_by_name_from_storage_runs() {
    // The product stores a string; this is the path from that string to a run.
    let dir = temp_dir("byname");
    let plan = order_to_delivery(8820);
    for name in ExecutionStrategy::names() {
        let mut strategy = ExecutionStrategy::from_name(name).expect("a known name");
        if let ExecutionStrategy::Speculative { window, .. } = strategy {
            strategy = ExecutionStrategy::Speculative { arms: 3, window };
        }
        let (_reference, log) = reference(&plan, &dir);
        let run = execute(
            strategy,
            &plan,
            strategy.is_replay().then_some(log.as_path()),
        );
        assert_eq!(
            run.trunk.get("fulfilled"),
            1,
            "{name} did not fulfil the order: {}",
            run.render()
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}
