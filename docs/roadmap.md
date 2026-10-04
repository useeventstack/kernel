# Release roadmap

Three months to a product people use. Infrastructure is not the constraint —
Cloudflare deploys globally on day one. **The three months are marketing, trust,
and customers.**

## Sprint 0 — the foundation (today)

Nothing user-facing ships. This sprint is the thing that makes sprint 1 possible
in days rather than weeks.

- [x] Architecture decisions recorded (ADR-001 … ADR-010)
- [x] Architecture diagram
- [x] Domain map and vocabulary
- [x] Engineering constitution
- [x] Product thesis and positioning
- [x] RFC system
- [x] CI skeleton
- [ ] Repository split decision (ADR-008 says one repo; confirm)
- [ ] First customer interview list — written, and first 5 contacted

**Exit:** a stranger can read this repository and know what is being built, why,
and on what evidence.

## Sprint 1 — day-1 product (days 1–3)

The first thing a real person can do. Deliberately small.

- [ ] Email + password auth, sessions in D1, Argon2id (ADR-010)
- [ ] **Tenant isolation test that attempts cross-tenant access** — ships
      *before* the first user, not after. Closes v1's `GAP-SEC-001` and
      `GAP-TEST-002`
- [ ] `POST /v1/events` — validate shape, dedupe, append to the per-tenant log
- [ ] Contract CRUD: a contract is a JSON document (ADR-005)
- [ ] Instance lifecycle: trigger → open → satisfied / violated / breached
- [ ] **Alarm sweep for breach detection, off the request path**
- [ ] Dashboard: contracts, open instances, breaches
- [ ] Deploy, live, on a real domain

**Exit:** a real user declares a contract, events flow in, and a deliberately
dropped event produces a visible, attributable breach.

## Sprint 2 — the first real user (days 4–7)

Not a feature sprint. A contact sprint.

- [ ] Walk 5–10 design partners through it live; watch, do not pitch
- [ ] Record every point of hesitation — that list is the roadmap
- [ ] Fix the top 3 friction points
- [ ] Pricing page: free tier, one paid tier, one design-partner tier
- [ ] Written security posture doc (what we do and do not claim)

**Exit:** at least one real person is using a real contract, and has said so.

## Sprint 3 — retention and depth (weeks 2–4)

- [ ] SDK for event emission (webhook-only is fine up to here)
- [ ] Escalation: contracts triggered by another contract's breach
- [ ] Compaction and snapshotting — the log growing unbounded is a real ceiling
- [ ] Alerting to a channel the customer already reads
- [ ] Onboarding: a working example a stranger runs in 60 seconds

## Sprint 4 — DSE, the differentiator (weeks 5–12)

Only after the product has users, and only when the target is real.

- [ ] `DurableStore` on Durable Objects (requires the Emscripten Rust target to
      stabilise — ADR-003)
- [ ] Re-establish crash coverage for the new platform. **The existing
      process-kill suite cannot run under Emscripten** (no `fork`, no `SIGABRT`),
      so a green build there proves nothing until equivalent coverage exists
- [ ] DSE opt-in per workflow, with the guarantee scoped to opted-in workflows
- [ ] Publish the proof: no rejected branch's effect escaped, on real data

## Explicitly not in the roadmap

- Multi-region active-active. Out of scope until someone pays for it.
- A visual workflow builder. A JSON contract is faster to ship and faster to
  debug; the builder is a month-3+ product if it is ever wanted.
- Replacing the customer's existing orchestrator.
