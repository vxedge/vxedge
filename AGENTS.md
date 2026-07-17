# AGENTS.md — vxedge/vxedge (core)

The core repo: Rust workspace hosting the control plane, API, CLI, and gateway provisioning.
Master spec: `../docs/ARCHITECTURE.md` (§5 control plane/vxAPI, §6 manifests, §7 vxgateway,
§9 virtualization). Read it before changing anything structural.

## Layout

```
proto/vxedge/v1/     # vxAPI Protobuf IDL — AUTHORITATIVE for all SDKs (buf-managed)
crates/
  vxedge-api/        # generated Rust bindings (tonic-build via build.rs) — never hand-edit
  vxedge-core/       # domain lib: manifest structs/validation, reconciler, backends
  vxedged/           # daemon: tonic gRPC server (+ tonic-web), job engine, event bus, audit
  vxedge-cli/        # thin client binary `vxedge`; may auto-spawn local vxedged
templates/           # (pending) gateway provisioning templates (netlink/OVS/tc/nftables, libvirt XML)
sdk/                 # (pending) generated SDKs (TS/connect-es, C++), never hand-edited
```

## Hard rules

- **No orchestration logic in any frontend.** The CLI is a 1:1 wrapper over vxAPI RPCs.
- **Manifests are the sole source of truth.** Mutating RPCs carry manifest documents/refs;
  daemon state lives only in ephemeral runtime dirs + logs. No database.
- **Idempotency via reconciler**: `up|down` compares desired (manifest) vs observed
  (libvirt/OVS/netlink state) and converges; running `up` twice is a no-op.
- **proto/ is the contract**: additive changes stay in `vxedge.v1`; breaking changes mean
  `vxedge.v2`. SDKs are generated in CI, never hand-written.
- Every mutating RPC returns a `Job`; progress is server-streamed (`JobService.Watch`).
- Transport defaults: Unix socket + peer creds locally; mTLS + OIDC remotely.

## Key dependencies (when scaffolding)

tonic/prost (+ tonic-web), serde/serde_yaml, rtnetlink, ovsdb client, `virt` (libvirt),
nftables via `nftables` crate or `nft -j`, clap (CLI), tokio, tracing.

## Build/test

Requires Rust 1.96+ (pinned in `rust-toolchain.toml`) + `protoc` (any recent; Rust proto
codegen runs in `crates/vxedge-api/build.rs` via tonic-prost-build, so `cargo build` is
self-sufficient given protoc). Stack: tonic 0.14 (+ tonic-prost), prost 0.14, edition 2024,
serde_yaml_ng (serde_yaml is deprecated upstream — do not reintroduce it).

- `cargo build --workspace` / `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check`
- `buf lint` for proto changes (CI runs it; `buf generate` produces the TS SDK, phase 2)
- CI: `.github/workflows/ci.yml` (build/test/clippy/fmt + buf lint)
- Gateway integration tests (future) run in network namespaces (root or userns); keep them
  behind a feature/ignored marker so plain `cargo test` works unprivileged.
- Smoke test: `vxedged --listen unix:///tmp/vxedge.sock` then
  `vxedge --endpoint unix:///tmp/vxedge.sock status`.

## Project skills

AI/contributor skills in `.claude/skills/`: **rust-patterns** (house Rust rules, per the
Microsoft Rust Patterns book), **testbed-quality** (the OEM-adoption bar), and
**gateway-verification** (netns/UDS/doctor verification loop + idempotency rule for
gateway/zone backend changes). Apply all three to every change. Plus four narrower
skills, load them when the file you're touching matches their scope: **open5gs-ueransim-
patterns** (fiveg/ config rendering + UE-netns networking gotchas), **vxapi-protobuf-
design** (proto/ RPC/message design and versioning), **network-provisioning-patterns**
(writing gateway/ netops/plan/provision code, as opposed to verifying it), and
**zone-backend-contract** (the container/qemu-microvm/libvirt attachment + guest-bundle
contract, shared with `vxzones`).
