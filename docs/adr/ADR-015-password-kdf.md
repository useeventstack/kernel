# ADR-015 — Password KDF: PBKDF2, because the platform offers nothing better

**Date** 2026-09-29 · **Status** accepted · **Scope** auth · **Relates to** ADR-010, ADR-007

> **Corrected 2026-09-30 after deployment.** The first version of this ADR said
> 600 000 iterations and ~284 ms of CPU. Both were wrong, and only deploying
> could find it. The corrections are inline and marked; nothing has been quietly
> rewritten (constitution rule 1).

## Context

ADR-010 and the sprint brief both require a **memory-hard** KDF: "Argon2id (or
scrypt), never a fast hash." Argon2id would need a WebAssembly module; scrypt
was assumed to be available through WebCrypto.

I measured the Workers runtime rather than assuming, with a probe against the
real `SubtleCrypto`:

| call | result |
| --- | --- |
| `importKey('raw', …, 'scrypt', …)` | **`NotSupportedError: Unrecognized key import algorithm "scrypt"`** |
| `deriveBits({name:'scrypt', N:16384, r:8, p:1}, …)` | same — the key never imports |
| `importKey('raw', …, 'SHA-256', …)` | `NotSupportedError` |
| `importKey('raw', …, 'PBKDF2', …)` | **ok** |
| `deriveBits` PBKDF2-SHA-256, 100 000 iterations | ok |

The evidence is a test, not a comment: `apps/worker/test/passwords.test.ts`
includes a work-factor floor and a ceiling, so lowering the count fails the
suite and asking for more than the platform accepts fails a different assertion.

## The correction: 100 000, not 600 000

The local test runtime accepted 600 000 iterations. **The deployed runtime does
not.** From `wrangler tail` on a production request:

```
NotSupportedError: Pbkdf2 failed: iteration counts above 100000 are not supported
(requested 600000).
```

Every signup returned `500` on the deployed product while passing in local
tests. This is the clearest argument in the sprint for the rule the constitution
already has: *a green build is not evidence on an unexercised platform*
(constitution rule 5).

**Correction 1.** The work factor is **100 000**, which is the platform ceiling.
`PLATFORM_MAX_ITERATIONS` in `apps/worker/src/auth/passwords.ts` is that number,
and a test asserts a stored hash never claims more.

**Correction 2.** `derive()` steps the work factor *down* rather than failing if
a future runtime lowers the ceiling, and logs loudly when it does. A login that
returns `500` because the KDF is too strong is indistinguishable from the
platform being down, and neither is acceptable.

**Correction 3.** The stored form carries the work factor that was *actually*
used (`pbkdf2-sha256$<iterations>$<salt>$<hash>`), so a verification never
re-derives at a number that was never applied to that hash.

## Decision

**PBKDF2-HMAC-SHA-256, 100 000 iterations, 16-byte random salt, 32-byte output.**
This is the strongest password KDF this platform will run today, and it is
chosen because it is what is available, not because it is good.

## Consequences

**Positive**

- A password is not recoverable by a fast hash. 100 000 HMAC-SHA-256 rounds is
  roughly five orders of magnitude more work than a single SHA-256.
- No new dependency, no WASM supply chain, no hand-rolled KDF.
- A self-describing stored form, so the cost can be raised later.

**Negative / accepted — and this is the important part**

- **PBKDF2 is not memory-hard.** scrypt and Argon2id resist GPU and ASIC
  attacks with parallelism and memory bandwidth; PBKDF2 resists them only
  through its iteration count. An attacker with specialised hardware attacks
  PBKDF2 far more cheaply than it attacks scrypt. This is a real weakening of
  the day-1 posture, forced by the platform.
- **We are pinned at the platform ceiling, 100 000.** ~~600 000 was the number
  in the first version of this ADR.~~ The platform will not give more, so this
  is the maximum strength available, not a chosen trade-off.

### The plan consequence is weaker than the first version claimed

The first version argued that a login costs ~284 ms and therefore "**cannot run
on the Free plan**" (10 ms CPU per invocation), making $5/month necessary.

**That argument is withdrawn.** The real number at the platform ceiling is
**tens of milliseconds**, measured locally at ~47 ms for 100 000 iterations.
That still exceeds the Free plan's documented 10 ms per-invocation CPU budget,
so *some* paid plan is needed for login — but the margin is 5×, not 28×, and I
have not observed a Free-plan request being killed. The honest claim is:

> A login at this work factor does not fit in the Free plan's documented 10 ms
> CPU budget. It fits comfortably on the $5/month Paid plan, which is what this
> deployment runs on.

The account is on **Workers Paid ($5/month)**, and that is a settled fact rather
than an intention. The evidence is in the running deployment, because every
capability this product depends on is itself Paid-only: Durable Objects with
SQLite storage (ADR-014), Cloudflare Queues with a dead-letter queue, cron
triggers, and Analytics Engine. There is a producer and a consumer attached to
`useeventstack-ingest` right now. A Free-plan account could not have created any
of them.

Paid here is a comfortable fit rather than a forced upgrade, and it should not
be described as anything else. The margin at the platform ceiling is about 5×
the Free plan's per-invocation CPU budget, not 28×.

## Revisit triggers

- **Cloudflare adds scrypt to `SubtleCrypto`.** `importKey` stops throwing and
  `deriveBits` accepts it. Move the derivation, keep the encoded format's
  algorithm field, and re-hash on next login.
- **The PBKDF2 iteration ceiling rises.** `PLATFORM_MAX_ITERATIONS` is the only
  constant that needs to move, and the ceiling test is the thing to update.
- **Argon2id via a vetted WASM module becomes acceptable.** It is the better
  answer on the merits; a WASM KDF is a supply-chain decision, not just a
  technical one.

## What is not claimed

- That this is as strong as scrypt or Argon2id. It is not, and the difference
  is a hardware attacker's advantage, not a theoretical one.
- That 100 000 iterations is enough against a determined attacker with GPUs. It
  is the platform's ceiling, not a security judgement.
- That the platform cannot support a memory-hard KDF. It cannot support one
  *without shipping WASM*, which is a different statement.
