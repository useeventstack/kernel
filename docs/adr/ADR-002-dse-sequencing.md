# ADR-002 — DSE is deferred; breach detection is the day-1 product

**Date** 2026-09-29 · **Status** accepted · **Scope** product sequencing

## Context

The kernel supports speculative execution. The temptation is to make the
guarantee — "no effect of a rejected future reaches the world" — the product
pitch from day 1.

Two problems with that.

1. **It cannot ship on day 1.** The guarantee requires a real durability
   boundary, which on Cloudflare means Durable Objects + the Emscripten Rust
   target (ADR-003). That is a port, not a config change.

2. **A guarantee is a warranty, not a feature.** A user who buys for "no
   double-charge" and finds it switched off has been sold something that isn't
   there. "Coming soon" is close to worthless for this class of claim. The
   breach-detection product, by contrast, is fully functional on day 1 and
   valuable with no DSE at all.

## Decision

**Day 1 product: contract declaration, event ingest, breach detection,
attribution, and a dashboard.** All of this works with a deterministic
(durability `Synchronous`) store. No speculation required.

**Later milestone: DSE as a per-workflow opt-in**, with the guarantee enabled
for the workflows that opt in. The kernel's default stays deterministic.

The claim structure is therefore:

| Phase | Claim | Status |
| --- | --- | --- |
| Day 1 | "We tell you when a chain of events did not complete." | true, live |
| Later | "And we can *prove* no effect of a rejected branch escaped." | opt-in, proven in production on customer data |

The second claim is far stronger once it is provable in production on real
traffic, rather than on a virtual-clock benchmark.

## Why the kernel is a natural fit on day 1 anyway

`DurableStore` is already the abstraction boundary. `MemoryStore` and
`FileStore` both implement it; the 212 tests pass against both. A
`DurableObjectsStore` (or a D1-backed store) is a third impl of a 10-method
sync trait. The kernel is store-agnostic by design, so the DSE port does not
disturb day-1 work.

## Consequences

- Day 1 ships a genuinely useful product, not a promise.
- DSE is a differentiated upgrade, not a launch blocker.
- The breach-detection path must be correct *without* DSE, so it cannot rely on
  the kernel's isolation guarantees for its critical logic.

## Revisit triggers

- A customer pays specifically for the no-double-charge guarantee → DSE becomes
  day-1 for that workflow.
- The deterministic path proves insufficient for a real contract.
