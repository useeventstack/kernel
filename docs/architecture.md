# Architecture

How the kernel works, and why each piece is shaped the way it is. For what is
*enforced* rather than merely intended, see [invariants.md](invariants.md).

---

## 1. The layering rule

```
domain/     plan, future, state, node, effect, delta — no I/O, no clock, no policy
ledger/     record, store, journal — the only things that touch bytes
kernel/     engine, evaluate, commit, durability, recovery, policy
ports.rs    DurableStore, EffectSink, CostModel, VirtualClock
```

`domain` depends on nothing. Everything else depends on it and on the ports.

This is not tidiness. It is what makes the failure matrix possible. The kernel
has no `SystemTime` and no `std::fs`; it reads the clock and the store through
ports, so a test can place a crash at *record 47 of attempt 1* and get a
deterministic answer. A kernel that reached for the wall clock directly would
still be correct and would still be untestable at the boundaries that matter.

---

## 2. State is content-addressed and immutable

```rust
StateNode(u64)   // an address into a StateStore, computed by hashing the state
```

A future holds a `StateNode`, never a state. A step produces a new node and
leaves the old one in place.

Three things follow, and all three are load-bearing:

- **Forking is O(1).** A child is `(parent's head, arm root)`. The size of the
  state is irrelevant to how long a fork takes. In a system that snapshots
  state per branch, this is the difference between a constant and a copy.
- **A fork record is 17 bytes per child.** It stores a hash. The whole branching
  structure of a run is proportional to the number of branches, not to the size
  of what was branched.
- **Merging is available for free.** Because equal states have equal addresses,
  "did this future change anything?" is an address comparison. The three-way
  merge needs no write-set bookkeeping: a key was written by `ours` iff
  `ours[k] != base[k]`. See `State::three_way_merge`.

The hash is FNV-1a, not cryptographic. That is sufficient — the store is trusted
in-process, and the content address is a deduplication device, not a security
boundary — and it is stated here because a reader will otherwise assume SHA.

`StateNode::EMPTY` is a sentinel at address 0, not the hash of the empty state.
Both map to an empty `State`; the sentinel exists so a plan can be run from
nothing without interning first.

---

## 3. Plans are flattened, and that is a safety property

A plan is compiled once into a single array of nodes with cursor positions. A
fork does not become a nested structure; its arms become **cursor ranges**:

```
  node 0   set total = 0
  node 1   FORK  ── arm 0: nodes 2..5 ── arm 1: nodes 6..9 ── arm 2: nodes 10..13
  node 14  SELECT
  node 15  COMMIT
  node 16  add committed += 1
```

A child starts at its arm's root and `advance` clamps to its arm's end.

The alternative — a tree, with a child holding a sub-plan — is more natural to
write and less safe to run. A cursor is easy to get wrong; a clamped cursor is
hard. `finished()` asks `cursor >= arm_end`, **not** `plan.node(cursor).is_none()`,
because in a flattened program the node *after* an arm's end is the next arm's
first node. A child that walked past its extent would silently start executing a
sibling's plan, against states it never branched from, and the log would look
well-formed throughout.

This is the shape of the bug the durable-cursor fix addressed: a child with no
durable work of its own resolved its resume cursor from the *parent's* record and
started mid-sibling.

---

## 4. A future

```rust
struct Future {
    id: FutureId,            // runtime-assigned, NOT stable under retry
    path: Vec<u32>,          // fork-arm path from the root — stable under retry
    base, base_version,      // where it started; never mutated
    head, head_version,      // what it has produced
    durable_head, durable_version, durable_cursor,   // what survived
    cursor, status, chain, effects, children, score,
}
```

The distinction between `head` and `durable_head` is the whole of recovery.
A rollback restores the second pair and the second cursor; a future resumes from
where its *durable* work ended, not from where its speculative work happened to
reach.

`path` and `id` are both futures' identity, and they behave differently on
purpose. `id` is convenient and is what the executor schedules on. `path` is what
anything durable is keyed on, because a rollback deletes the futures a fork
created and the re-executed fork allocates fresh ids — while rebuilding the same
paths. See §6.

Statuses: `Open`, `Running`, `Blocked`, `Evaluable`, `SelectionRecorded`,
`Committed`, `Rejected`, `Failed`, `Abandoned`. The last four are terminal.

---

## 5. Durability is a policy, not a mode

```rust
enum Durability {
    Synchronous,                    // every record durable before the next step
    GroupCommit { batch: usize },   // batch N, then wait
    Async { window: usize },        // run ahead up to W records, writer in background
}
```

One kernel, three policies. This is deliberate: the failure matrix runs the same
crash points under all of them and requires the same answer, so the policies
cannot differ in *semantics* — only in when work becomes recoverable. A policy
that changed the answer would be a second implementation wearing a flag.

`enforce_window` runs before *every* record, not only before steps. A `Settled`
or `Committed` record is as much a fact as a step is, and a policy that let the
executor run ahead of durability and then wrote unbounded control records would
quietly exceed its own window. A window that is not enforced is not a bound.

`peak_speculation` reports the high-water mark, and `sync_wait` reports time
parked. Both exist because "asynchronous" is a claim and a claim needs a number.

---

## 6. Effects

```rust
enum EffectClass { Pure, Read, IdempotentWrite, Compensatable, Irreversible }
```

The class decides two things: whether the effect may be performed during
speculation, and what must be true before it is released.

- `Read` and `IdempotentWrite` are performed when the step runs, and the result
  is journalled. Replay returns the journalled value; it never re-observes. That
  single rule is the whole of the determinism guarantee — an effect that is
  re-executed must not produce a second, different answer.
- `Compensatable` and `Irreversible` are recorded as **intents** and released at
  commit.

**The key.** `EffectKey` is `(execution, arm path, ordinal)`. It is never
supplied by the caller and never derived from a `FutureId`.

This is not a detail. A rollback discards the futures a fork created, so a retry
allocates new ids. A key built from ids is stable until the first crash, and
then a speculative `IdempotentWrite` re-issues under a fresh key, the target sees
a request it has never seen, and applies it twice. The token has to survive the
thing it is deduplicating, so it is built from the arm path, which does. The
process-kill test asserts the resumed run charges exactly what a clean run
charges, and no more.

**The ordering at commit.** The commit record is appended, then made
*durable*, then the deferred effects are released:

```
  append Committed  ──▶  force_durable  ──▶  observe AfterCommit  ──▶  release
```

Releasing on "the record is written" rather than "the record is proven" means a
crash in between rolls the commit back with the charge already at the bank. No
recovery can undo that. The one flush is the price of the guarantee; it is paid
deliberately, and only here.

**Release debt is derived, not remembered.** A `Committed` record with no
matching `Released` record is a commit whose effects the world has not seen. That
set is recomputed from the durable prefix on every rollback, so a restarted
process reaches the same conclusion the crashed one did.

---

## 7. Commit

```
  1. validate:      the future is Evaluable, has a selection, is not already authoritative
  2. merge:         three_way_merge(base = future.base, ours = trunk, theirs = future.head)
  3. conflicts:     any key both sides wrote differently → ConflictPolicy
  4. record:        append Committed{future, trunk, trunk_version, release}
  5. durable:       force_durable(trunk_version)
  6. release:       issue the deferred effects, each with a Released record
```

Step 4 is the atomic commit point. Step 5 is where irreversibility is paid for.

`ConflictPolicy::Abort` is the default and the only honest option: the runtime
does not know which of two conflicting values is correct. `TakeFuture` exists
because sometimes it is the right call, and is named precisely because it is the
weaker one — it does not remove the conflict, it picks a winner without being
asked.

A selection is only taken over **settled** futures. Scoring a half-executed
branch against a finished one is not a small imprecision: a branch that has not
reached its score write has no score, so the evaluator hands the win to whichever
arm merely looks viable, and the log then records a well-formed selection, a
merge and a commit — all of a wrong answer. The condition the select *waits* on
and the condition that *releases* it (`child_settled`) are deliberately the same
predicate: a speculative scheduler is only correct when those are the same fact.

---

## 8. The ledger

```rust
enum Payload {
    Opened  { plan },
    Step    { delta, cursor },
    Effect  { delta, cursor },
    Fork    { parent, cursor, children },
    Selected { among, scores, winner, cursor },
    Committed { future, cursor, trunk, trunk_version, release },  // the commit point
    Settled { future, status },
    Released { future, ordinal },
}
```

Framed on disk, with a length prefix and a checksum. `recover()` drops a torn
tail — a half-written frame from a crash is not corruption, it is a crash, and
the store distinguishes them.

`Payload::cursors_moved()` reports every `(future, cursor)` a record leaves
behind, and it is not just `Step` and `Effect`: a `Fork` moves the parent's
cursor, and a `Committed` moves *two* — the winner's and the trunk's, because
the commit is the moment the winner's work becomes the trunk's. Omitting the
second is how a committed trunk ends up resuming before its own commit and
re-committing a future that is already authoritative.

---

## 9. Recovery

A restart is given the durable prefix and nothing else:

```rust
let records = kernel.durable_records();   // the ONLY supported recovery input
let graph   = rebuild(&records, execution, plan)?;
graph.validate()?;
```

`durable_records()` is a method rather than a convention because the alternative
is a recovery that reads the kernel's in-memory record list — which contains
records that were buffered and never proven, and which, before this was fixed,
also contained records a rollback had logically discarded. A restarted process
has no access to either. Recovery that reads them reconstructs a graph that
includes a commit which was rolled back.

Cut by **record count**, not by version. A version-based cut is subtly wrong:
control records carry the version they observed, which can exceed the durable
frontier, so a version cut drops durable records — a `Settled{Rejected}` the
store certainly holds.

Recovery then replays: states are re-interned, futures are reconstructed with the
same arm paths, effects are re-journalled from their dispositions, and **every
commit is recomputed**. The recomputed merge must equal the trunk the record
claims, or recovery fails loudly:

```
  the commit of FutureId(2) claims trunk 2d3a2aacd99d6571
  but the merge of its base, its head and the trunk is 90a7efaa0bc9810d
```

That check is the reason to trust a recovered graph. Replaying a commit record
*mechanically* would succeed on a log written by a buggy kernel; recomputing it
and comparing catches exactly that.

Rollback, in the same process, does the inverse and must additionally:

- drop futures whose `base_version` is beyond the durable frontier, because the
  fork that created them is being discarded;
- truncate the in-memory log to the durable prefix;
- re-derive `authoritative` from that prefix — carried in memory, it would
  otherwise make a re-executed commit trip its own "already authoritative" guard;
- not demote a future whose commit was durable, or nothing will ever set it back;
- recompute the release debt from the prefix.

---

## 10. What is deliberately absent

No compaction or snapshotting — the log grows without bound.
No multi-node coordination — one store, one clock, one writer.
No async runtime — the executor is a deterministic virtual-time scheduler, which
is what makes the matrix possible and would have to be replaced for real
concurrency.
No general expression language — a plan is a flat list of nodes, which is what
makes flattening and clamping possible.

Each omission is a simplification that buys a property worth more here than the
feature would: the crash boundaries are exact, and the recovery argument is
short enough to check by reading.
