# ADR-001 — Platform: Cloudflare, fully edge, from day 1

**Date** 2026-09-29 · **Status** accepted · **Scope** v2 platform architecture

## Context

v2 is a contract-first event platform. A company declares "when `order.placed`
happens, `charge.captured` and `inventory.reserved` must follow within 60s, or
the contract is in breach." The platform must observe chained events, detect
breaches, and be globally reachable.

The kernel in this repo (`src/`) is a durable execution kernel with first-class
execution futures. Its `DurableStore` trait requires a **real** durability
boundary: `commit_all` / `commit_prefix` must make bytes survive process death.
`FileStore` implements this with `sync_data()` / `sync_all()`.

The hard constraint: the founder has no paid tooling budget. Infrastructure must
run on free tiers, be globally distributed, and be live within days.

## Decision

Build entirely on Cloudflare, fully edge, from day 1. No hybrid, no regional
fallback, no containers.

- **Compute** — Workers (Free: 100k req/day, 10ms CPU/invocation; Paid: $5/mo)
- **Contract/tenant state** — D1 (SQLite-backed, single Durable Object per DB)
- **Runtime coordination** — Durable Objects (SQLite storage, real transactions)
- **Ingest fan-out** — Queues
- **Static assets / dashboard** — Workers Static Assets (free, unlimited)
- **Edge config** — WAF, rate limiting, DNS, CDN

**Explicitly rejected: Durable Objects + Emscripten for day 1.** DSE is deferred
to a later milestone (see ADR-002). The reason is not architectural preference —
it is that the Emscripten Rust target is a *first public experimental preview
announced 2026-09-28*, i.e. one day old, with a pre-release Tokio patchset still
under upstream review. See ADR-003.

## Consequences

**Positive**
- Global in days, not months. Cloudflare deploys globally by default.
- ~$5/month at launch; $0 on the Free plan for a solo founder at low volume.
- One platform, one set of primitives, no cross-cloud networking.
- The DSE path stays open and is the natural day-90 differentiator.

**Negative / accepted**
- No true shared-nothing parallelism in the durability path (DO is single-threaded).
- D1 single-writer: each database is one Durable Object processing queries
  serially. This is a throughput ceiling, and is acceptable because the kernel's
  `workers: 1` default matches it.
- The platform bet is on Cloudflare. Emscripten is a preview; a break there
  delays the DSE milestone, not the product.

## Revisit triggers

- D1/DO throughput ceiling actually hit in production.
- Emscripten target reaches stable (1.0) release.
- A customer requires data residency in a specific region.
