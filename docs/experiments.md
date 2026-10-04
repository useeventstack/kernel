# Experiments

What was measured, how, and what the numbers do and do not support.

Every number below is produced by `cargo run -- bench` on this repository. None
is estimated or carried over from elsewhere.

---

## 1. The question, and why the baseline is the hard part

Does running alternatives as durable futures pay for itself?

The obvious way to answer that — compare a futures runtime against a
straightforward sequential runtime — measures almost nothing, because the
alternative does strictly more work. Three arms executed in sequence to find one
winner costs three times the compute of committing the first one.

So the benchmark compares arms that differ in *one* variable at a time:

| arm | durability | branching |
|---|---|---|
| `sync` | every record durable before the next step | yes |
| `group(8)` | batch 8, then wait | yes |
| `futures` | run ahead up to a window, writer in the background | yes |

All three branch, evaluate and commit. The only difference is *where the fsync
sits*. This is the comparison that makes the result mean anything, and it is the
one most often skipped.

It also means the benchmark is not generous to the futures arm. `group(8)` gets
the batching win for free, with no branching machinery at all, and it takes most
of the improvement. The table below shows exactly how much.

---

## 2. The workload

```
4 arms × 40 state steps each
  arm i:  set score = i;  add work += 1  (×40)
trunk:    add counter += 1
          FORK → 4 arms
          SELECT (highest score)
          COMMIT
          add counter += 100
```

Four arms of equal length, uniform cost, and a score that is written on the first
step of each arm. The winner is arm 4, so `score=4` and `work=40` in the
committed state.

The shape is deliberately uninteresting. The interesting shapes — arms of
unequal cost, more arms, an evaluator that sometimes picks wrong — are listed
under §7 as not shown, because each one changes the useful-work ratio and the
ratio is the argument.

Cost model: 50 µs per record, 20 µs per KiB, and a configurable fsync. The
`useful` column is `steps in the committed state / steps executed`.

---

## 3. The clock is virtual, and that is a limitation

The benchmark runs on a `VirtualClock` driven by a `CostModel`, not wall time.

This is a deliberate choice with a specific consequence. The thing worth
measuring here is the **shape** of the cost: how much sits on the critical path,
how much overlaps, how much is wasted. That shape is a property of the design, not
of this machine's disk, and it is reproducible — running the benchmark twice gives
identical numbers to the last digit.

It is also a real limitation, stated plainly: **these are not throughput numbers
for real hardware.** For those, `ues::ledger::probe` measures the actual store on
the actual machine, and refuses to calibrate against a filesystem where
durability is free — such a number would silently make every downstream result
meaningless.

---

## 4. Results

### 4 arms × 40 steps, 1 worker, 5 ms fsync

```
  arm          wall ms    dur ms   work ms   useful   waste  records     KiB
  sync           902.5     902.5     165.0      24%     76%      178     3.8
  group(8)       192.4      28.4     165.0      24%     76%      178     3.8
  futures        182.3      18.3     165.0      24%     76%      178     3.8
```

The synchronous arm spends its entire wall time parked: every record costs an
fsync on the critical path, 178 of them. `group(8)` cuts that to 28 ms by
amortising 8 records per flush. `futures` reaches 18 ms.

**`group(8)` takes 4.7× of `futures`' 4.95×, and it does so with no branching
machinery whatsoever.** That is the single most important number here. It means
most of what a "durable speculative execution" system appears to win against a
synchronous baseline is *batching*, and batching has nothing to do with
futures. Any report of the sync-to-futures gap as evidence for durable futures
is reporting fsync arithmetic.

The `useful`/`waste` columns are the price: three of four arms are discarded, so
roughly a quarter of the work survives. A latency-only table would hide that
entirely.

### The same workload with more workers

```
2 workers, 5 ms fsync
  arm          wall ms    dur ms   work ms records peak spec
  sync           902.5     902.5     165.0     178        1
  group(8)       233.2     229.2     165.0     178        8
  futures         98.3      16.3     165.0     178       20

4 workers, 5 ms fsync
  arm          wall ms    dur ms   work ms records peak spec
  sync           902.5     902.5     165.0     178        1
  group(8)       233.2     231.2     165.0     178        8
  futures         56.3      15.3     165.0     178       34
```

Two things worth reading off this.

The synchronous arm does not move. It is bounded by `records × fsync` regardless
of how much parallelism is available, because the flush is on its critical path
every time. This is the clearest statement of what the design is buying: not
throughput, but *removing the durability boundary from the critical path*.

And `group(8)` gets slightly **worse** with more workers (192 → 233 ms), because
extra workers contend for the same flush and the group boundary, not the work, is
the constraint. `futures` improves 3.3× across the same range. The two designs
scale differently, and only one of them scales with the thing you would add
workers for.

### The fsync sweep

Time parked on durability, as a share of wall time, 1 worker:

```
    fsync          sync     group(8)      futures   speedup vs sync
      5ms          100%          15%          10%       4.95x
     10ms          100%          98%          16%       9.23x
     20ms          100%          99%          28%      15.72x
     40ms          100%         100%          49%      21.99x
     80ms          100%         100%          90%      22.25x
    160ms          100%         100%          95%      22.25x
    320ms          100%         100%          98%      22.25x
```

This is the table that answers "when is this worth it", and the answer is
narrower than the headline speedup suggests.

**Group commit collapses at 10 ms.** One worker cannot refill a group of eight
faster than the flush takes, so once the flush exceeds the work needed to fill a
group, the arm is 98% durability-bound and batching buys nothing. It is a
technique with a precondition, and the precondition is a *cheap* flush.

**Futures degrades gracefully** — 10%, 16%, 28%, 49%, then saturating near 100%
— because its window is not tied to a group boundary. It can run ahead by
whatever the work allows rather than by whatever a group happens to hold.

**The 22× ceiling is not the design's speedup.** It is the ratio of total flush
time to total work time in this workload. Even with everything overlapped, the
run still pays for every fsync; it just stops paying for them one at a time. Once
the flush dominates completely, both designs are dominated by it and the ratio
saturates.

---

## 5. What the numbers support

A narrow claim, stated at the width the evidence supports:

> Durable futures remove the durability boundary from the critical path. This
> matters when the fsync is long relative to the work available to fill a
> batching group — that is, exactly when group commit stops helping. Below that
> threshold, most of the win is batching and futures adds little. The cost is
> discarded work, which is roughly `(arms − 1) / arms` of everything executed,
> and which is only worth paying when the branches are expensive enough that
> committing the wrong one is worse.

Three tests in `src/benchmark.rs` hold the benchmark to that, because a
benchmark that cannot be caught lying is not a measurement:

- **B1** every arm reaches the same committed state, so the arms differ in cost
  and nothing else;
- **B2** speculation reports the waste it costs, so a speedup cannot be bought by
  not counting the discarded work;
- **B3** the fsync sweep actually moves the cost model.

B3 exists because the first version of the sweep computed a flush latency,
printed it in a table header, and never applied it to the cost model. The result
was seven identical rows that read exactly like a measurement — the most
dangerous kind of wrong benchmark, because it is indistinguishable from a result.

---

## 6. The correctness experiments

These are not performance claims, but they are the reason the design is usable at
all, and the failure matrix is where the work actually went.

`tests/failure_matrix.rs` crashes at every checkpoint of every future — before
and after each step, each effect, each fork, the selection, before/during/after
the commit, after the release, and on both sides of each durability boundary —
across linear and branching plans and under all six durability policies. For each
it asks: did the run finish, is the trunk right, does the log alone reconstruct
it, and did a rejected future perform an irreversible effect.

`tests/kill_process.rs` does the same against a real process. The child dies of
`SIGABRT` with its write buffer deliberately unflushed at around 25 points
spanning both sides of the commit; the parent then treats the file as a corpse,
reads the durable prefix, rebuilds, and finishes the run.

Writing the matrix found seven semantic bugs in the kernel, each of which would
have produced a *plausible* wrong answer rather than an error:

1. A journalled `Read` was never folded into state during recovery — the graph
   validated while the trunk was missing every observation the run had made.
2. The post-step window — a record written but not yet durable, the single most
   important window in speculative execution — was never observed at all.
3. `FailureInjector::at` and `repeating` were the same thing, so a one-shot fault
   looped until the attempt budget died and recovery was never actually tested.
4. Rollback did not truncate the in-memory log, so it retained records a crashed
   process could never leave, including commits it had discarded.
5. Irreversible effects were released after the commit record was *written*
   rather than after it was *durable*, so a crash in that window left a charge at
   the bank for a commit that no longer existed.
6. Idempotency keys were built from a runtime `FutureId`, which a rollback
   invalidates — so the first crash after a retry would double-apply a
   speculative write.
7. Durable cursors were resolved by version alone, so a child with no durable
   work inherited its parent's cursor and resumed inside a sibling's plan.

Plus two in the selection path: the trunk selected and committed over
half-executed arms, and `evaluate` scored futures that had not finished. Both
produced a well-formed selection, a merge and a commit — of a wrong answer.

That list is the actual result of this project. The benchmark says the design is
worth considering in a narrow window; the failure matrix says the interesting
bugs are in the corners, and names them.

---

## 7. Not shown

- Real fsync latency. `ues::ledger::probe` measures it; the tables above use a
  parameter.
- More than four arms, or arms of unequal length. Unequal arms change which
  evaluator policy is right, which is a different experiment.
- A selection the evaluator gets wrong. The merge machinery handles it — the
  rejected arm is discarded — but the *cost* of a bad evaluator is not measured.
- More than one worker *thread*. The scheduler is deterministic virtual time;
  real concurrency would need a different executor and would introduce a source
  of nondeterminism recovery would then have to absorb.
- Contention. Multiple executions on one store, hot state nodes, or a slow world.
- The storage cost of a long run. There is no compaction, so the log grows
  without bound; a run that forks heavily writes a fork record per child, which
  is small, but nothing here bounds the total.
