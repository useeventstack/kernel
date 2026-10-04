# Correctness invariants

Each invariant below is enforced by code, not only checked by a test. Each entry
names the mechanism that enforces it and the test that would fail without it.

236 tests pass, of which three files carry most of the weight:

| file | what it establishes |
|---|---|
| `tests/failure_matrix.rs` | crash at every checkpoint of every future, on linear and branching plans, under every durability policy |
| `tests/kill_process.rs` | the same, against a real `SIGABRT` with an unflushed write buffer |
| `tests/strategy_agreement.rs` | four execution strategies, one outcome, and no write of a rejected future ever offered to the world |

The matrix asks four questions per crash point, in this order, because a later
answer is worthless if an earlier one fails:

1. did the run finish?
2. is the trunk what a failure-free run produces?
3. **does the log alone reconstruct that trunk?**
4. did any irreversible effect of a *rejected* future reach the world?

Question 3 is the one that makes the rest mean anything. Every result is checked
by rebuilding from the durable prefix with `rebuild`, not by inspecting
in-memory state a real crash would have lost. Question 4 is the reason the file
exists rather than a generic "did it recover": a runtime can recover perfectly
and still have done damage.

---

## Branching

### I1. Siblings cannot see each other's state
**Enforced by** content addressing: a child is created at the parent's `head`
node and every step produces a *new* node. There is no shared mutable state to
leak.
**Test** `tests/isolation.rs` — nine tests over a three-arm fork, including
`::every_durability_policy_produces_the_same_result` and
`::converging_arms_share_one_stored_version`.

### I2. A child cannot execute a sibling's plan
**Enforced by** `advance`, which clamps to the future's `arm_end`, and by
`finished`, which tests `cursor >= arm_end` rather than `plan.node(cursor).is_none()`.
**Why the distinction** in a flattened program the node *after* an arm's end is
the next arm's first node, so the weaker test lets a child walk into a sibling's
plan against states it never branched from.
**Test** `tests/isolation.rs::a_future_cannot_reach_another_futures_state` and `::a_sibling_cannot_observe_another_siblings_writes`.

### I3. Forking is O(1) and a fork record is 17 bytes per child
**Enforced by** `ForkedChild { future, arm, root, base }` — four varints and a
node address. State size is not in the record.
**Test** `tests/isolation.rs::forking_copies_eight_bytes_and_no_state`.

---

## State and merge

### I4. A state node's address is a function of its content
**Enforced by** `StateStore::intern`, which hashes on insert. Equal states get
equal addresses; that is what makes "did this future change anything?" an address
comparison.
**Test** `src/domain/node.rs` unit tests.

### I5. The merge is a three-way merge
**Enforced by** `State::three_way_merge(base, ours, theirs)`. A key is taken from
`ours` iff `ours[k] != base[k]`, from `theirs` iff `theirs[k] != base[k]`, and a
key both sides changed differently is a **conflict**.
**Why no write set** the three-way relation subsumes it exactly, and a write-set
bookkeeping bug is a silent wrong answer rather than a loud one.
**Test** `tests/commit.rs`.

### I6. A conflicting commit is refused by default
**Enforced by** `ConflictPolicy::Abort` being `#[default]`. `TakeFuture` exists,
is documented as the weaker option, and is never the default.
**Test** `tests/commit.rs::a_moved_trunk_conflicts_instead_of_being_overwritten`, and
`::the_weaker_policy_commits_and_says_what_it_overwrote` for the named exception.

---

## Effects

### I7. A journalled value is replayed, never recomputed
**Enforced by** `JournalEntry::replay` returning the stored value. The result
lives in the effect delta's `disposition` in the ledger, not in the world and not
only in state.
**Why it matters** an effect re-executed against a world that has moved produces
a second, different answer, and recovery is not deterministic any more.
**Test** `tests/effects.rs::a_replayed_read_returns_the_journalled_value_rather_than_observing_again`.

### I8. A `Read` folds its journalled value into state
**Enforced by** the kernel on the live path **and** by `recovery.rs` on the replay
path, which re-applies `Operation::Set` from the disposition.
**The bug this fixes** recovery originally advanced the head *version* but not the
head *node*, so a recovered graph validated while the trunk was missing every
observation the run had made.
**Test** `tests/failure_matrix.rs::the_failure_free_run_is_the_reference` — the
reference is compared against the rebuilt graph, so a lost read fails it.

### I9. An irreversible effect is issued only after its commit is durable
**Enforced by** `force_durable(trunk_version)` between appending the commit
record and calling `release_owed`.
**The bug this fixes** release ran on "the record is *written*". A crash in that
window rolled the commit back with the charge already at the bank, and no
recovery can undo that.
**Test** `tests/failure_matrix.rs::a_failure_during_the_commit_window_either_did_nothing_or_committed`
checks the direction: `BeforeCommit` and `DuringCommit` must leave the trunk
unchanged; `AfterCommit` and `AfterRelease` must leave it committed.

### I10. An idempotency key is stable under retry
**Enforced by** `EffectKey` being `(execution, arm path, ordinal)`. `Future::path`
is regenerated identically by a re-executed fork.
**The bug this fixes** keys were `(FutureId, ordinal)`. A rollback discards the
futures a fork created and the retry allocates new ids, so a speculative
`IdempotentWrite` re-issued under a fresh key and the target applied it twice.
**Test** `src/domain/effect.rs::a_key_survives_a_retry_that_allocates_a_new_future_id`,
and end-to-end in `tests/kill_process.rs`, which asserts the resumed run charges
exactly what a clean run charges.

### I11. Release debt is derived from the log
**Enforced by** `owed_from_durable_prefix`, called on every rollback: a
`Committed` record with no matching `Released` is a commit the world has not seen.
**Why** a debt held in memory is a debt a restarted process does not have.
**Test** `tests/failure_matrix.rs` at `AfterRelease`.

### I12. A rejected future's effect never reaches the world
**Enforced by** the class rule: `Compensatable` and `Irreversible` are recorded
as intents and released only by a commit.
**Test** `tests/effects.rs` (13 tests) and the fourth question of every matrix
point, in-process and across a real `SIGABRT`.

---

## Selection

### I13. A selection is taken over settled futures only
**Enforced by** `run_select` blocking while any child is unsettled, and
`evaluate::select` filtering to `FutureStatus::Evaluable`.
**The bug this fixes** the trunk selected immediately after forking, over
half-executed arms whose score write had not run. Every score was `None`, the
evaluator picked the first viable-looking arm, and the log recorded a
well-formed selection, a merge and a commit — all of a wrong answer.
**Test** `tests/effects.rs::a_trunk_failure_propagates_and_a_branch_failure_does_not`
and the whole matrix.

### I14. The condition the select waits on is the condition that releases it
**Enforced by** both `run_select` and `unblock_if_done` calling the single
predicate `child_settled`.
**Why** if the select waited for something stricter than what releases it, a child
that failed halfway would never satisfy the first and the trunk would wait
forever.
**Test** the failed-arm test above; a divergence here deadlocks or over-selects.

### I15. Ties are broken deterministically
**Enforced by** `max_by_key(|(id, v)| (*v, Reverse(*id)))` — the lowest future id
wins, every time.
**Why** an evaluator whose tie-break depends on iteration order makes recovery
non-deterministic, and every other property here is downstream of that.
**Test** `tests/recovery.rs::recovery_is_idempotent`.

---

## Durability and rollback

### I16. A future resumes from its durable work, not its speculative work
**Enforced by** `rollback_to_durable(head = durable_head, at = durable_version,
cursor = durable_cursor)`.
**Test** the matrix, plus `a_rollback_never_leaves_a_broken_chain`.

### I17. A durable cursor is resolved per future, not per version
**Enforced by** `absorb` keying its cursor map on `(FutureId, Version)`.
**The bug this fixes** keyed on `Version` alone, a child with no durable work
inherited the *parent's* cursor for its base version, and a rollback resumed it
inside a sibling's plan.
**Test** the matrix; the failure is a wrong final state, not an error.

### I18. A commit moves the trunk's cursor as well as the winner's
**Enforced by** `Payload::cursors_moved`, which reports `(TRUNK, cursor)` for a
`Committed` record.
**The bug this fixes** the committed trunk resumed *before* its own commit and
re-committed, failing on "future is already authoritative".
**Test** `tests/failure_matrix.rs::a_failure_at_every_lifecycle_point_recovers_correctly`.

### I19. Futures created by discarded work are forgotten
**Enforced by** `rollback` removing every future with `base_version > durable`,
and detaching it from its parent's `children`.
**Why** otherwise the re-executed fork creates a *second* set of children beside
the first, and the log ends up with two forks and two commits for one execution.
**Test** the matrix.

### I20. A durable commit is a fact and is never rolled back
**Enforced by** `rollback` skipping any future named in a durable `Committed`
record, and re-deriving `authoritative` from the prefix.
**Why** demoting a `Committed` future to running work means nothing will ever set
it back — the commit already happened, so no later record re-asserts it — and the
run then finishes reporting that nothing committed while the log says otherwise.
**Test** `tests/failure_matrix.rs`, `AfterCommit` and `AfterRelease`.

### I21. The in-memory log is truncated to the durable prefix on rollback
**Enforced by** `self.records.truncate(writer.durable_records)`.
**The bug this fixes** the kernel kept records a crashed process could never
leave, including commits it had logically discarded, and recovery rebuilt a graph
containing them.
**Test** the matrix; `tests/failure_matrix.rs` compares against
`durable_records()`, never `records()`.

### I22. Recovery is given the durable prefix and nothing else
**Enforced by** `Kernel::durable_records` being the only supported input, and
`records()` being documented as inspection-only.
**Test** every matrix assertion, and `tests/kill_process.rs` which cannot do
otherwise — the process holding the records is dead.

### I23. A commit is recomputed, not trusted
**Enforced by** `rebuild` re-running the merge and comparing to the recorded
trunk.
**Why it matters** replaying a commit record *mechanically* succeeds on a log
written by a buggy kernel. Recomputing catches exactly that.
**Test** `tests/recovery.rs` corrupts a record and requires either a clean error
or a consistent graph — never a wrong state.

### I24. A torn tail is a crash, not corruption
**Enforced by** the length-prefixed framing plus a checksum, and `recover()`
dropping an incomplete trailing frame.
**Test** `tests/recovery.rs` truncates a frame at several lengths;
`tests/kill_process.rs` hits it on roughly half its kill points by design.

### I25. An empty prefix is refused plainly
**Enforced by** `rebuild` requiring an `Opened` record. An empty log is not an
execution.
**Test** `tests/recovery.rs::an_empty_prefix_is_not_an_execution`, and
`tests/kill_process.rs::a_kill_before_the_first_record_leaves_nothing_and_that_is_fine`.

### I26. The durability policy changes timing and nothing else
**Enforced by** one kernel with three policies, and the matrix running the same
crash points under all of them.
**Test** `a_failure_under_every_durability_policy_recovers_correctly` — six
policies × five crash points, same required answer.

### I27. Every execution strategy reaches the same outcome
**Enforced by** `ExecutionStrategy` being a *configuration* — a `Durability`, a
sink, and (for `replay`) a source log — and never a different plan. Two
configurations of one runtime cannot disagree about its answer.
**Test** `tests/strategy_agreement.rs::every_strategy_produces_the_same_events_and_the_same_trunk`
— four strategies, one workload, identical trunk content address, event stream
and version. Mutating `observe` to suppress reads makes it fail, so the test is
not vacuous.

### I28. The strategies differ in cost, and the test says so
**Enforced by** the agreement test also requiring different `flushes` and
`Durability` for the issuing pair.
**Why** if the timings matched, the agreement test would be four runs of one
configuration and would prove nothing.
**Test** `::deterministic_and_speculative_agree_on_the_answer_and_differ_on_the_cost`.

### I29. No write of a rejected future is ever *offered* to the world
**Enforced by** `EffectRequest::may_perform_now` being a total function of class
and authority, with no override.
**Test** `::no_effect_of_a_rejected_future_was_ever_offered_to_the_world` asserts
on `RecordingSink::attempts()` — every call to `perform` — and not merely on what
was committed. Also `tests/effects.rs`.

### I30. Suppressing a read is not suppressing a write
**Enforced by** two sink types rather than one flag: `SuppressedSink` (replay,
reports unreadable reads) and `ObservationOnlySink` (observe, performs reads and
suppresses writes).
**Why it matters** a read's result is consumed by the execution. Suppressing it
produces a *different* run, not the same run with fewer side effects — the
agreement test found this the first time it was implemented.
**Test** `tests/strategy_agreement.rs::an_observe_run_evaluates_the_workload_and_changes_nothing`
(reads performed, writes refused) and
`::a_replay_lands_on_the_same_outcome_exactly_when_the_log_is_sufficient`.

### I31. A recovered cursor is where the future actually stopped
**Enforced by** `rebuild` recording a `Selected`/`Committed` record's cursor
against the **executing** future — the winner's parent — because those nodes live
in the parent's plan, not in any arm.
**Why it matters** a graph can report the right *state* while pointing every
future at the wrong *place*, and then continue by re-running a decision the log
had already made. This was a live bug: the check did not exist, and
`Kernel::resume` walked into it on its first test.
**Test** `tests/recovery.rs::every_recovered_cursor_matches_the_cursor_that_was_executed` —
compares every future's recovered cursor with the cursor the kernel actually
executed, under two plans and two durability policies.

### I32. Resuming carries the graph across, and nothing else
**Enforced by** `Kernel::resume` mapping every counter and head from
`ExecutionGraph`, refusing a graph for a different plan or a record count that
disagrees with the graph, and resetting exactly three things: the virtual clock,
the executor slots, and the trunk's `arm_end` (`rebuild` gives the trunk an
unbounded end because it has no plan, and a trunk whose end is `u64::MAX` is a
future the scheduler can never call finished).
**Test** `tests/strategy_agreement.rs::a_replay_lands_on_the_same_outcome_exactly_when_the_log_is_sufficient`
replays every record boundary of a real 26-record log and requires the exact
boundary the log implies, plus `tests/recovery.rs::every_durability_policy_recovers_to_the_same_state`.

---

## The benchmark's own invariants

A benchmark that cannot be caught lying is not a measurement, so
`src/benchmark.rs` carries three:

- **B1.** Every arm reaches the same committed state. A benchmark comparing arms
  that disagree on the answer is measuring something other than cost.
- **B2.** Speculation reports the waste it costs. If the speculative arm reported
  no waste, the waste is not being counted.
- **B3.** The fsync sweep actually moves the cost model. The first version
  computed a flush latency, printed it, and never applied it — producing a table
  of identical rows that read exactly like a result.

---

## Reproducing

```bash
cargo test                                  # all 236
cargo test --test strategy_agreement       # four strategies, one answer
cargo test --test failure_matrix            # the crash matrix
cargo test --test kill_process              # real SIGABRT, ~1 min
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```
