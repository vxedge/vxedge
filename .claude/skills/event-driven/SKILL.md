---
name: event-driven
description: The daemon's event vocabulary and what may be built on it — use when emitting an event, adding a subscriber, wiring a frontend to live state, or reaching for events as a record of what happened. Covers the event/job/audit split, which is the distinction most likely to be got wrong.
---

# Event-driven design in vxedged

Code: `crates/vxedged/src/{events,jobs,audit}.rs`; RPC surface `EventService.Subscribe`,
`JobService.Watch`. Channel-choice rationale lives in `rust-patterns` (ch. 5) — this is about
*what* to send, not which primitive to send it on.

## The current state, stated plainly

`EventBus` is plumbed end to end: broadcast channel (capacity 256), monotonic `evt-%08x` ids,
`EventService.Subscribe` streams it. But the **only** events anything emits are four
`job-state` strings from `JobEngine` — `pending`, `running`, `succeeded`, `failed` — all with
`ResourceKind::Unspecified`. Nothing emits a domain fact. A frontend that subscribes today
learns that jobs change state and nothing about the vehicle.

So the bus is not a legacy to work around; it is an empty vocabulary to fill. That is the
opportunity, and the reason to get the rules right before the first ten events exist.

## Three sinks, three different questions

| Sink | Question it answers | Guarantee |
|---|---|---|
| `JobService.Watch` | "how far along is *this* operation?" | latest-state per job, no history |
| `EventService.Subscribe` | "what is happening in the system *now*?" | live fan-out, lossy, no replay |
| `audit.rs` | "what was done, by whom, against what?" | durable, ordered, the record |

Getting these wrong is the failure mode to avoid. Concretely:

- **Job progress is not an event.** `p.update(40, "zone 2/3: up")` is progress on one operation
  and belongs to that job's watch channel. Emitting it on the bus turns a shared channel into
  one operation's log and pushes every other subscriber into lag.
- **The bus is not the audit log.** It is fire-and-forget: `let _ = tx.send(event)` with no
  subscribers is a no-op, and a slow subscriber *skips* messages rather than blocking the
  sender. Never derive a record from it. If something must be recoverable after the fact, it
  goes to `audit.rs` — which is exactly why that file exists separately.
- **Provisioning never waits on a subscriber.** Emission is best-effort by construction; keep
  it that way. An event that can block `up` has made a dashboard a dependency of the testbed.

## What an event is

A **domain fact that already happened**, about a named resource. Past tense, no imperatives,
no requests.

- Good: `gateway.slice-applied`, `zone.up`, `zone.down`, `service.spawned`, `fiveg.ue-registered`.
- Bad: `zone.please-start` (a command), `progress` (not a fact), `error` (not a resource).

Every event must carry a real `resource_kind` and `resource_vxid`. The four existing job-state
events set `ResourceKind::Unspecified`, which is why they are unfilterable — a subscriber
cannot ask "everything about `node-cont-01`" and get an answer. Do not add a fifth like them.

## `kind` should stop being a bare `&str`

`EventBus::emit(kind: &str, …)` takes a free string today, so the vocabulary is whatever any
call site typed. As soon as there is a second family of events, make `kind` a closed enum with
one `as_str`, so:

- the set of events is discoverable in one place,
- a typo is a compile error rather than an event nobody is subscribed to,
- and the proto's documented vocabulary and the code's actual one cannot drift — the same
  failure `vxapi-protobuf-design` guards against on the wire.

## Ordering, and what not to promise

`seq` is a per-bus counter, so ids are monotonic within one daemon process and mean nothing
across restarts or across a federated controller. Order is only guaranteed relative to a single
emitter. Do not build anything that assumes global ordering, causality between two resources,
or that event *n* implies event *n-1* was delivered — a lagging subscriber legitimately misses
messages, and `broadcast` tells it so.

If a consumer needs "everything that happened, in order, without loss", it wants the audit log
plus a state query at subscribe time, not a bigger channel.

## Adding an event

1. Is it a fact about a resource, already true? If not, it is progress or a command.
2. Does a subscriber exist or is one named on the board? An event nobody consumes is a shape
   guess that will be wrong by the time it is needed.
3. Set `resource_kind` and `resource_vxid` — a filterable event or none.
4. Emit *after* the state change is durable, never before, or a subscriber can observe a
   transition that then fails.
5. Do not emit inside a hot loop or a per-packet path. Measurement code must not perturb the
   measurement (`rust-patterns`, vxagents rule), and the bus is a shared resource.
