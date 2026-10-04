# ADR-010 — Auth: self-hosted on D1, with a seam for a managed provider

**Date** 2026-09-29 · **Status** accepted · **Scope** product surface

## Context

The day-1 product is multi-tenant: contracts belong to a tenant, the event log
is per-tenant (ADR-006), and the dashboard shows only your contracts and
breaches. That requires real identity and real tenant isolation.

Constraints: no paid tooling (founder), free tiers only, live in days, and
tenant isolation must not be a retrofit — v1 shipped with `GAP-SEC-001`
("tenant isolation not tested", P0, never closed) and `GAP-TEST-002` ("no
security boundary tests", P0, never closed). Getting this wrong the same way
twice is not acceptable.

## Decision

**Email + password authentication, implemented in the Worker, with sessions in
D1 and passwords hashed with a memory-hard KDF. Behind a `UserStore` trait so a
managed identity provider can be swapped in later without touching callers.**

Rejected for day 1:

- **Clerk / Auth0 / WorkOS** — all have free tiers, but each adds a vendor whose
  free tier changes without notice, and each puts an identity boundary between
  the user and our own tenant table. For a product whose entire claim is
  "we know exactly what happened and to whom," owning identity is worth more
  than the convenience.
- **GitHub OAuth only** — fine for a developer tool's day 1, but the moment a
  non-engineer is a customer (per ADR-001's buyer discussion, the champion is
  often a founder) it forces a GitHub account. Keep it as a *second* method,
  not the only one.
- **Workers Auth** — a plausible native option; requires evaluation against the
  tenant model before committing.

## The part that must not be skipped

1. **Passwords** — Argon2id (or scrypt), never a fast hash. Cloudflare Workers
   supports WebCrypto; Argon2 is available via WASM. Cost is per-login, which is
   far cheaper than per-event and is exactly the tradeoff you accept when the
   login surface is small.
2. **Sessions** — a random 256-bit token, stored **hashed** in D1, in an
   `HttpOnly; Secure; SameSite=Lax` cookie. Store the hash, not the token, so a
   database read cannot mint a session.
3. **Tenant isolation is enforced in one place.** A single
   `requireTenant(session)` gate at the edge of every route. v1's P0 gaps were
   not "missing isolation" — they were "no test proving it." So:
4. **Isolation gets tests before it gets users.** Two tenants, two contracts,
   one attempting to read the other's breach. That test is the deliverable that
   closes v1's `GAP-SEC-001` and `GAP-TEST-002`, and it is worth more than a
   week of features.

## Consequences

- No third-party identity vendor, no vendor free-tier cliff.
- Argon2 in WASM adds bundle weight; acceptable on a login path.
- Writing auth is ~1 day. Getting isolation *tested* is the real work, and it
  is the part v1 skipped.
- The `UserStore` seam keeps a managed provider possible later without a
  migration, so this is not a dead end.

## Revisit triggers

- Enterprise SSO requirement → add SAML/OIDC, keep the seam.
- More than a few hundred logins/day → reconsider managed auth for rate-limit
  and abuse-response reasons.
