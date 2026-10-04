# ADR-017: Contract evaluation stays inline; the queue is a trail, not an offload

**Date** 2026-09-30 · **Status** accepted · **Scope** ingest · **Relates to** ADR-006, ADR-013, ADR-014

> This ADR exists to correct a false claim rather than to record a new design.
> `apps/worker/src/queue.ts` opened by asserting that ingest did not evaluate
> contracts inline, and argued at length for why. It did evaluate them inline.
> The argument was sound; the architecture it described was never built.

## Context

Ingest has three candidate shapes.

| shape | what `POST /v1/ingest` waits for | blast radius of a bug |
| --- | --- | --- |
| evaluate inline | the contract consequence | wrong breach, wrong `forbids` verdict |
| enqueue and return | the durable log write | a consequence that arrives late or not at all |
| evaluate in the queue | the log write | both of the above, plus a lag the API cannot express |

The shipped code is the first. `engine/ingest.ts` matches triggers, opens
instances, evaluates `forbids`, and moves instances to `satisfied` or `breached`
inside the request. The cron `sweep` then handles anything that was still open
when its deadline passed.

The queue exists and is wired: a producer in ingest, a consumer, a DLQ, and an
`IngestSerial` Durable Object per business key. What the consumer does today is
serialise, acknowledge, retry on failure, dead-letter, and count. It does not
evaluate.

That makes the queue, as built, **decorative with respect to correctness**: it
adds cost and a false impression of offloading, while the work stays on the
request path.

## The argument for moving evaluation to the queue

It is a good argument and it is why the queue exists. Ingest latency is currently
the sum of every contract that matches the event. A customer who buys the product
more heavily makes ingest slower for themselves, which is the wrong direction,
and a single slow tenant's contract count becomes everybody's latency.

## Why it is not done yet

The offload is not a refactor. It changes what `201 Created` means, and three
things have to be answered before it can be:

1. **What does the API report?** Today a caller learns the consequence from the
   response. Tomorrow it learns "recorded". Every consumer of this API has to
   change, and the honest options are a `202` with a status resource, or a
   `201` plus polling. Choosing wrong here means a customer's integration
   silently stops seeing breaches.
2. **What happens when the queue is unavailable?** Queues have delivery limits
   and a DLQ. An event whose evaluation is dead-lettered is an event that produced
   no contract consequence and no breach. Today that cannot happen, because the
   evaluation ran before the response was sent. After the move, "we recorded your
   event and did not evaluate it" is a state the product must be able to detect,
   report, and replay. The replay path has to exist *before* the move, not after.
3. **What does the `forbids` duplicate rule depend on?** The rule that catches a
   double capture is correct today partly because the two events were evaluated
   in order within one request. Across a queue, ordering rests on
   `IngestSerial`, which orders *acknowledgement*. Ordering the acknowledgement is
   not the same as ordering the evaluation, and that gap has to be closed and
   tested first.

None of these is answered by writing code. They are answered by deciding what the
API promises.

## Decision

**Evaluation stays inline.** The queue keeps the three jobs it actually does —
durable fan-out with retry, per-business-key acknowledgement ordering, and
analytics — and is documented as doing exactly that.

**The migration is deferred, with its preconditions written down**, and
`test/queue.test.ts` asserts that the consumer does not evaluate. That assertion
is the point: it means this file cannot quietly drift back into claiming an
offload that does not exist. When the decision in (1) is made, the test is what
gets deleted, deliberately and visibly.

## Consequences

**Positive**

- A caller that gets `201` knows the event was evaluated against every matching
  contract. There is no window in which a product can say "accepted" and mean
  "we will get to it".
- No dead-lettered message can mean a silently missing breach.

**Negative, and accepted**

- Ingest latency scales with matching-contract count. Real, and unaddressed.
- The queue costs money and buys durability and ordering, not offload. That is a
  worse trade than it looks until (1) is decided, and it is stated here rather
  than hidden in a comment that argued the opposite.
- A customer who declares many contracts pays for it in ingest latency.

## What is not claimed

- That the queue reduces ingest latency. It does not, today.
- That evaluation is asynchronous. It is not.
- That the offload is a small change. It is not, and the three questions above
  are the reason.

## Revisit triggers

- The API contract is redesigned around a status resource (question 1). That
  answer unblocks the rest.
- A DLQ replay tool exists, with a test proving a replayed message produces the
  same breach it would have produced inline (question 2).
- `IngestSerial` orders evaluation, not just acknowledgement, and
  `test/queue.test.ts` proves the ordering under interleaved delivery
  (question 3).
