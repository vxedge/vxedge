---
name: service-boundaries
description: When a variation point in vxedge becomes a trait, how backends are selected, and who owns errors across a seam — use when adding a backend (zone, gateway data plane, service type), when a `match` on a kind appears in more than one function, or when deciding whether an abstraction is earned. Complements rust-patterns, which covers types and errors within a module.
---

# Service boundaries

Code: `crates/vxedge-core/src/{zones,gateway,services}/`, `crates/vxedged/src/services.rs`.
Read `rust-patterns` first — this is the layer above it: not how to write a type, but where to
put a seam and when one is earned.

## The rule: two implementations, or two call sites

A variation point becomes a trait when **either** is true:

1. A second implementation exists, or is on the board with a name and a section number.
2. The `match` on its kind appears in more than one function.

Both are evidence the shape is real. One implementation and no named second is not a seam, it
is speculation — and speculative traits are worse than a `match`, because a `match` is trivial
to delete and a trait has to be un-invented.

The counterexample worth remembering: `Shell` (`gateway/netops.rs`) is a struct, not a trait,
because there has only ever been one way to run a command. It still gets the whole benefit —
netns selection and sudo escalation are *injected*, so every provisioning path is testable
unprivileged. **Inject the side effect before abstracting the algorithm.**

## What a seam owns

- **Its own error type.** The seam's error is the vocabulary its callers match on
  (`ZonePlanError`, `NetOpsError`, `ServiceBackendError`). An implementation's internal error —
  a libvirt failure, a podman exit code — is wrapped, never leaked, or callers end up matching
  on the implementation they were supposed to be insulated from.
- **Its own contract doc.** If a skill already states the contract (`zone-backend-contract`),
  the trait's doc comment cites it rather than restating it. Two statements of one contract
  drift; that is the whole reason §6.1 was deleted.
- **Nothing about selection.** Which implementation runs is a decision the *model* makes
  (`Device.backend`, `ServiceDef.service_type`). A backend that inspects the model to decide
  whether it applies has re-implemented dispatch inside itself.

## Dispatch once, at one site

```rust
// One place decides. Everything downstream holds a &dyn ZoneBackend.
fn backend_for(kind: Backend) -> &'static dyn ZoneBackend { … }
```

Static enum dispatch is fine and often better; the load-bearing part is **one** decision site.
The anti-pattern is what `zones/backend.rs` does today: `match np.backend { … }` repeated in
`node_up`, `node_down` and `node_status`, so a fourth backend edits three functions and nothing
in the code says they must agree. That is the shape to remove, not to imitate.

## Plans stay pure; backends are the impure edge

`GatewayPlan`, `NodePlan`, `FiveGPlan`, `TrafficPlan` contain no I/O and no privileges — they
unit-test anywhere. A seam must not undo that:

- A trait method takes a plan (and `&Shell`) and performs effects. It does not *compute* the
  plan. If a backend needs to decide something, that decision belongs in planning, where it is
  testable and diffable.
- Adding a backend must not change any plan. If it does, the plan was carrying an
  implementation detail — the fix is upstream, in the model or the planner.

## Idempotency belongs to the seam, not to each implementation

`up|down` compare desired against observed and converge; running `up` twice is a no-op. State
that in the trait's doc comment as a *contract*, so a new implementation inherits the
obligation rather than rediscovering it. Every idempotency bug this project has found lived on
a re-run path nobody exercised manually — the contract is what makes "test the second `up`"
obvious to whoever adds the fourth backend.

## Errors must stay actionable across the seam

The house standard (`rust-patterns`) is that an error names the failing operation, the cause,
and the fix. A seam is where that decays fastest, because the wrapper knows less than the
implementation. So:

- Wrap with context the caller can act on, not with a layer name. `"zone `node-a`: libvirt
  domain define failed: …"` beats `"backend error"`.
- Do not collapse distinct failures into one variant to keep the enum small. `MissingBundle`
  and `AmbiguousBundle` are two different mistakes with two different fixes; merging them
  would have made a mis-wired H3 arm indistinguishable from an unbuilt one.

## Before you add the trait

- Is there a second implementation, or a named one on the board? If not, stop.
- Does an existing skill state the contract? Cite it.
- Will the netns integration test still pass unchanged? A refactor that needs the test rewritten
  changed behaviour, whatever the diff says.
