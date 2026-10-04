# Architecture principles

Six principles. They are ordered by how often they will be argued about.

---

## 1. Durability is a property of the store, never of the request

A response is not an acknowledgement until the write is committed. Storage
choice is therefore a correctness decision before it is a cost or latency one.

**Enforced by:** every write path ends in a real transaction. Eventually
consistent storage (KV) is a cache and is labelled as one in code and in review.

**Fails without it:** an ingest endpoint returning 200 for a write that is not
yet durable is a lie the customer discovers later.

## 2. Evaluate, don't poll. But sweep anyway.

Contract satisfaction is evaluated **when events arrive**, not on a timer —
evaluation must be part of the write path, or breaches are detected hours late.

Breach *detection* (the deadline sweep) **is** a timer, and must not depend on
user traffic. A contract that is only checked when a human opens the dashboard
is not monitoring.

**Enforced by:** an alarm/scheduled sweep, independent of request volume.

**Fails without it:** a quiet system silently accumulates breaches.

## 3. Deadlines are per-expectation

One contract has many expectations with different urgencies. A single timeout
cannot express "money in 60 seconds, analytics whenever."

**Enforced by:** the contract schema carries `within` per expectation, and the
schema is validated (ADR-005).

**Fails without it:** every contract is either too strict (noise) or too loose
(misses real breaches).

## 4. Identity and tenancy are established before data exists

Multi-tenant from the first commit, with isolation proven by a test that
attempts cross-tenant access. Not retrofitted, not assumed.

**Enforced by:** a single `requireTenant(session)` gate at the route edge, and
a test that a tenant cannot read another's contract or breach.

**Fails without it:** v1's `GAP-SEC-001` and `GAP-TEST-002`, both P0, both
never closed.

## 5. Keep the kernel free of the product

The kernel (`src/`) has zero dependencies and knows nothing about tenants,
auth, billing, or HTTP. It is adopted, tested, and upstreamed on its own terms.

**Enforced by:** dependency check in CI; product code lives in `crates/` and
`apps/`.

**Fails without it:** the kernel stops being independently adoptable, and the
upstream contribution (ADR-003) becomes impossible.

## 6. A guarantee is opt-in and per-workflow

Determinism is the default. Speculation and its safety guarantee are enabled
only for workflows that ask, and only where the economics justify it.

**Enforced by:** `ExecutionPolicy::default()` is synchronous; a test asserts it.

**Fails without it:** a correctness feature becomes a performance tax on
everyone, and a user who did not opt in inherits a risk they did not choose.

---

## Where these came from

Principles 1, 2, 4, and 6 each answer a specific failure we have already seen or
documented — v1's P0 security gaps, the absence of a breach branch, the DSE/edge
conflict, the temptation to make speculation universal. They are not
aspirations; they are the opposite of a mistake already made.
