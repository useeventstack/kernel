# ADR-006 — D1 for state, not KV. Queues for ingest. DO for coordination.

**Date** 2026-09-29 · **Status** accepted · **Scope** storage

## Context

Three Cloudflare storage primitives are candidates for an append-only durable
event log with transactional writes. The founder asked which is right, and did
not know what the primitives are. Answering both.

### What they are

- **D1** — SQLite at the edge. SQL, indexes, real transactions. Docs state
  plainly: *"Each individual D1 database is backed by a single Durable Object"*
  and each database *"is inherently single-threaded, and processes queries one
  at a time."* Limits: 10 GB/database, 1 TB/account (Paid); 500 MB / 5 GB
  (Free). Time Travel 30 days Paid / 7 days Free.
- **Workers KV** — an eventually-consistent key/value store replicated
  globally. Reads may be served from a cache that is not yet current. Fast
  reads worldwide, no read-your-writes.
- **Durable Objects** — a single-threaded compute object with strongly
  consistent, transactional storage (SQLite-backed). The primitive D1 is built
  on, and the only one with a real serialisation point.

### The requirement

The kernel's `DurableStore::commit_prefix` must make bytes survive process
death, and the contract model requires a *durable fact that a contract was
outstanding*. Both need ordering and atomicity.

KV cannot supply either. Eventually-consistent reads mean the same key can
return two different values depending on where the read is served from. For a
system whose product is "we know what happened," that is disqualifying at the
write path. It is fine as a read cache in front of a log.

## Decision

| concern | primitive | why |
| --- | --- | --- |
| **the append-only event log (`DurableStore`)** | **Durable Objects, SQLite storage** | see below — output gates are the fsync |
| contracts, tenants, projections, breach queries | **D1** | SQL + transactions for read-heavy queries |
| ingest fan-out, decoupling | **Queues** | absorbs bursts; at-least-once is correct here |
| hot read cache, dashboard lists | **KV** | read-heavy, tolerant of staleness, globally fast |
| per-contract coordination / lock | **Durable Objects** | the only primitive with a true serialisation point |
| blobs, large payloads | **R2** | free tier, egress-free to Workers |

**One Durable Object per tenant holds that tenant's log.** D1 holds the
projected, queryable view.

### Why the log is Durable Objects and not D1

This corrects an earlier draft of this ADR, which put the log in D1. D1 is a
managed database, presented by Cloudflare as a separate network hop from
application code; it has no per-log append or prefix-truncate primitive, and
D1 read replication means there is no single global total order. It is the
right store for projections, not for the log.

Durable Objects with SQLite storage maps to every method of `DurableStore`:

| `DurableStore` | DO SQLite |
| --- | --- |
| `append` | `ctx.storage.sql.exec()` — synchronous, real SQLite |
| `commit_prefix` / `commit_all` | **output gates** — see below |
| `read_from` | SQL cursor over the log table |
| `truncate_to` | `DELETE … WHERE seq < N` inside a transaction |
| `recover` (longest valid prefix) | PITR bookmarks + a checksum column in our own log |

**Output gates are the fsync equivalent**, and this is the load-bearing fact:
"Output gates hold outgoing network messages (responses, fetch requests) until
pending storage writes complete. This ensures clients never see confirmation of
data that has not been persisted."

Crash atomicity is documented too: "In case of a machine failure, either all of
the writes will have been stored to disk or none of the writes will have been
stored to disk."

Caveats recorded honestly:
- DO single-object throughput is documented only as a *soft* 1,000 rps limit
  with no published hard number. Load-test before relying on it.
- A DO is single-threaded and *strictly serializable*, which matches
  `ExecutionPolicy::default()`'s `workers: 1`. True parallelism is unavailable
  in the log path. Accepted.

Sources: [sqlite-storage-api](https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/),
[rules-of-durable-objects](https://developers.cloudflare.com/durable-objects/best-practices/rules-of-durable-objects/),
[storage-options](https://developers.cloudflare.com/workers/platform/storage-options/),
[how-kv-works](https://developers.cloudflare.com/kv/concepts/how-kv-works/).
Full detail in [../research/cf-edge-event-log-storage.md](../research/cf-edge-event-log-storage.md).

### KV is still eliminated from the correctness path

KV "achieves high performance by caching which makes reads eventually-consistent
with writes… may take up to 60 seconds or more to be visible in other global
network locations." For a breach oracle a stale read produces a **false
breach**. If a KV read is ever used to decide whether an effect fired or a
contract is satisfied, that is a bug.

## Consequences

- Ordering and atomicity come from DO's transactional storage and output gates,
  not from application code.
- The single-writer-per-object ceiling is real (soft 1,000 rps) and accepted.
  Contention is handled by one object per tenant, not by a queue of writers.
- KV is explicitly *not* on the correctness path.
- D1 is a projection, not the source of truth. Every breach determination must
  be reproducible from the log alone — the same "log alone is sufficient"
  property the kernel already proves for its own recovery.

## Revisit triggers

- A single tenant's log exceeds DO throughput (load-test before believing 1,000 rps).
- Log compaction becomes urgent → R2 for sealed segments, DO for the head.
