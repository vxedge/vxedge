# vxedge (core)

Control plane, API, and CLI of the [vxedge](https://github.com/vxedge/docs) SDV testbed —
an all-software platform for evaluating 5G network-slice mapping onto zonal & central
software-defined-vehicle E/E architectures.

Master spec: [`docs/ARCHITECTURE.md`](https://github.com/vxedge/docs/blob/main/ARCHITECTURE.md).
Contributor guidance: [`AGENTS.md`](AGENTS.md). Work items: [`TASKS.md`](TASKS.md).

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

Scaffold status: the vxAPI v1 contract, manifest model/validation, daemon transport, and CLI
command tree are in place; the gateway/zone backends and job engine are the next work items
(`TASKS.md` §2–3).
