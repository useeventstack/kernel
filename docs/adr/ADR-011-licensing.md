# ADR-011 — Licensing: open kernel, proprietary platform

**Date** 2026-09-29 · **Status** accepted · **Scope** licensing

## Context

The founder stated the company is not open source, while the repository shipped
an MIT `LICENSE`, `license = "MIT"` in `Cargo.toml`, and MIT framing in the
README. That is a contradiction, and it had to be resolved rather than picked
arbitrarily.

Two artifacts live in this repository and they have different strategic jobs:

1. **The kernel** (`src/`) — 212 tests, zero dependencies, honest documentation,
   a correctness claim nothing else makes.
2. **The platform** — Worker, auth, tenants, contract product, hosted service.

## Decision

**Split them.**

| component | licence | visibility |
| --- | --- | --- |
| durable execution kernel (`src/`) | MIT | public, independently adoptable |
| platform, product, hosted service | proprietary (`UNLICENSED`) | private |

This repository is the platform and is private. The kernel's MIT licence is
recorded here as intent and will be published to a dedicated public repository
when the split is executed.

## Why

**The kernel is a credibility and adoption asset.** ADR-003 already identified
the path: a tested, documented `DurableStore` for Durable Objects is the artifact
nobody has, and publishing it is how the project becomes a Cloudflare
*contributor* rather than a customer. A public kernel with a rigorous README is
what earns the demo. Proprietary, that asset is gone and the project competes on
features alone against Temporal and Airflow.

**The platform is where value is captured.** Nobody pays for hosted monitoring
because the engine is open; they pay because it works and someone maintains it.
Temporal (MIT) and Sentry (proprietary) both run open engines, and both captured
their value in the service.

A closed kernel with an open platform would be the worst of both: no adoption
surface, no credibility, and no product yet.

## Consequences

- The repository `LICENSE` is proprietary; `Cargo.toml` is `UNLICENSED`.
- The kernel must remain buildable and testable **on its own**, with no
  dependency on platform code. This is the hermeticity rule in the constitution
  and it is now also a licensing boundary.
- Splitting the repos later is a mechanical move, not a rewrite.
- Anyone reading a commit from before this ADR should note the licence changed;
  MIT grants already made are irrevocable for those commits.

## Work required to execute

- [ ] Extract `src/`, `tests/`, and the kernel docs into a public repository
- [ ] Add `LICENSE` (MIT) at the root of the extracted kernel
- [ ] Restore `license = "MIT"` in the kernel's own `Cargo.toml`
- [ ] Keep this repository private with no MIT grant
- [ ] ADR-003's upstream contribution plan depends on this

## Revisit triggers

- A competitor forks the kernel and ships it commercially. The answer is
  support, not a licence change.
- The kernel needs a paid dependency to stay maintained.
