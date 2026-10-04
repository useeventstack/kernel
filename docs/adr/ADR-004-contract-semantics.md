# ADR-004 — Contract semantics: saga, not distributed transactions

**Date** 2026-09-29 · **Status** accepted · **Scope** contract model

## Context

The founding idea: "when `deployment.completed` runs, every event derived from
it is attached to the contract; if one is missing, we know when the contract
fails to complete. Treat it transactionally, like SQL."

The instinct is right; the mechanism is not available. SQL transactions work
because the database is the only party touching the data. In a real contract
chain the steps touch Stripe, a warehouse, an ERP, and a partner webhook. Those
systems do not participate in our lock. A distributed lock held across a call
to Stripe is a promise we cannot keep.

## Decision

**Model contracts as sagas, not ACID transactions.**

1. **Three outcomes, not two.**

   | outcome | meaning |
   | --- | --- |
   | satisfied | all expected events arrived before the deadline |
   | violated | an event arrived that the contract forbids or contradicts |
   | breached | an expected event never arrived before the deadline |

   The breach branch is the new one. Nothing in v1 had it.

2. **Compensation is a first-class effect class.** The kernel already
   classifies effects as `Pure` / `Read` / `IdempotentWrite` /
   `Compensatable` / `Irreversible`, and defers the last two until commit. That
   is the saga pattern with compensation modelled as a type rather than as
   ad-hoc rollback code.

3. **Breach detection is itself durable state.** A crash of our system must not
   lose the fact that a contract was outstanding. This is the property the
   kernel's log-only-reconstruction invariant already provides.

4. **Idempotency keys derive from identity, not runtime ids.** Already decided
   in the kernel (`(execution, arm path, ordinal)`), because a rollback
   allocates new `FutureId`s and a naive key double-applies at exactly the
   crash moment.

## Consequences

- No distributed locks, no 2PC, no false safety.
- Compensation logic is written explicitly and is testable.
- The guarantee is "no *irreversible* effect of a rejected future escapes,"
  which is honest and provable — not "exactly-once," which is not achievable
  for a remote target.

## Revisit triggers

- A contract genuinely requires cross-system atomicity that compensation cannot
  express. The answer then is a human reconciliation process, not a lock.
