# vxedge

The vxedge control plane: the `vxedged` daemon, the **vxAPI** gRPC/Protobuf contract, the thin `vxedge` CLI, and vxgateway, the software-defined edge gateway (bridge/OVS, VLAN, tc HTB, WireGuard, VXLAN, Open5GS/UERANSIM lifecycle). It contains no zone or compute images; those live in [`vxparts`](https://github.com/vxedge/vxparts).

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![CI](https://github.com/vxedge/vxedge/actions/workflows/ci.yml/badge.svg)](https://github.com/vxedge/vxedge/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/tag/vxedge/vxedge?label=release)](https://github.com/vxedge/vxedge/tags)

vxedge is an open, all-software testbed for mapping 5G network slices onto zonal and central software-defined-vehicle (SDV) E/E architectures on a single machine. Every published number ships with the manifest and image digests that produced it.

## Layout

| Path | What |
|---|---|
| `proto/vxedge/v1/` | **vxAPI** — the gRPC/Protobuf contract (authoritative for all SDKs) |
| `crates/vxedge-api` | Generated Rust bindings (tonic) |
| `crates/vxedge-core` | Domain library: manifest model + validation, reconciler, backends |
| `crates/vxedged` | Daemon serving vxAPI (Unix socket; mTLS TCP later) |
| `crates/vxedge-cli` | The `vxedge` binary — thin vxAPI client |

## Build & run

Requires Rust 1.96+ (pinned via `rust-toolchain.toml`) and `protoc`. New to the project?
Start with [GETTING-STARTED](https://github.com/vxedge/docs/blob/main/GETTING-STARTED.md).

```bash
cargo build --workspace
cargo test --workspace

# Terminal 1 — daemon
./target/debug/vxedged --listen unix:///tmp/vxedge.sock

# Terminal 2 — CLI
./target/debug/vxedge --endpoint unix:///tmp/vxedge.sock status
```


## Status

- **Phases 1 and 2 are complete.**
  - The gateway backend, zone backends (netns, libvirt, raw-QEMU microVM, container) and the full 5G S-NSSAI lifecycle are live end to end.
  - The `GatewayService`, `ZoneService`, `ComputeService`, `TrafficService` and `SvcService` RPCs are all live.
- **Phase 3 is in progress:** `vxedge vehicle up` end to end.

See [`docs/ROADMAP.md`](https://github.com/vxedge/docs/blob/main/ROADMAP.md) and this repo's [`TASKS.md`](TASKS.md).

## Links

- Site: <https://vxedge.dev>
- Architecture: [`docs/ARCHITECTURE.md`](https://github.com/vxedge/docs/blob/main/ARCHITECTURE.md)
- Results: [`docs/RESULTS.md`](https://github.com/vxedge/docs/blob/main/RESULTS.md) · Roadmap: [`docs/ROADMAP.md`](https://github.com/vxedge/docs/blob/main/ROADMAP.md)
- Contributing: [CONTRIBUTING](https://github.com/vxedge/.github/blob/main/CONTRIBUTING.md) · Security: [SECURITY](https://github.com/vxedge/.github/blob/main/SECURITY.md)
- License: Apache-2.0 ([`LICENSE`](LICENSE))
