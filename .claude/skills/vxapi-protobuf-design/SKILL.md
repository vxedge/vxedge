---
name: vxapi-protobuf-design
description: Design and evolution rules for vxAPI (proto/vxedge/v1/, gRPC/Protobuf) — use when adding or changing any RPC, message, or service, or wiring a new frontend SDK. The API-first spine every frontend (CLI, Qt/QML, web, Electron) depends on identically.
---

# vxAPI design — the API-first spine

Reference: `ARCHITECTURE.md` §5 (control plane & API). Proto IDL lives in
`vxedge/proto/vxedge/v1/`; `crates/vxedge-api` is a pure re-export of what
`tonic-build` generates from it (`lib.rs`: "this crate only re-exports what
tonic-build generates... the Protobuf IDL is the authoritative contract").

## Every mutating verb returns a Job — no exceptions

- Mutating RPCs (`Apply`/`Up`/`Down`) return `Job` (`jobs.proto`); progress is observed
  via `JobService.Watch` (server streaming), never a bespoke per-service progress type.
  `Job.kind` is a dotted string (`"gateway.up"`, `"zone.down"`, `"telemetry.collect"`) —
  reuse that convention exactly for any new verb, don't invent a new shape.
- Read-only RPCs (`Status`, `List`) return their resource directly, not a `Job` —
  `GatewayStatus`/`ZoneStatus`-shaped messages, synchronous.
- `Apply` means validate + stage without starting (plan/diff semantics) — distinct from
  `Up`. Don't collapse them even when a new service's staging step is trivial; the
  distinction is part of the contract every frontend already codes against.

## Naming — deliberately not buf's stock convention

`buf.yaml`'s lint `except` list is load-bearing, not a lint-fatigue shortcut — read the
comments before "fixing" a lint warning it silences:

- `RPC_REQUEST_STANDARD_NAME` is off on purpose: request messages follow
  `<Resource><Verb>Request` (`GatewayUpRequest`), not buf's default
  `<Service><Verb>Request`. Match the existing pattern for any new service.
- `RPC_REQUEST_RESPONSE_UNIQUE` / `RPC_RESPONSE_STANDARD_NAME` are off because
  `Up`/`Down`/`Apply` intentionally share the `Job` response type across every service —
  a per-RPC wrapper message here is noise, not safety.
- `breaking: use: [FILE]` is on — buf's breaking-check runs in CI on every PR. A field
  renumber or removal on an already-shipped message fails the PR; don't work around it
  by hand-editing generated code, fix the proto.

## Versioning

- Additive changes (new RPC, new optional field, new service) stay in `vxedge.v1`.
  Anything that breaks an existing client — field removal/renumber, changed semantics
  of an existing RPC — means a new `vxedge.v2` package, not a v1 mutation. This is what
  makes buf's `FILE`-level breaking-check meaningful instead of a check you route around.

## SDKs are generated, never hand-written — in any of the three languages

- Rust (`vxedge-api`, tonic), C++ (Qt GRPC), TypeScript (connect-es) are all generated
  from the same proto — that's the entire point of a single IDL (§5.2's rationale over
  SOME/IP/bare REST). If a frontend needs a convenience wrapper, put it in that
  frontend's own thin-client code, never by hand-editing generated bindings or adding
  fields to the generated types.
- No second source of truth beside manifests: mutating RPCs carry manifest documents or
  references to them (`ManifestRef`) — don't add fields to carry state the manifest
  already owns, and don't have `vxedged` persist anything the manifest can't reconstruct
  (ephemeral runtime dirs + logs only, per §5.3).

## Before opening a PR that touches `proto/`

1. `buf lint` and `buf breaking --against '.git#branch=main'` locally — CI runs both,
   catching it here is faster.
2. Regenerate all three SDKs' bindings; a proto change with stale generated Rust/C++/TS
   is a broken build for every frontend, not just the one you're working in.
3. If the change is additive, confirm it really is — buf's breaking-check is the
   objective check but the `Job`/`Status` split and message-naming rules above aren't
   things it enforces, only a reviewer (or this skill) catches those.
