# ADR-005 — Contract declarations are schema-validated, not type-system-enforced

**Date** 2026-09-29 · **Status** accepted · **Scope** contract model

## Context

v1 carried a gap ledger. Two entries are load-bearing for v2:

- `GAP-WORKFLOW-002` — "Node classification not type-enforced" (P1, never closed)
- `GAP-WORKFLOW-001` — "Simulation conflates replay" (P1, never closed)

v1's contracts validated the *shape* of a single event at ingest
(`ContractViolation`, JSON Schema). Nothing knew that `order.placed` was
*supposed to be followed by* `charge.captured` within 30 seconds. That is the
genuinely new part of v2.

There is a real question of whether the expectation should live in the type
system, the schema, or the data.

## Decision

**Declare expectations as data — a contract document — validated by a JSON
Schema, evaluated at runtime.** Not in the Rust type system, not in the
workflow DSL's types.

Rationale:

1. **Users author contracts as data.** A customer-facing product has to accept
   a contract from someone who is not writing Rust. That means JSON/YAML, not a
   type.
2. **Type-enforcement closes the v1 gap by moving the burden to the author.**
   `GAP-WORKFLOW-002` failed because classification was a convention. Encoding
   it in types only helps if the author is a type checker; a JSON document
   authored in a dashboard gets the same validation, with a better error
   message, because the schema is applied to untrusted input at a boundary.
3. **The kernel stays unchanged.** Expectations are a product-layer concern;
   they consume events and emit breaches. They do not belong in `src/domain`.

## Consequences

- Contracts are versionable, diffable, and reviewable as documents.
- A contract can be added without redeploying the runtime. Required for the
  day-1 product.
- Validation is at ingest and at declaration time; a malformed contract is
  rejected with a precise error rather than failing at 2am.

## Revisit triggers

- Contracts need to be composed/inherited across many files → consider a
  compile step, still producing the same runtime document.
