# ADR-014 — The Durable Object log is rows in SQLite, not a file

**Date** 2026-09-29 · **Status** accepted · **Scope** storage · **Relates to** ADR-003, ADR-006, ADR-013

## Context

The sprint brief's premise, from Cloudflare's 2026-09-28 announcement of
`wasm32-unknown-emscripten`, was that the kernel's `FileStore` would work on
Durable Objects because `durable-object-fs` "stores files as rows in the Durable
Object's SQLite storage, written synchronously and committed with the Durable
Object's transaction."

The architecture that follows is right: a Durable Object's SQLite storage is a
real serialisation point, and its output gates hold responses until pending
writes commit. The specific mechanism, though, needs checking, and checking it
is the entire point of the spike.

## What the documentation says

`durable-object-fs` is a **third-party npm package** (danlapid), not a crate and
not documented anywhere on developers.cloudflare.com. Its own README, under
"Limits and semantics", says:

> `fsyncSync`/`fdatasyncSync` **validate the handle and flush its buffered
> writes (with `writeBack`)**. SQL writes use Durable Object storage output
> gates; **use `await ctx.storage.sync()` when you need an explicit asynchronous
> durability barrier.**

and, on the `writeBack` option, which is **off by default**:

> Buffer writes in memory; write whole pages on `fsync`, `close`, `flush()`, or
> past `dirtyLimit`. … With `writeBack`, data is durable once `fsync`/`close`
> returns, like an OS page cache: buffered writes are lost if the object is
> evicted first.

Reading the source, with the default (`writeBack: off`) the flush function
returns at its first line when nothing is pending — and nothing is ever pending
when `writeBack` is off. `fsync` is therefore a **no-op that returns success**.
`fdatasync` is byte-identical to it, so `sync_data` and `sync_all` are
indistinguishable. Separately, Emscripten's `NODEFS` defines no `fsync` stream
operation at all.

## Decision

**The tenant log is rows in the Durable Object's own SQLite storage. No
filesystem, no `fsync`, no `durable-object-fs`.**

`apps/worker/src/do/tenant-log.ts` inserts into a `log(seq, kind, at, body)`
table inside `ctx.storage.transactionSync`, and returns only after the row reads
back. The durability boundary is the object's storage commit, held by the
output gate — which is exactly what ADR-006 said the boundary would be, and
which the `durable-object-fs` docs also name ("written synchronously and
committed with the Durable Object's transaction").

## Consequences

**Positive**

- **No shim that reports success without persisting.** That is the failure mode
  the sprint brief called the worst thing this product could have, and it is
  the failure mode a `fsync` shim would have had. This design does not have one,
  because there is nothing to emulate: the write either is in SQLite or the
  append returns an error.
- The store is one table and one transaction, which is easier to reason about
  than a page cache and an fsync protocol.
- `sync_data` and `sync_all` being indistinguishable stops mattering, because
  the kernel's store does not call either.

**Negative / accepted**

- **The kernel cannot run on this store as `FileStore` does.** A future
  `DurableObjectsStore` in `crates/` implements the `DurableStore` _trait_
  against DO SQL. It is not `FileStore` with the disk swapped, and it must not
  claim to be.
- **No Emscripten dependency on the day-1 write path.** The Rust kernel on
  Emscripten is still a separate milestone (ADR-003), and the product does not
  wait for it. That is a deliberate narrowing: the durability boundary is
  load-bearing, the language it is written in is not.
- `durable-object-fs` may still be the right tool for a workload that genuinely
  needs a file — a world save, a PDF. It is the wrong tool for a log whose
  framing and checksums we control.

## Revisit triggers

- Cloudflare documents `durable-object-fs` as supported API, or `NODEFS` grows
  an `fsync` op that actually reaches storage. Then a `FileStore` really could
  sit on top of it, and the question becomes live again.
- A workload needs POSIX file semantics — random access, mmap, a directory
  tree — rather than an append-only byte log.
