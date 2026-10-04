# Domain map

The language this product is made of. Terms here are load-bearing: a term used
in a different sense in code is a bug, not a variant.

```
                         ┌──────────────────────┐
                         │       Tenant         │  a customer account
                         │  id, name, plan      │  everything below is scoped to one
                         └──────────┬───────────┘
                                    │ 1:N
                    ┌───────────────┴───────────────┐
                    ▼                               ▼
         ┌────────────────────┐          ┌────────────────────┐
         │       User        │          │    ApiKey         │
         │  email, pw hash   │          │  prefix, hash     │
         │  session cookie   │          │  for machine      │
         └────────┬───────────┘          │  ingest           │
                  │ belongs to          └────────┬───────────┘
                  │ Tenant                       │ presents
         ┌────────┴──────────────────────────────┘
         ▼
 ┌──────────────────────────────────────────────────────────┐
 │                     Contract                             │  a DECLARATION
 │  name, version, trigger, expects[], forbids[], deadline   │  authored data
 │  schema-validated (ADR-005)                               │  never code
 └────────────────────────┬─────────────────────────────────┘
                          │ instantiates
                          ▼
 ┌──────────────────────────────────────────────────────────┐
 │                ContractInstance                           │  ONE real occurrence
 │  id, contract@version, correlation (e.g. order_id)        │  opened when the
 │  status: open │ satisfied │ violated │ breached           │  trigger arrives
 │  expectations: [{ event, within_ms, seen_at, critical }] │
 └───────┬──────────────────────────────────────┬───────────┘
         │ observed by                          │ opens
         ▼                                      ▼
 ┌────────────────────┐              ┌────────────────────┐
 │       Event        │              │      Breach        │  the missing
 │  type, payload,   │              │  which expectation  │  thing, named,
 │  received_at,     │              │  how late,         │  attributable
 │  dedupe key       │              │  correlation        │
 └────────┬───────────┘              └────────────────────┘
          │ 1:N
          ▼
 ┌──────────────────────────────────────────────────────────┐
 │                    EventLog  (per tenant)                  │  append-only
 │  DurableStore: append · commit_prefix · read_from          │  ordered
 │  truncate_to · recover(longest valid prefix)              │  durable
 └──────────────────────────────────────────────────────────┘
```

## The three distinctions that matter most

**Event vs ContractInstance vs Contract.** An *event* is something that
happened. A *contract* is a rule about what should happen next. An *instance* is
one real occurrence of that rule being tested. Confusing the last two is the
mistake that turns a monitoring tool into a confusing one: a contract is a
document, an instance is a fact.

**Satisfied / violated / breached are mutually exclusive and terminal.**

| status | meaning | terminal |
| --- | --- | --- |
| `open` | still waiting on at least one expectation | no |
| `satisfied` | all expectations met inside their windows | yes |
| `violated` | a forbidden combination occurred | yes |
| `breached` | a window elapsed, an event never arrived | yes |

Three outcomes, not two. `violated` means something wrong happened; `breached`
means something right failed to happen. They need different responses and
different on-call pages.

**The correlation key is the business key, not an internal id.** An instance is
found by `(tenant, contract, correlation)` where correlation is `order_id` —
not by a database rowid. A user looking at a breach wants to see their order.

## Vocabulary to retire from v1

v1's `.ai/glossary/` used "Integrations" inconsistently with Resource/Connection.
v2 uses: **Tenant**, **User**, **ApiKey**, **Contract**, **ContractInstance**,
**Event**, **EventLog**, **Breach**, **Expectation**. One term, one meaning.

## Where the kernel fits

The kernel does not know any of these words. It has `Plan`, `Future`, `State`,
`Effect`, and a ledger of records. A ContractInstance maps onto a plan; a
DSE-enabled execution of a contract's remediation maps onto futures. The kernel
stays ignorant of tenancy, auth, and HTTP (principle 5).
