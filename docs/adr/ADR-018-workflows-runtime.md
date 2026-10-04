# ADR-018: Cloudflare Workflows is the execution engine; the TS kernel is the invariant suite

**Date** 2026-10-02 · **Status** accepted · **Scope** execution · **Supersedes** the experimental `apps/api/src/do/execution.ts` production direction and reframes ADR-017 (execution offload question)

## Context

The repository carries two execution engines:

- A Rust kernel in `src/`, hermetic, deterministic, crash-tested at exact record boundaries.
- A TypeScript runtime in `apps/api/src/runtime/engine.ts` that now mirrors it against four serialisable ports.

In October 2026 Cloudflare shipped the pieces we were approximating: Workflows (GA, rearchitected during Agents Week for agent scale), Durable Object facets, DO SQLite, queues with at-least-once delivery, and a DSQL projection of events. To be world-class on edge and not invent a parallel scheduler, the product's production path should use Workflows as the durable execution runtime.

## Decision

- **Production durable execution runs on Cloudflare Workflows.** `WorkflowEntrypoint` instances own the step journal; `step.do`, `step.sleep`/`sleepUntil`, `step.waitForEvent`, retries, and compensation mirror the kernel's journal/advance/park semantics.
- **The Rust kernel remains the correctness contract.** The invariants in `src/` (idempotency ordinals, total effect safety, scope isolation, crash replay) are encoded as compiler rules and property tests that the Workflows layer must satisfy; they are not removed.
- **Ingest stays on the queue:** `IngestSerial` per tenant/business key orders, acknowledges, retrys, and dead-letters. `engine/ingest.ts` remains the synchronous contract consequence writer.
- This does not change contracts, tenant isolation, passkeys, billing, or DSE arbitration.

## Consequences

- Positive: we stop re-implementing a managed durable runtime; Workflows's persistence, alarms, retries, and cost model are production.
- Accepted cost: the TS experimental `Execution` DO is now a fixture/audit path rather than the sole edge runtime; any future reliance on it must be justified against Workflows parity.

## Revisit triggers

- Workflows cannot express DSE branch isolation for a contract profile.
- A contract needs stronger replay/idempotency proofs than Workflows's step journal provides.
