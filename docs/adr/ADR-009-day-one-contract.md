# ADR-009 — The day-1 contract: an order-to-delivery chain

**Date** 2026-09-29 · **Status** PROVISIONAL — awaiting founder confirmation
**Supersedes** the open item "first contract" in [README](README.md)

> ⚠️ **This ADR contains an assumed chain.** The founder has not yet supplied a
> real e-commerce contract. The chain below is a worked example chosen because it
> is the shortest chain with real money in it. Replace the `expect` block and
> nothing else in the design changes. See [Swapping in a real chain](#swapping-in-a-real-chain).

## Context

Every architectural decision so far (contract semantics, schema-validated
declarations, DSE sequencing) is independent of *which* events form a contract.
That is by design — but it leaves the day-1 build with no concrete target, and an
unnamed contract is how a sprint produces a generic event bus nobody needs.

## The assumed chain

An order is placed. Six things are now supposed to happen. They are not
simultaneous and they are not equally urgent, which is the point: a single
timeout cannot express this.

| # | event | expect within | why that deadline |
| --- | --- | --- | --- |
| 1 | `payment.captured` | 60s | money is taken; customer is waiting |
| 2 | `inventory.reserved` | 60s | must happen before we promise stock |
| 3 | `fulfillment.requested` | 5m | warehouse queue, not instant |
| 4 | `label.created` | 15m | carrier round-trip |
| 5 | `customer.notified` | 60s | transactional email, user-visible |
| 6 | `analytics.recorded` | 30m | internal, least urgent |

Terminal states, per ADR-004 (three outcomes, not two):

- **satisfied** — all six arrived inside their own deadlines
- **violated** — an event arrived that the contract forbids (e.g. a second
  `payment.captured` for the same order)
- **breached** — a deadline elapsed with the event still missing

`payment.captured` is the one that matters most, and it is also the one that
would be handled by DSE when that ships: a retried capture must not
double-charge. That is the exact scenario the kernel's arm-path idempotency key
was designed for (README decision #4).

## The declaration

Contracts are data (ADR-005), so this is a document, not code:

```json
{
  "contract": "order-to-delivery",
  "version": 1,
  "trigger": {
    "event": "order.placed",
    "where": { "channel": "ecommerce" }
  },
  "match": { "tenant": "$tenant" },
  "expects": [
    { "event": "payment.captured",   "within": "60s",  "critical": true },
    { "event": "inventory.reserved", "within": "60s",  "critical": true },
    { "event": "customer.notified",  "within": "60s",  "critical": true },
    { "event": "fulfillment.requested", "within": "5m" },
    { "event": "label.created",      "within": "15m" },
    { "event": "analytics.recorded", "within": "30m" }
  ],
  "forbids": [
    { "event": "payment.captured", "more_than": 1, "because": "double charge" }
  ],
  "deadline": "30m"
}
```

Two details that matter and are easy to lose:

- **`within` is per-expectation, not one global timeout.** A single deadline
  cannot express "money in 60s, analytics whenever."
- **`forbids` is the violation branch.** `more_than: 1` on `payment.captured`
  catches the double-charge without waiting for anything to go missing. This is
  cheap to evaluate and catches the worst outcome earliest.

## What the day-1 system does with this

1. Ingest `order.placed` → open a **contract instance** keyed by
   `(tenant, contract, order_id)`. This row is written durably before the
   response is returned (ADR-006: D1, real transaction).
2. Each later event is matched to open instances of the same tenant.
3. On match: record the expectation, check `forbids`, and if this was the last
   outstanding expectation, mark the instance `satisfied`.
4. On a **cron/alarm sweep** (not on the request path): any instance past its
   per-expectation deadline becomes `breached` with the missing event named.

Step 4 must not depend on a user request arriving. A contract that is only
evaluated when someone looks at it is not monitoring. This is the single most
important implementation requirement in the day-1 build, and it is the thing v1
got wrong by having no breach branch at all.

## Swapping in a real chain

When a design partner supplies their real chain:

1. Replace the `expects` / `forbids` arrays. Nothing else changes.
2. If it needs a real DAG of internal actions (not just observing external
   events), that is where the kernel gets used — see ADR-002.
3. If it needs *derived* events ("this order is late, therefore
   `order.escalated`"), that is a new contract triggered by another contract's
   breach. Supported by matching on `breached` instances, not implemented in day 1.

## Open

- Is `order.placed` the right trigger, or does the customer care about a
  different first event?
- Is 6 the right size, or is a real chain 17 as originally conceived? If it is
  17, the per-expectation deadline list is the thing that has to scale, and it
  should be checked for a rendering limit in the dashboard early.
- Does `analytics.recorded` belong in a customer-facing contract at all, or is
  it noise? It is here to test the deadline model against a low-stakes event.
