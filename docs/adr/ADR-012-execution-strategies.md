# ADR-012 — `ExecutionStrategy`: the execution axis, not a build flag

**Date** 2026-09-29 · **Status** accepted · **Scope** kernel + product
**Relates to** ADR-001, ADR-002, ADR-003, ADR-004, ADR-006, ADR-008

## Context

The product's brief for this sprint is that _how a contract is executed is a
choice the customer makes_: `deterministic`, `speculative`, `replay`, or
`observe`, selected per contract, with one non-negotiable property behind all of
it —

> for a given contract, every strategy must produce the same contract outcome
> and the same events.

There is a real risk in this, and it is the risk this ADR exists to close. A
platform with a `mode` field tends to grow one default path and three thinner
ones, and the property above is exactly the thing that quietly stops being
true. The question is whether the kernel already has the seam that makes four
genuinely different strategies cheap, or whether the seam has to be cut.

It does. Every strategy is a choice of four values, and all four are already
ports:

| what a strategy chooses              | port it chooses                   |
| ------------------------------------ | --------------------------------- |
| where bytes become durable           | [`DurableStore`] + [`Durability`] |
| which alternatives survive           | [`Evaluator`]                     |
| what is allowed to reach the world   | [`EffectSink`]                    |
| how much may run ahead of durability | [`Durability`]                    |

Two existing facts do the load-bearing work:

1. **`Evaluator::score` returning `None` _is_ exclusion.** A policy constraint is
   an evaluator that refuses; evaluation and admissibility are one mechanism, so
   filtering a future out of a speculative run needs no new machinery.
2. **`RecordingSink::observed()` records what reached the world**, not what was
   merely committed. A test can therefore assert an effect of a rejected future
   was _never issued_.

## Decision

### 1. One type, four variants, in the kernel

`src/strategy.rs` defines:

```rust
pub enum ExecutionStrategy {
    Deterministic,
    Speculative { arms: usize, window: usize },
    Replay,
    Observe,
}
```

It is **not** a mode of the kernel. It is a _total description_ of a kernel
configuration, and it maps onto the existing ports with no new machinery:

| strategy        | `Durability`       | sink                  | store                         |
| --------------- | ------------------ | --------------------- | ----------------------------- |
| `deterministic` | `Synchronous`      | caller's              | caller's                      |
| `speculative`   | `Async { window }` | caller's              | caller's                      |
| `replay`        | `Synchronous`      | `SuppressedSink`      | rebuilt from a durable prefix |
| `observe`       | `Synchronous`      | `ObservationOnlySink` | caller's                      |

The evaluator is the **caller's** for all four, and that is not an oversight: a
strategy that also chose the evaluator could make two strategies agree by scoring
them the same way, which would make the agreement test prove nothing.

`Speculative { arms }` is **validated against the plan**: it refuses to run a
plan whose fork has a different number of arms. A strategy parameter that
silently disagreed with the workload would be a knob nobody can reason about,
which is the exact failure `kernel/policy.rs` was written to avoid.

### 2. A strategy is a configuration, not a different program — and the brief's

"no forking" is corrected here

The sprint brief describes `deterministic` as "one line of work … **no
forking**". Taken literally that is incompatible with the agreement property: a
plan with alternatives, executed with one alternative, is a _different program_,
and nothing guarantees it picks the same arm as an execution that evaluates all
of them. The two strategies would agree by luck, and luck is not a property.

**Chosen:** a strategy never changes the plan. It changes the _timing_ and the
_effect policy_. `deterministic` runs the same arms and the same evaluator, with
every record durable before the next step and no run-ahead. `speculative` runs
the same arms with the same evaluator and overlaps them.

This is already the kernel's own position — `Durability`'s doc comment says the
three variants are _"the same runtime with different timing, not different
runtimes"_, and `AGENTS.md` repeats it. The brief's table compressed "no
forking" from "no speculative _run-ahead_", which is what `Durability` controls.
The correction is recorded here rather than made silently.

**The consequence that makes this worth doing:** agreement is then structural,
and the cross-strategy test is a _check_ of a property rather than the property
itself. Two runtimes that disagree are a bug we can detect; one runtime with two
configurations cannot disagree about its answer, only about its cost.

### 3. "Same events" means contract events, not external effects

The brief says every strategy must produce _"the same contract outcome and the
same events"_. Read as "the same effects reached the world", the property is
**false by definition** — `replay` is defined as _"external effects suppressed"_
and `observe` as _"takes no action of its own"_. Two of the four strategies are
specified to let nothing out. The brief cannot mean that and mean the four
strategies at once.

**Chosen — two properties, asserted separately, both required:**

- **P1 (contract agreement, all four strategies).** The _contract outcome_ is
  the same: the same trunk state, the same content address, the same event
  stream. This is `StrategyRun::agreement()`.
- **P2 (effect policy, per strategy, by design).** `deterministic` and
  `speculative` issue _exactly the same_ effects. `replay` and `observe` issue
  **no write of any class**, and the tests assert that too.

P1 is the product's claim. P2 is what makes `replay` and `observe` useful rather
than broken. Asserting only P1 would let a sink that drops everything pass; that
is the failure mode a durability shim has, and it must not be reachable from
here.

### 3a. Suppressing a read and suppressing a write are different operations

The first implementation of this ADR used one suppressing sink for both `replay`
and `observe`, and **the agreement test caught it**: `observe` reached a
different trunk from `deterministic`.

The cause is worth stating precisely, because it is a general rule and not a
detail of this code:

> A **read has a result the execution consumes**. A **write does not**.
> Suppressing a read does not produce "the same run with fewer side effects" — it
> produces a _different run_, whose state is computed from a value nobody
> obtained. Suppressing a write leaves the computation intact and only the world
> unchanged.

So the distinction is in the type system, not in a comment:

| sink                  | reads                           | writes     | used by   |
| --------------------- | ------------------------------- | ---------- | --------- |
| `SuppressedSink`      | returns `Unit`, reports the gap | suppressed | `replay`  |
| `ObservationOnlySink` | performed, journalled           | suppressed | `observe` |

`observe` — "evaluate the contract, take no action of its own" — **reads the
world and writes nothing**. A read is not an action. Suppressing it would make
`observe` a strategy that cannot agree with anything, including itself.

`replay` takes nothing from the world, because its read results come from the
journal it rebuilt, and the journal is consulted _before_ the sink.

### 3b. A replay reproduces an execution exactly when the log is sufficient

Discovered by the test, and the reason it is written the way it is: for a run
cut at **every** record boundary of a real log, a replay lands on the
failure-free trunk **iff every read the winning future made is already in the
journal**. Before that boundary the run cannot match, and it says so —
`SuppressedSink::suppressed_unreadable` names the read it could not answer.

This is the "log alone is sufficient" property, and it is _bounded_. A replay is
not a re-run: it cannot invent an observation it was never given. Stating the
boundary is more useful than claiming an unconditional equivalence, and the test
derives the boundary from the log rather than from a number somebody typed.

The product consequence: a contract whose outcome depends on an observation must
either have that observation journalled before the cut, or the `replay` strategy
reports the gap. It is a property of the contract, not of the platform.

### 4. The kernel does not know the word "contract"

`docs/domain-map.md` says the kernel "does not know any of these words", and
principle 5 of `docs/architecture-principles.md` says the same. The strategy
abstraction is therefore about a **`Workload`**, in the kernel's own vocabulary
(`Plan`), and the product maps a contract document onto a `Plan`. The mapping is
recorded in ADR-013.

This is a constraint on naming, not on capability, and it is why the type is
`ExecutionStrategy` and not `ContractStrategy`.

### 5. `SuppressedSink` — a real sink, named for what it does

`EffectSink` gains `SuppressedSink`. It performs nothing and records every
attempt, so a test can assert both halves: what _would_ have been issued, and
that nothing was.

Its documentation states the distinction that matters: **a suppressed effect is
not "did not run", it is "not issued, and later replayable"**. A suppressed
`Read` returns `EffectValue::Unit` and no value, because a sink that invents a
value would be a lie. The value a replay needs comes from the effect journal,
which is consulted _before_ the sink — which is why a replayed run is correct
without re-observing the world.

### 6. Replay is a rebuild _and a continuation_, and that needed a new kernel API

`replay` does: recover the source store (dropping any torn tail), decode the
durable prefix, `rebuild()` the execution graph, and **continue the execution
from there** with a `SuppressedSink`.

Continuing needed something the kernel did not have. `rebuild` could prove a
past was recoverable; nothing could _finish_ it. That is not a nice-to-have: a
Durable Object that restarts must continue its execution, and re-executing into a
fresh log would put two executions in one log and break the invariant the whole
store rests on.

`Kernel::resume` was therefore added. It is a mechanical 1:1 mapping from
`ExecutionGraph` onto a `Kernel` — every counter, head, cursor and owed effect
comes from the graph, never from a fresh default — and it is documented field by
field, including the three things it deliberately does **not** carry across (the
virtual clock, the executor slots, and the trunk's `arm_end`).

**And it immediately found a real bug.** `rebuild` had never been checked for
cursors — `tests/recovery.rs` compared statuses, heads and bases, never _where
each future was_. Two `Select`/`Commit` handlers recorded the executing future's
next cursor against the _winner_ instead of against the parent, so a recovered
graph reported the right state while pointing every future at the wrong place.
`tests/recovery.rs::every_recovered_cursor_matches_the_cursor_that_was_executed`
was written first, failed, and is the fix's evidence. This is invariant I13 in
`docs/invariants.md`.

## Consequences

**Positive**

- Four strategies, one runtime, one ledger format, one recovery path. The
  strategy is a value in a contract row.
- The agreement property is a _test with a name_, not a convention.
- A strategy that does not work is a variant that fails one test, not an
  architecture.

**Negative / accepted**

- `deterministic` does not reduce a fork to one arm. A customer picking it for
  "boring" still gets the plan's alternatives evaluated, serially and
  synchronously. This is recorded, because it is a real difference from the
  brief's table.
- `SuppressedSink` returning `Unit` for a `Read` means a _replay from an empty
  journal_ cannot recover the observed value. It cannot, and pretending
  otherwise is the failure this decision prevents.
- The agreement test is only as good as the workloads it runs. It covers a
  fork/effect/commit workload; it does not prove agreement for workloads nobody
  has written yet.

**Not claimed**

- That the four strategies have the same _cost_. They do not, and the test
  asserts the opposite: `deterministic` and `speculative` must differ in
  `flushes` / `wall_time` while agreeing on the answer, or the test is running
  the same configuration twice and proving nothing.

## Revisit triggers

- A strategy needs to change the _plan_ rather than the configuration (for
  example, a genuinely different algorithm per contract). That is a second
  abstraction — `Workload` selection — and it needs its own ADR, because it
  breaks P1 structurally.
- The platform port of this (ADR-003, Phase 4/5) cannot re-establish P1 on
  Durable Objects. Then the agreement claim must be narrowed to the platforms
  where it is tested, and the README updated in the same commit.
