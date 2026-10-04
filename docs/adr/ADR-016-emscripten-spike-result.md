---
title: "ADR-016 — The Emscripten spike: Rust Durable Objects work, the filesystem does not"
date: 2026-09-29
status: accepted
relates_to: [ADR-003, ADR-014]
---

# ADR-016 — The Emscripten spike result: the runtime works, `std::fs` does not

**Date** 2026-09-29 · **Status** accepted · **Scope** kernel runtime target

## What was built and run

A Rust Durable Object on `wasm32-unknown-emscripten`, built and executed
locally in `workerd`. Not a hello-world: a Durable Object with three Durable
Objects' worth of bindings, a `#[event(fetch)]` handler, and a SQL-backed
storage schema.

| component | version |
| --- | --- |
| rustc | 1.98.1 |
| emscripten | 6.0.10 (emsdk), provisioned by `worker-build` |
| wasm-bindgen | 0.2.129 |
| `worker` | 0.8.7 |
| `worker-build` | 0.8.7 |
| target | `wasm32-unknown-emscripten` |

## Findings, in order of importance

### 1. `std::fs` writes report success and persist zero bytes

```
FS_WRITE sync_data=Ok sync_all=Ok size=0
FS_READ 0 bytes:
```

`write_all` returned `Ok`. `flush` returned `Ok`. **`sync_data` returned `Ok`.
`sync_all` returned `Ok`.** And the file was **empty**. The `File::create` had
made a real file (the later read found it rather than reporting absence), so
this is not "no filesystem" — it is **a write path that returns success and
discards the data.**

This is the single worst failure mode a durability product can have, and it is
the default behaviour of this target. A `DurableStore` built on `std::fs` here
would be a store that reports `commit_all()` succeeded for every call.

### 2. Adding `-sNODERAWFS` does not fix it — it stops the Worker booting

```
Uncaught Error: The process.binding method is not implemented
  at node-internal:public_process:193:11 in binding
```

`worker-build` does not add `-sNODERAWFS`, and adding it via `build.rs` moves
the failure from "silently wrong" to "does not start". Working past this needs
the hand-written `addToLibrary` JavaScript that the one reference
implementation (Pumpkin, a Minecraft server) carries — a JS library that rebinds
`NODEFS`, maps `emSetImmediate` to `scheduler.wait(0)`, patches `fs.constants`,
and sets `FS.ignorePermissions`. That is not a build flag; it is a port.

### 3. `state.storage().sql()` works from Rust

```
SQL_WRITE ok at=1790732645346
SQL_STATS rows=2
```

Synchronous SQL against the Durable Object's own SQLite storage, from a
`bin`-target Rust module, inside a Durable Object. This is the store ADR-014
chose, and it needs no filesystem and no shim.

### 4. The toolchain is real, and the build is slow

`worker-build --emscripten --release` provisions emsdk and wasm-bindgen itself.
Building **`worker-build` itself** took **9 minutes 12 seconds at `-j 1`** on
this machine, and was OOM-killed twice at default parallelism. Anyone budgeting
for this should know the toolchain is a multi-gigabyte install and the build is
not cheap on a laptop.

## Decision

**The kernel's `DurableObjectsStore` will be built on `state.storage().sql()`,
not on `std::fs`.** ADR-014 reached the same conclusion from the documentation;
this reaches it from execution.

`std::fs` on this target is **refused**, not worked around. There is no shim in
this repository that pretends an fsync persisted anything, and there never will
be one: a shim that reports success without persisting is indistinguishable from
a real store until the moment it matters, which is the moment it is least
affordable.

## Consequences

- **Phase 4 of the sprint is unblocked in principle**: a `DurableStore` over DO
  SQL is a normal implementation of ten methods, and the trait was designed for
  exactly this.
- **Phase 5's crash coverage on this platform is still not established.** A
  Durable Object restart is not a `SIGABRT`, and `tests/kill_process.rs` cannot
  run here. Nothing in this spike changes that, and no claim about crash
  recovery on Durable Objects is made anywhere in this repository.
- **The Emscripten runtime is not on the day-1 critical path**, and was not
  needed for the deployed product. The log is a Durable Object with SQL written
  in TypeScript (ADR-014); this is the milestone that would put the *kernel* on
  the platform.
- **The dependency-free rule is preserved.** `crates/` will depend on the kernel
  by path and on nothing else, so `Cargo.lock` still lists one package.

## Revisit triggers

- Emscripten grows a `fsync` op that reaches storage, **and** a supported
  `durable-object-fs` ships in Cloudflare's own documentation rather than a
  third-party repository. Then a `FileStore` is genuinely portable again.
- The Rust-on-Emscripten target reaches a stable release, which would make
  porting the kernel a scheduled piece of work rather than a spike.
