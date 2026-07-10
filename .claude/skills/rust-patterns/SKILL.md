---
name: rust-patterns
description: Industry-grade Rust patterns for vxedge repos — use when writing or reviewing any Rust code here (new modules, APIs, async code, error types, tests). Grounded in the Microsoft Rust Patterns book (microsoft.github.io/RustTraining/rust-patterns-book) and this codebase's established conventions.
---

# Rust patterns for vxedge code

Reference: [Microsoft Rust Patterns & Engineering How-Tos](https://microsoft.github.io/RustTraining/rust-patterns-book).
Chapter numbers below refer to it. These are the house rules — deviations need a comment
explaining why.

## Type-level design (ch. 3: Newtype & Type-State)

- **Parse, don't validate.** Raw input (YAML, RPC payloads) crosses exactly one boundary into
  a validated type, and everything downstream trusts that type. `ExperimentManifest::from_yaml`
  and `GatewayPlan::from_manifest` are the canonical examples: a `GatewayPlan` existing *means*
  the manifest was valid and provisionable.
- **Plans are pure data.** Desired-state types (`GatewayPlan`) contain no I/O and no privileges,
  so they unit-test anywhere. Side effects live in separate apply/observe functions.
- Use newtypes when two values share a representation but not a meaning (vxids vs interface
  names vs slice ids are all `String` today — newtype them as APIs harden).
- Enums over booleans for verbs and modes (`Verb::{Apply,Up,Down}`, `SliceMode`), giving
  exhaustive matches when a variant is added.

## Error handling (ch. 10)

- **Libraries use `thiserror`; never `anyhow` in a library crate.** Error enums are typed per
  domain (`PlanError`, `NetOpsError`, `TelemetryError`) so callers can match.
- **Errors must be actionable.** The standard is the CAP_NET_ADMIN error: name the failing
  operation, state the cause, give the fix ("run under sudo, or setcap …"). An error message
  that doesn't tell the user what to do next is a bug.
- `unwrap`/`expect` only in tests, or with an `expect("invariant …")` message proving the
  invariant (e.g. "validated: vlan slice has vlan id"). Never on I/O results.
- Fail honestly: a job that couldn't finish reports FAILED with the real error — never a
  hollow SUCCEEDED (see the gateway Up verify step).

## Async & concurrency (ch. 5, 6, 9, 16)

- **Channel choice is semantic** (ch. 5): `tokio::sync::watch` for latest-state observation
  (job progress), `broadcast` for fan-out events (event bus — slow consumers skip, never
  block), `mpsc` to bridge into response streams.
- **Blocking work leaves the runtime**: syscalls/process exec go through
  `tokio::task::spawn_blocking` (gateway provisioning is the template).
- **Mutex choice** (ch. 9): `std::sync::Mutex` for tiny critical sections that never cross an
  `.await` (ProbeState); `tokio::sync::Mutex` only when a guard must live across awaits.
  Hot-path totals are lock-free atomics.
- Aborted tasks (`handle.abort()`) must leave shared state consistent — design updates to be
  atomic per message, as in the probe receiver.

## Serde (ch. 11)

- **Authoritative schema ⇒ `deny_unknown_fields`** (vxedge-core manifests: unknown keys are
  errors — the CLI is the only validator).
- **Consumer view ⇒ tolerant partial struct** (vxtelemetry `ManifestView`: read your slice,
  ignore the rest, never duplicate validation).
- Field renames (`type` → `node_type` etc.) via `#[serde(rename)]`; kebab/lowercase enum
  representations match the documented YAML schema exactly.

## Testing (ch. 14)

Three tiers, all present, all required for new subsystems:
1. **Unit** — pure logic (plan building, parsers, pacing math).
2. **Unprivileged e2e in plain `cargo test`** — loopback probe run, fake-agent ingest+collect.
3. **Privileged/environmental behind `#[ignore]`** with a reason string and a dedicated CI job
   (netns gateway tests). Plain `cargo test` must always pass unprivileged.

Test the failure paths by construction (bad manifest, unanswered probe, EEXIST re-apply) —
the three CI-caught idempotency bugs all lived on re-run paths nobody exercises manually.

## Crate architecture (ch. 15)

- Workspace layout: domain lib (`-core`) / daemon / thin binary; binaries contain arg parsing
  and wiring only — logic lives in libraries so tests can reach it (vxprobe lib+main split).
- `publish = false` until crates.io publication is a deliberate decision.
- Generated code (`vxedge-api`) is never hand-edited; regeneration must be deterministic.
- Pin the toolchain (`rust-toolchain.toml`), deny warnings in clippy, keep `cargo deny`
  (advisories/licenses/sources) green — CI enforces all three.

## Unsafe (ch. 12) and macros (ch. 13)

- `unsafe` is effectively forbidden: none exists in the workspace; introducing any requires a
  soundness comment and review. FFI (libvirt, DCGM) goes through vetted binding crates.
- No proc-macros of our own; derive macros (serde, thiserror, clap) are the ceiling.

## Performance (vxagents rule)

Measurement code must not perturb the measurement: no allocations on hot paths, buffers
reused, `hdrhistogram` for bounded-memory percentiles, counters as relaxed atomics. Budget
and document CPU/RSS when adding an agent.
