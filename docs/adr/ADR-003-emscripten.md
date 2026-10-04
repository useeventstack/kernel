# ADR-003 — Emscripten Rust target: adopt later, contribute back

**Date** 2026-09-29 · **Status** accepted · **Scope** DSE runtime target

## Context

Cloudflare announced the `wasm32-unknown-emscripten` Rust target for
`wasm-bindgen` on **2026-09-28** — a first public *experimental preview*,
one day before this ADR was written. The post reports:

- Native Rust, including Tokio-based applications, running in Workers.
- A `durable-object-fs` backend that "stores files as rows in the Durable
  Object's SQLite storage," written synchronously and committed with the
  Durable Object's transaction.
- A Rust-native Minecraft server (Pumpkin) running inside a Durable Object,
  surviving restarts.

This appears to be exactly what the kernel's `FileStore` needs: a real,
transactional, synchronous durability boundary. It revises an earlier
conclusion in this project that Durable Objects could not host the kernel.

## Decision

**Do not build the production DSE runtime on an experimental preview.**

Adopt the Emscripten target when it stabilises. In the meantime:

1. Keep the kernel platform-agnostic (`DurableStore` is a 10-method sync trait).
2. Track the preview. Re-evaluate when it reaches 1.0 or Cloudflare documents
   it as supported.
3. **Contribute upstream rather than depend passively.** The most valuable
   artifact is not the fs backend (Cloudflare already built it) but a *tested,
   documented `DurableStore` implementation for Durable Objects storage* — the
   thing nobody has, with a proof that the durability invariants hold.

## Consequences

**Positive**
- DSE remains achievable, with a genuinely good long-term target.
- The contribution path is real: a reusable DO-backed store is useful to
  Cloudflare's own users and establishes credibility as a contributor rather
  than a customer.
- No production durability claim rests on a one-day-old preview.

**Negative / accepted**
- DSE is not available on day 1 or day 30. (Accepted — see ADR-002.)
- Effort spent on the store port is partly speculative. Mitigated: the trait is
  small and the port is genuinely useful locally regardless.

## What must be rebuilt for the new platform

`tests/kill_process.rs` **cannot run under Emscripten**: no `fork`, no
`SIGABRT`, no `std::process::abort`. It is 30s of the ~40s test suite and is
currently the strongest evidence that the durability claims hold. Before
trusting the kernel on any new platform, equivalent coverage must be
re-established natively (simulated abrupt termination against a DO-backed
store). A green build on a platform the suite cannot exercise is not evidence.

## Revisit triggers

- Emscripten target hits 1.0, or Cloudflare publishes DO fs as supported API.
- A customer requires the DSE guarantee in production.
