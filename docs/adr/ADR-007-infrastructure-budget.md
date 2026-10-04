# ADR-007 — Infrastructure budget: free tiers only, with stated ceilings

**Date** 2026-09-29 · **Status** accepted · **Scope** operations

## Context

Constraint from the founder: no paid tooling budget; free tiers only; launch
today. Global in three months. This ADR records what that actually costs and
where it breaks, so the ceiling is known rather than discovered.

## Decision

Launch on the **Workers Free plan**, move to **Workers Paid ($5/month minimum)**
when any of these become true. Track the thresholds.

### Verified free-tier limits (from Cloudflare pricing/limits docs, 2026-09-29)

| primitive | free limit |
| --- | --- |
| Workers requests | 100,000/day |
| Workers CPU | 10 ms per invocation |
| Workers duration | no charge |
| Egress / bandwidth | **no charge** |
| Static assets | free, unlimited |
| D1 | 1 database (Free) / 10 (Paid); 500 MB / 10 GB per DB; 5 GB / 1 TB per account |
| D1 Time Travel | 7 days (Free) / 30 days (Paid) |
| D1 queries per Worker invocation | 50 (Free) / 1000 (Paid) |
| Workers KV | 100k reads/day, 1k writes/day, 1k deletes/day, 1k lists/day |
| Durable Objects | **available on Free**, SQLite storage backend only — 100k req/day, 13,000 GB-s/day |

### When to upgrade to Paid ($5/mo)

Any one of:

1. More than **one** D1 database — the Free plan allows 1, and per-tenant
   sharding is the design (ADR-006). This is expected to be the first trigger.
2. **D1 queries per Worker invocation: 50** on Free, vs 1000 on Paid. Contract
   evaluation must batch under 50 queries per invocation; exceeding it is a
   redesign signal, not just an upgrade.
3. **KV write limit**: 1,000 writes/day. Any write path on KV exhausts this in
   minutes. KV is a read cache only (ADR-006).
4. D1 Time Travel beyond 7 days.
5. DO compute beyond 13,000 GB-s/day (Free) or requests beyond 100k/day.

**Two corrections to earlier drafts of this ADR**, both found by re-reading the
pricing page rather than trusting my own summary:

- An earlier draft claimed DO *required* the Paid plan. It does not. The docs
  state: *"Durable Objects are available both on Workers Free and Workers Paid
  plans"*, with the Free plan limited to **SQLite storage backend only**. Since
  SQLite is exactly the backend this design needs (ADR-006), the $0 day-1 claim
  holds after all.
- An earlier draft then "corrected" that into "$5/month is the day-1 cost". That
  was also wrong. Both errors came from reasoning about DO in the abstract
  instead of reading the table.

**Day-1 cost is $0.** The $5/month Paid plan is triggered by the second tenant,
not by the architecture.

## Consequences

- Day-1 cost is **$0** (Workers Free). The Paid plan is a scale decision.
- The binding day-1 constraints are therefore:
  - **D1: 50 queries per Worker invocation** (1000 on Paid). Contract evaluation
    must batch, not query in a loop.
  - **D1: 1 database** on Free — the first per-tenant sharding forces the upgrade.
  - **DO: 13,000 GB-s compute/day, 100k requests/day** on Free.
  - **KV: 1,000 writes/day** — never on the write path.
- DO billing is on **wall-clock duration while active or idle-but-unhibernated**.
  Hibernation is a cost lever, not just a latency one: an idle DO that is
  eligible for hibernation is not billed. Design day-1 for hibernation.
- Global reach is free and immediate. **The three-month timeline is marketing
  and trust, not infrastructure.** Do not let the infra timeline delay launch.

## Explicitly not adopted, and why

- **Supabase** — rejected by the founder (bill risk). Its free tier pauses
  inactive projects after 7 days, which is fatal for a durability product where
  a cold database means a cold ledger.
- **AWS / GCP** — free tiers exist but the durable-compute primitives are not
  free at the scale needed, and the operational surface is far larger than a
  solo founder can maintain during a sprint.
- **Self-hosted open source** (PostgreSQL, Temporal, n8n) — viable later, and
  explicitly on the table as a future option. Not now: requires a host with a
  real disk and fsync, which is the exact thing the Free plan does not provide.

## Revisit triggers

- Second tenant → upgrade to Paid.
- Any user-visible outage traced to a Free-plan limit → upgrade immediately.
