# ADR index

Architecture Decision Records for useEventStack v2. Each ADR states context,
the decision, and its consequences, including the consequences that are
unpleasant. New decisions supersede rather than rewrite: edit the index below
when a record is replaced.

| ADR | Title | Status | Date |
| --- | --- | --- | --- |
| [001](ADR-001-platform.md) | Platform: Cloudflare, fully edge, from day 1 | accepted | 2026-09-29 |
| [002](ADR-002-dse-sequencing.md) | DSE deferred; breach detection is the day-1 product | accepted | 2026-09-29 |
| [003](ADR-003-emscripten.md) | Emscripten Rust target: adopt later, contribute back | accepted | 2026-09-29 |
| [004](ADR-004-contract-semantics.md) | Contract semantics: saga, not distributed transactions | accepted | 2026-09-29 |
| [005](ADR-005-contract-declaration.md) | Contracts are schema-validated data, not types | accepted | 2026-09-29 |
| [006](ADR-006-storage.md) | D1 for state, not KV; Queues for ingest; DO for coordination | accepted | 2026-09-29 |
| [007](ADR-007-infrastructure-budget.md) | Free tiers only, with stated ceilings and upgrade triggers | accepted | 2026-09-29 |
| [008](ADR-008-repository-layout.md) | Kernel stays, product is a sibling crate | accepted | 2026-09-29 |
| [009](ADR-009-day-one-contract.md) | The day-1 contract: an order-to-delivery chain | **PROVISIONAL** | 2026-09-29 |
| [010](ADR-010-auth.md) | Auth: self-hosted on D1, with a seam for a managed provider | accepted | 2026-09-29 |
| [011](ADR-011-licensing.md) | Licensing: open kernel, proprietary platform | accepted | 2026-09-29 |
| [012](ADR-012-execution-strategies.md) | `ExecutionStrategy`: the execution axis is a value, not a build flag | accepted | 2026-09-29 |
| [013](ADR-013-day-one-implementation.md) | Day-one implementation: modules, routes, D1 schema, and the breach sweep | accepted | 2026-09-29 |
| [014](ADR-014-durable-object-log.md) | The Durable Object log is rows in SQLite, not a file: `fsync` is a no-op on Emscripten | accepted | 2026-09-29 |
| [015](ADR-015-password-kdf.md) | Password KDF: PBKDF2. **Superseded** — passwords were removed in favour of passkeys; the KDF and every stored hash are gone | superseded | 2026-09-29 |
| [016](ADR-016-emscripten-spike-result.md) | Rust Durable Objects build on Emscripten; `std::fs` is not durable, DO SQLite is | accepted | 2026-09-29 |
| [017](ADR-017-evaluation-inline.md) | Contract evaluation stays inline; the queue is a durable trail, not an offload | accepted | 2026-09-30 |
| [018](ADR-018-workflows-runtime.md) | Production durable execution runs on Cloudflare Workflows; the Rust kernel remains the invariant/correctness proof suite | accepted | 2026-10-02 |

ADR-009 is provisional: it contains an assumed e-commerce chain pending founder
confirmation. It is deliberately isolated so a real chain replaces one JSON
block without touching the architecture.

ADR-012 and ADR-013 record three spec/reality conflicts and the resolutions:
that a strategy is a *configuration* and not a different program, that "same
events" means contract events rather than external effects, and that D1 is one
database with a `tenant_id` column rather than one database per tenant.

## Resolved

| Topic | Resolution |
| --- | --- |
| D1 vs DO vs KV | [ADR-006](ADR-006-storage.md) — DO per tenant for the log, D1 for contracts/queries, KV read-cache only and never on the correctness path |
| First contract | [ADR-009](ADR-009-day-one-contract.md) — assumed chain, needs confirmation |
| Platform | [ADR-001](ADR-001-platform.md) — Cloudflare, fully edge |
| Cost | [ADR-007](ADR-007-infrastructure-budget.md) — **$0 day 1**; DO is available on Free with SQLite storage |
| Auth | [ADR-010](ADR-010-auth.md) — self-hosted email+password, sessions in D1, isolation tested before users |
| DSE timing | [ADR-002](ADR-002-dse-sequencing.md) — deferred, breach detection ships day 1 |
| Execution strategy | [ADR-012](ADR-012-execution-strategies.md) — four strategies, one runtime; agreement is a test, not a convention |
| Sweep host | [ADR-013](ADR-013-day-one-implementation.md) — Worker cron over D1, deliberately not the Durable Object |
| DO storage shape | [ADR-014](ADR-014-durable-object-log.md) — rows in DO SQLite, not a file; `fsync` is a no-op on Emscripten |
| Password KDF | [ADR-015](ADR-015-password-kdf.md) — PBKDF2-SHA-256 at **100 000**. History, not posture: there are no passwords. The KDF module was deleted and every stored hash nulled by migration `0005`; see `docs/apps.md` and the passkey routes in `openapi.json`. |
| Paid plan trigger | [ADR-015](ADR-015-password-kdf.md) — argued that password hashing forced a paid plan. **Moot**: there is no password to hash, and a passkey ceremony costs single-digit milliseconds of CPU. The $5/month plan is still deployed, for Durable Objects and headroom rather than for login. |

## Still open

| Topic | Question | Blocking |
| --- | --- | --- |
| Ingest | SDK, webhook, or both? | day-1 scope |
| Ingest offload | What does `POST /v1/ingest` report once evaluation is asynchronous? [ADR-017](ADR-017-evaluation-inline.md) defers the move until this is answered, along with DLQ replay and evaluation ordering | DSE milestone |
| DSE target | Emscripten stabilise vs. a conventional host | DSE milestone |
| Emscripten provenance | Upstream `wasm-bindgen`/Tokio landing is asserted by the Cloudflare blog but unverified in a `cloudflare/` repo — see [ADR-003](ADR-003-emscripten.md) | DSE milestone |
| Name | Is `useEventStack` still the right commercial name? | post-launch |
