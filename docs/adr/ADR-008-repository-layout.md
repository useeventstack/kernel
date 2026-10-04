# ADR-008 — Repository layout: kernel stays, product is a sibling crate

**Date** 2026-09-29 · **Status** accepted · **Scope** repository

## Context

The repo currently holds only the kernel (`src/`, `tests/`, `docs/`). The
product needs a Worker, a dashboard, contracts as data, auth, and billing. The
kernel is Rust with **zero external dependencies**, which is what makes
`cargo test` hermetic and the 212 tests meaningful.

Earlier in this project the kernel was described as "frozen." The founder
clarified: the repo is the main repo, it is still live, and it should not be
frozen. Both things are true — the kernel is *editable*, but the proven
invariants should not be casually disturbed.

## Decision

Keep one repository. Add a **sibling crate** for the product rather than a new
repo, and leave the kernel crate's dependency count at zero.

```
src/           kernel, unchanged dependencies: none
crates/        (future) product crates, may take dependencies
apps/worker/   (future) the Cloudflare Worker — TypeScript
apps/web/      (future) dashboard + marketing
docs/adr/      these records
```

**Do not add dependencies to the kernel crate.** A dependency in the kernel
would compromise the property that `cargo test` is fully offline, hermetic and
deterministic — which is the only reason the failure matrix can place a crash at
an exact record boundary rather than approximately.

## Consequences

- One repo, one `git log`, one place to look. Correct for a solo founder.
- The kernel remains independently testable and independently adoptable, which
  is what makes the upstream contribution in ADR-003 possible.
- Product dependencies are isolated in `crates/`/`apps/`, so the blast radius of
  a bad dependency is bounded.

## Revisit triggers

- The product and kernel need genuinely separate release cadences → split repos
  then, with the kernel published to crates.io (`publish = false` must change).
