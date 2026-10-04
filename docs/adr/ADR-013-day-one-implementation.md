# ADR-013 — Day-one implementation: modules, routes, schema

**Date** 2026-09-29 · **Status** accepted · **Scope** platform
**Relates to** ADR-005, ADR-006, ADR-007, ADR-009, ADR-010, ADR-012

## Context

Phases 6–10 need decisions that are not in the ADRs: where each piece of code
lives, what the HTTP surface is, what the D1 schema is, and — the one that
actually shapes the product — **what runs the breach sweep**.

Three spec/reality conflicts were found while reading, and are resolved here
rather than silently.

## Conflict 1 — "D1, per tenant" is not what the sprint's own schema is

The brief's §4 table says `contracts, instances, breaches, users, sessions` live
in **D1, per tenant**, and separately says the $5/mo Paid plan is needed almost
immediately because "Free allows one D1 database and this design is per-tenant".

The brief's own D1 schema has **no per-tenant database**: every one of the seven
tables carries a `tenant_id` column. Those two statements contradict each other,
and Free plan (ADR-007: one D1 database, 50 queries per invocation) is enough
for exactly the schema that was written.

**Chosen:** **one D1 database, `tenant_id` on every row**, and the _Durable
Object_ is the per-tenant unit — one object per tenant, holding that tenant's
log. This keeps Free viable for day one, keeps the isolation test meaningful
(the gate is `requireTenant`, not a database boundary), and matches the schema
that was actually specified.

Consequence recorded honestly: tenant isolation is enforced by **application
code**, not by the database. That is weaker than physical separation and is
exactly why the isolation test is part of the deliverable (constitution rule 6,
ADR-010) rather than a follow-up. If a future customer needs physical
separation, that is one D1 database per tenant plus a routing table, and
nothing above the store changes.

## Conflict 2 — the breach sweep cannot depend on the Emscripten spike

The brief requires breach detection to be a timer, "a DO alarm or cron
trigger", independent of user traffic. It also makes the Emscripten/DO spike
Phase 3 and the DO-backed store Phase 4 — i.e. the sweep's host is the _riskiest
part of the sprint_.

**Chosen: the sweep is a Worker cron trigger, and the DO alarm is not used for
it.** Reasons, in order:

1. `expectations.due_at` and `expectations.seen_at` are D1 columns with an index
   on `due_at`. A sweep is a single indexed query; it does not need a serialised
   object to be correct.
2. If the Emscripten path fails (a real possibility — the target is a
   one-day-old preview, and the local toolchain does not have it installed),
   the product's single most important requirement must not be the thing that
   failed with it.
3. A DO per tenant that must be woken to discover breaches is a cost
   (ADR-007: DO bills wall-clock duration) and a hibernation-correctness
   problem, for no benefit at day-one volume.

**Consequence:** the sweep is a _projection_ over D1, and it must be
reconstructible from the log. That is the ADR-006 property ("every breach
determination must be reproducible from the log alone"), and it is what keeps
the sweep honest rather than authoritative: D1 can be rebuilt from the log, so a
bug in the sweep corrupts a projection, not a fact.

The DO's role is the **write path**: append + commit before the response
acknowledges (ADR-006 output gates). That is where the real durability boundary
is, and that is the part that needs Phase 3/4.

## Conflict 3 — "no logging in kernel" and the brief's "one line of work"

Recorded in ADR-012 §2. The plan is fixed; the strategy is a configuration. No
new conflict, but the brief's table is corrected there.

---

## Layout

```
src/                kernel (Rust)            zero dependencies — CI enforces
crates/do-store/    DO-backed DurableStore    Rust, dependency-free, optional
apps/worker/        product API + sweep       TypeScript
apps/web/           dashboard + marketing     static assets
migrations/         D1 SQL
```

`crates/do-store` depends on the kernel **by path** and on nothing else, so
`Cargo.lock` gains no package (constitution rule 3, ADR-008). It is only
compiled on the Emscripten target; on `x86_64` the module is behind a
`cfg(target_family = "wasm")` and the crate builds empty. The **contract tests**
for it run on `x86_64` against a portable in-memory backend, so a store that
cannot pass the kernel's semantics fails on the machine that runs CI, not on the
platform that does not.

## HTTP surface

All under one Worker. Content type `application/json` in and out. Errors are
`{"error": {"code", "message", "detail"}}` — `code` is stable, `message` is for
a human, and no message ever contains a secret or a stack.

| method | path                | auth                   | purpose                               |
| ------ | ------------------- | ---------------------- | ------------------------------------- |
| `POST` | `/v1/auth/signup`   | none                   | create tenant + first user            |
| `POST` | `/v1/auth/login`    | none                   | session cookie; rate-limited          |
| `POST` | `/v1/auth/logout`   | session                | drop session                          |
| `GET`  | `/v1/auth/me`       | session                | who am I                              |
| `GET`  | `/v1/contracts`     | session                | list contracts **of my tenant**       |
| `POST` | `/v1/contracts`     | session                | declare a contract (schema-validated) |
| `GET`  | `/v1/contracts/:id` | session                | one contract, with its strategy       |
| `GET`  | `/v1/instances`     | session                | instances, filter `?status=`          |
| `GET`  | `/v1/instances/:id` | session                | one instance, with expectations       |
| `GET`  | `/v1/breaches`      | session                | breaches, newest first                |
| `GET`  | `/v1/events`        | session                | recent events for the tenant          |
| `POST` | `/v1/events`        | session **or** API key | ingest                                |
| `GET`  | `/health`           | none                   | liveness; no tenant data              |

`/v1/events` is the only machine-facing route. It is the only one that accepts
an API key, and the key resolves to exactly one tenant.

**The gate.** Every authenticated route resolves its tenant in one place,
`requireTenant(request)`, and receives a `Session` whose `tenant_id` is the
_only_ source of tenant identity. A route that reads a tenant id from a body, a
query string, or a path segment is a bug, and the isolation test is written to
fail if that changes.

## D1 schema

Exactly the sprint's schema, with the indexes the query paths need:

```sql
tenants(id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL);

users(id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL REFERENCES tenants(id),
      email TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL,
      created_at INTEGER NOT NULL);
CREATE INDEX users_tenant ON users(tenant_id);

sessions(id_hash TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id),
         tenant_id TEXT NOT NULL REFERENCES tenants(id),
         created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
CREATE INDEX sessions_user ON sessions(user_id);

api_keys(id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL REFERENCES tenants(id),
         prefix TEXT NOT NULL UNIQUE, key_hash TEXT NOT NULL,
         created_at INTEGER NOT NULL, last_used_at INTEGER);
CREATE INDEX api_keys_tenant ON api_keys(tenant_id);

contracts(id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL REFERENCES tenants(id),
          name TEXT NOT NULL, version INTEGER NOT NULL, definition TEXT NOT NULL,
          execution TEXT NOT NULL DEFAULT 'deterministic'
            CHECK (execution IN ('deterministic','speculative','replay','observe')),
          enabled INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL,
          UNIQUE(tenant_id, name, version));
CREATE INDEX contracts_tenant ON contracts(tenant_id, name);

contract_instances(id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL REFERENCES tenants(id),
                   contract_id TEXT NOT NULL REFERENCES contracts(id),
                   contract_version INTEGER NOT NULL, correlation TEXT NOT NULL,
                   execution TEXT NOT NULL,
                   status TEXT NOT NULL
                     CHECK (status IN ('open','satisfied','violated','breached')),
                   opened_at INTEGER NOT NULL, closed_at INTEGER,
                   UNIQUE(tenant_id, contract_id, correlation));
CREATE INDEX instances_tenant_status ON contract_instances(tenant_id, status);
CREATE INDEX instances_contract ON contract_instances(contract_id);

expectations(id TEXT PRIMARY KEY, instance_id TEXT NOT NULL REFERENCES contract_instances(id),
             event_type TEXT NOT NULL, within_ms INTEGER NOT NULL,
             due_at INTEGER NOT NULL, seen_at INTEGER, critical INTEGER NOT NULL DEFAULT 0);
CREATE INDEX expectations_due ON expectations(due_at);
CREATE INDEX expectations_instance ON expectations(instance_id, event_type);
CREATE INDEX expectations_open ON expectations(instance_id, seen_at);

events(id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL REFERENCES tenants(id),
       event_type TEXT NOT NULL, payload TEXT NOT NULL, received_at INTEGER NOT NULL,
       correlation TEXT, dedupe_key TEXT);
CREATE UNIQUE INDEX events_dedupe ON events(tenant_id, dedupe_key)
  WHERE dedupe_key IS NOT NULL;
CREATE INDEX events_tenant_time ON events(tenant_id, received_at);
CREATE INDEX events_correlation ON events(tenant_id, correlation, event_type);

breaches(id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL REFERENCES tenants(id),
         instance_id TEXT NOT NULL REFERENCES contract_instances(id),
         expectation_id TEXT, kind TEXT NOT NULL CHECK (kind IN ('missing','forbidden')),
         event_type TEXT, detail TEXT, observed_at INTEGER NOT NULL);
CREATE INDEX breaches_tenant_time ON breaches(tenant_id, observed_at);
CREATE UNIQUE INDEX breaches_once ON breaches(instance_id, expectation_id, kind)
  WHERE expectation_id IS NOT NULL;
```

Three decisions inside that schema:

1. **`CHECK` on `execution` and `status`.** A strategy that is not one of the
   four cannot be stored. The agreement property is worth more than the
   flexibility to invent a fifth name, and a bad value should fail at the write,
   not at 02:00 (ADR-005).
2. **`breaches_once` is a unique index on `(instance_id, expectation_id, kind)`.**
   The sweep is idempotent by construction rather than by a read-then-write
   check: a repeated alarm that tries to record the same breach twice loses the
   second insert. This is the cheapest correct answer to "alarms re-run; the
   sweep must converge" and it costs no D1 query.
3. **`events_dedupe` is partial and unique.** A retried ingest of the same event
   with the same `dedupe_key` is a no-op at the database, not at the application
   layer. At-least-once delivery is correct here (ADR-006), so dedupe belongs
   where the duplicate arrives.

## Query budget

Free plan: **50 D1 queries per Worker invocation** (ADR-007). Every list route
is one query with a join or a correlated `IN`, never a loop. The ingest path is
five statements, fixed, and the sweep is three. The dashboard's first paint is
three queries, issued concurrently.

## Auth

ADR-010, with the choices the brief left open:

- **Password hash: scrypt via WebCrypto.** Argon2id needs a WASM module and a
  custom parameter tuning; WebCrypto's scrypt is native, and scrypt is a
  memory-hard KDF as ADR-010 requires. Not a fast hash. If Argon2 is wanted
  later it is a drop-in behind the same `UserStore` trait, which is what the
  trait is for.
- **Session token:** 256 bits from `crypto.getRandomValues`. Stored as
  **SHA-256**, in D1. The plaintext token exists only in the cookie.
  `HttpOnly; Secure; SameSite=Lax; Path=/`.
- **Login rate limit:** one Durable Object, `login-throttle`, keyed by
  `sha256(email) | ip`. Free plan allows Durable Objects; a DO is the only
  primitive here that gives a strongly consistent counter, and a counter in D1
  would spend the 50-query budget on abuse protection.

## What is a "commit" here

The response to `POST /v1/events` returns only after the DO's storage write has
committed (ADR-006 output gates). If the DO cannot be reached, the route returns
`503` with a retry hint. It does **not** buffer into KV and it does not return
`202`, because a `202` for a write that is not durable is the lie ADR-006 and
principle 1 exist to prevent.

## Consequences

- **Free plan is viable for day one** (Conflict 1). The upgrade trigger is the
  Free plan's 50-query limit or DO compute, not a second tenant.
- The sweep does not depend on the riskiest part of the sprint (Conflict 2).
- Tenant isolation is an application-level invariant, proven by a test.
- Adding a second `ExecutionStrategy` requires an ADR, because `CHECK` and
  `strategy.rs` both have to move in the same commit. That is the intended cost.

## Revisit triggers

- Emscripten works and DO-backed execution is faster than D1 for the log →
  revisit Conflict 2 with measurements rather than arguments.
- A second tenant needs physical separation → one D1 database per tenant.
- More than a few hundred logins/day → move auth behind the `UserStore` seam.
