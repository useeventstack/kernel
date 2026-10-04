# Research notes and the novelty claim

What this design is made of, and what — if anything — is new about the
combination. Written to be checkable rather than persuasive.

---

## 1. The claim, stated at its honest width

**Not claimed:** that durable alternatives, three-way merge, effect journalling,
idempotency keys, or optimistic commit are new. Every one of them is prior art,
decades old, and extensively deployed.

**Claimed:** that their *composition* — a workflow runtime where the unit of
speculation is a durable, content-addressed, isolated future that can be
evaluated against its peers and committed atomically, with a log that
reconstructs the whole graph by replay — is not something the surveyed systems
offer as a first-class abstraction, and that the composition has properties the
pieces do not have individually.

That is a modest claim and it is the one the evidence supports. Anyone who finds
a system that does all of it should read that as "this was already solved", not
as a defect in the survey.

---

## 2. Where the ideas come from

| this design | prior art | what was taken | what is different here |
|---|---|---|---|
| content-addressed state, O(1) fork | git objects, ZFS, Bazel, Merkle trees | store a hash, not a copy | the hash *is* the state, so "did this branch change anything" is an address comparison, and a fork record is 17 bytes |
| three-way merge at commit | git merge, CRDTs, MVCC conflict resolution | base/ours/theirs with conflicts surfaced | the base is the fork point the runtime recorded, and the merge is recomputed during recovery and compared to the log |
| durable alternatives | workflow fan-out (`Promise.all`/child workflows), Airflow dynamic task mapping, Temporal child workflows | run branches, keep one | the branches are *durable* and isolated, not in-memory promises whose loss loses the work |
| effect journalling, replay | Airflow's task-level replay, Temporal's event history | record the observation, return it on replay | the journal is the *only* source of an effect's value; a replayed effect is never re-observed |
| idempotency keys | Stripe API idempotency, inbox/outbox, sagas | caller-supplied token, target dedupes | the key is **derived by the runtime** from `(execution, arm path, ordinal)` — a caller-supplied key cannot distinguish "the same request again" from "a different plan doing a similar thing" |
| commit as an atomic record | two-phase commit, saga coordinators, ledger commit | one record is the commit point | the record is the *whole* commit, and recovery recomputes it rather than trusting it |
| asynchronous persistence | every group-committing database and log | flush in the background | it is a *policy on the same kernel*, not a mode, and the failure matrix proves all policies give the same answer |

The row that carries the most weight is idempotency. It is the one where a
naive version of this design fails silently, and where the interesting
observation is not "use idempotency keys" but **the key must be stable under
retry, and the obvious identity is not stable**. See §5.

---

## 3. The DSE paper

The prototype that preceded this one was built after reading *"Distributed
Speculative Execution for Resilient Cloud Applications"* (Li, Chandramouli,
Bernstein, Madden — [arXiv:2412.13314](https://arxiv.org/abs/2412.13314)), and
this repository reproduces none of that system's mechanisms. It implements
neither `libDSE` nor its protocol, and claims no equivalence with either.

Two things are worth recording, because they shaped the decision to rewrite
rather than extend:

- The paper's mechanism is **asynchronous logging with deterministic recovery
  of the durable prefix** — speculative execution in memory, persistence off the
  critical path, replay of what was proven. That part is real and it is what the
  first prototype demonstrated.
- The terms that circulated around the original idea — *proactive and reactive
  steps*, *ordering points*, *causality records* — are not in the paper. The
  paper's own framing is a log-structured, deterministic-replay design.

That distinction mattered practically: the first prototype could prove
asynchronous persistence and deterministic recovery, and had no notion of
alternatives at all. So the honest conclusion from reading it was not "extend this"
but "the persistence half is solved and the interesting half is elsewhere".

---

## 4. What the surveyed workflow systems do and do not have

Systems examined: Temporal, Restate, Inngest, Cloudflare Workers, Vercel
Functions, AWS Step Functions, Azure Durable Functions, Conductor, plus the
underlying database machinery (MVCC engines, event-sourced stores) and the
classical literature (sagas, the transactional outbox pattern, two-phase commit).

What they all have, and what this design takes:

- **Durable history and replay.** Every one. An event log, a replay mechanism,
  determinism as a requirement.
- **Isolation between concurrent branches.** Via activities, task queues, or
  separate executions.
- **Effect discipline.** Sagas and compensating actions, activity idempotency
  keys, at-least-once delivery as the stated contract.
- **Branching where the platform provides it.** Child workflows, fan-out.

What none of them appeared to offer as a *first-class* abstraction:

- **A branch as a durable, addressable thing you can point at.** Child workflows
  are addresses, but the parent does not hold their state and cannot compare
  them, merge one into the trunk, or discard the rest with a single record.
- **A commit that is atomic with respect to the state, not just the pointer.**
  Choosing a winner and *merging* it into concurrent trunk state is a step these
  systems either leave to the caller or do not do.
- **A single evaluator-driven selection as a durable event.** A conditional or a
  race between children resolves; the *decision* is not usually a logged,
  replayable fact with the losing branches' disposal recorded alongside it.
- **A content-addressed state model where forking is free.** Most of these
  systems pay for a branch by duplicating a workflow's work, not by copying a
  pointer.

The honest reading of that list: the *pieces* are all deployed somewhere, and
several are deployed together in systems I did not examine. What is claimed is
that in the workflow systems surveyed, the durable alternative is a thing you
build *out of* their primitives, not a thing they give you.

---

## 5. The one genuinely non-obvious finding

**Idempotency keys must be stable under retry, and the obvious identity is not.**

The natural key is `(future, effect ordinal)`. It is stable within an attempt
and obviously unique within a run. It is wrong.

A rollback — the normal response to a failure, and the mechanism that makes
speculation recoverable at all — discards the futures a fork created. When the
fork is re-executed, the runtime allocates **new** `FutureId`s for the same arms.
A key built from the id is therefore stable until the first crash after a retry,
at which point a speculative `IdempotentWrite` is re-issued under a fresh key.
The target has never seen it, so it applies it. The write happened twice.

The fix is to key on something the re-execution regenerates identically: the
**arm path** — the sequence of fork choices from the root, which is a function of
the plan, not of the runtime. `EffectKey` is `(execution, path, ordinal)`.

This is the sort of thing that separates a design that recovers from a design
that is merely correct in the absence of failure, and it was found by the process
kill test rather than by reading. It generalises: *anything durable must be
keyed on something the retry reproduces, and runtime-assigned identity is
usually not it.* The same reasoning shows up in the durable-cursor fix, where a
cursor resolved by version alone let a child inherit its parent's position.

---

## 6. What is deliberately not novel

- Asynchronous persistence with group commit. Decades old.
- Content-addressed storage and Merkle trees. Older.
- Three-way merge. `diff3` predates everything here.
- Sagas, the outbox pattern, two-phase commit. All classical.
- Speculative and optimistic execution generally, from CPU pipelines to
  optimistic database concurrency control.

`useEventStack` is a composition of these into a workflow abstraction, with the
combination's properties — atomic commit of a merge, log-only reconstruction,
effect safety under rollback — argued from the parts and checked by the failure
matrix. It is a research prototype exploring whether that composition is worth
having, not a new theory of durable computation.

---

## 7. How to check any of this

The interesting claims are mechanical, so they are checkable:

```bash
cargo test                                  # 212 tests
cargo test --test failure_matrix            # crash at every checkpoint
cargo test --test kill_process              # real SIGABRT, resume from disk
cargo run -- bench                          # four arms, claims stated in the output
```

The failure matrix is the load-bearing one. It is the thing that would catch a
future change breaking any of the above, and it is where the seven real bugs in
§6 of [experiments.md](experiments.md) came from.
