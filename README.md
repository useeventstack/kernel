# ues — a durable execution kernel

[![Rust](https://img.shields.io/badge/rust-1.75%2B-000?logo=rust)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](#license)
[![dependencies: 0](https://img.shields.io/badge/deps-0-brightgreen)](#zero-dependencies)

A durable execution kernel with **first-class execution futures** and
**236 passing tests**. No dependencies, no async runtime, no clock you do not control, no
network. It is a library: it does not know what an event is, what a contract is,
or that Cloudflare exists.

This is the kernel from [useEventStack](https://useeventstack.com), published on
its own under ADR-011. The platform built on it — the hosted contract layer, the
API, the dashboard — is proprietary and lives in a separate private repository.

## What it is for

Most durable-execution code treats a retry as a function you call again. This
models execution as a **future** — a value with an identity that exists the moment
it is created, before anything has run. That is what makes speculative execution
and deterministic replay the *same* mechanism rather than two subsystems, and what
lets idempotency keys derive from `(execution, arm path, ordinal)` instead of a
runtime id that changes on every retry.

```rust
use ues::kernel::Kernel;

let kernel = Kernel::new(store);
// Execution is deterministic: the same plan produces the same trunk.
```

## The four strategies are one runtime

`deterministic`, `speculative`, `replay` and `observe` are configurations of a
single runtime, not four engines. A test requires all four to produce the same
trunk and the same durable event stream for one workload. Deterministic is the
default, and a test asserts it — nobody opts into speculation by accident.

## What is actually proven, and how you can check it

Everything below is checkable by reading the code or by running it.

- **236 tests pass.** `cargo test`.
- **0 external dependencies.** Enforced by CI, not by intention. That is
  what lets a crash land at an exact record boundary instead of approximately.
- **The crash matrix.** `tests/kill_process.rs` forks the real binary, kills it
  with `SIGABRT` at points computed as fractions of the run and spanning both
  sides of the atomic commit, then finishes the run from whatever survived on
  disk. That is what shows the log alone is sufficient to reconstruct state.
- **No clock you do not control.** `src/simulation/clock.rs`; time advances in
  ticks, so a test cannot pass because a real clock was slow.
- **Effect safety is a total function** of effect class and whether the future is
  authoritative — not a flag anyone can flip.

There is no formal verification and no Jepsen run for this project. Do not read
any implication to the contrary into this file.

## Zero dependencies

No serde, no tokio, no rand, no log. Determinism is not something you configure
into a dependency tree; it is something you cannot have by accident when you have
none. `cargo build --offline` works on a machine that has never seen a crate
registry.

## Platform notes

`tests/kill_process.rs` needs `fork`, `SIGABRT` and
`std::process::abort`. It cannot run under Emscripten or wasm32 — a green build
on a platform the suite cannot exercise is not evidence, and this repository says
so rather than claiming coverage it does not have.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — how it is put together
- [`docs/invariants.md`](docs/invariants.md) — what is enforced, and which test
  proves each one
- [`docs/experiments.md`](docs/experiments.md) — what was measured
- [`docs/adr/`](docs/adr/) — the decisions, with what each one costs
- The crate-level docs in `src/lib.rs` carry a worked example, run as a doctest
  by `cargo test`

## License

MIT — see [`LICENSE`](LICENSE).

**This licence covers the `ues` kernel only.** It does not cover the
useEventStack platform: the hosted API at api.useeventstack.com, the dashboard,
the marketing site, or any hosted service. Those are proprietary and licensed
separately — see [ADR-011](docs/adr/ADR-011-licensing.md) for why the split is
that way.

You may use, modify and commercially redistribute this kernel, including
embedding it in a product of your own. If you build something on it and want help
operating it, that is a separate conversation.
