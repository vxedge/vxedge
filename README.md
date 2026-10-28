# vxedge

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![CI](https://github.com/vxedge/vxedge/actions/workflows/ci.yml/badge.svg)](https://github.com/vxedge/vxedge/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/tag/vxedge/vxedge?label=release)](https://github.com/vxedge/vxedge/tags)

vxedge is an open, all-software testbed that maps 5G network slices onto software-defined-vehicle (SDV) E/E architectures on a single Linux machine. This repository is its control plane: the `vxedged` daemon, the **vxAPI** gRPC/Protobuf contract, the thin `vxedge` command-line client, and vxgateway, the software edge gateway (bridge or Open vSwitch, VLAN, `tc` HTB shaping, WireGuard, VXLAN, and the Open5GS and UERANSIM 5G lifecycle). Everything is declared in YAML manifests, the CLI only wraps vxAPI calls, and every published number ships with the manifest and image digests that produced it. Acronyms are defined in [`ACRONYMS.md`](https://github.com/vxedge/docs/blob/main/ACRONYMS.md).

## Install

Prebuilt `vxedge` and `vxedged` for Linux x86-64 and ARM64 are attached to each [release](https://github.com/vxedge/vxedge/releases). Each release carries `SHA256SUMS` and build provenance. The binaries are built on Ubuntu 24.04 (glibc 2.39), so they run on 24.04 and newer. Verifying provenance needs `gh` 2.49 or newer.

```bash
gh release download -R vxedge/vxedge --pattern '*'                                   # both archives + SHA256SUMS
sha256sum --ignore-missing -c SHA256SUMS                                             # must say OK
gh attestation verify vxedge-*-x86_64-unknown-linux-gnu.tar.gz -R vxedge/vxedge     # aarch64 on ARM
tar xzf vxedge-*-x86_64-unknown-linux-gnu.tar.gz
cd vxedge-*-x86_64-unknown-linux-gnu                                                 # vxedge, vxedged, LICENSE, NOTICE, README.txt
```

## Quick start

The daemon and the CLI talk over a Unix socket. This smoke test needs no privileges. Run it from the extracted directory (prefix `./`), or from `target/debug/` after a [source build](#build-from-source):

```bash
./vxedged --listen unix:///tmp/vxedge.sock &
./vxedge --endpoint unix:///tmp/vxedge.sock status
# gateway vxgateway: state=1 detail=no gateway provisioned
./vxedge --help          # full command tree
```

Without root, `vxedged` logs a warning that `/run/vxedge` is missing and keeps going; add `--runtime-dir /tmp/vxrt` to silence it.

Real provisioning (`vehicle up`, `zone up`) needs `CAP_NET_ADMIN`: run `vxedged` under sudo, or `sudo setcap cap_net_admin+ep vxedged`. Without it, `up` validates the manifest and then fails with an actionable message, by design. A vehicle with virtual-machine zones also needs a guest bundle built once on the host. The steps are in [GETTING-STARTED](https://github.com/vxedge/docs/blob/main/GETTING-STARTED.md), and per-verb usage is in the [USER-GUIDE](https://github.com/vxedge/docs/blob/main/USER-GUIDE.md).

## Build from source

Requires Rust 1.96+ (pinned by `rust-toolchain.toml`; rustup fetches it on the first `cargo` command) and `protoc`.

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Proto changes are also checked with `buf lint` in CI. Contributor rules are in [`AGENTS.md`](AGENTS.md).

## Layout

| Path | What |
|---|---|
| `proto/vxedge/v1/` | **vxAPI**, the gRPC/Protobuf contract (authoritative for all SDKs) |
| `crates/vxedge-api` | Generated Rust bindings (tonic) |
| `crates/vxedge-core` | Domain library: manifest model and validation, reconciler, backends |
| `crates/vxedged` | Daemon serving vxAPI over a Unix socket |
| `crates/vxedge-cli` | The `vxedge` binary, a thin vxAPI client |
| `containers/fiveg/` | Open5GS and UERANSIM container builds |

## Status

Stage: "pre-release" and "Phases 1–2 complete, phase 3 in progress" (`project.stage`, `project.status` in [`FACTS.md`](https://github.com/vxedge/docs/blob/main/FACTS.md)). Before 1.0 the manifest schema and vxAPI may change between minor versions ([`VERSIONING.md`](https://github.com/vxedge/docs/blob/main/VERSIONING.md)); published results are frozen.

Fidelity is not uniform. The [parity ledger](https://github.com/vxedge/docs/blob/main/FACTS.md#parity-ledger) marks each part of the virtual vehicle as real, emulated, planned or caveat. For example, the 5G radio is "UERANSIM gNB and UE in software" (emulated), and timing is "Userspace timestamps on a shared host" (caveat). Read the ledger before drawing conclusions from a number.

Phase plan and open work: [`ROADMAP.md`](https://github.com/vxedge/docs/blob/main/ROADMAP.md) and this repository's [`TASKS.md`](TASKS.md). Measured results and their caveats: [`RESULTS.md`](https://github.com/vxedge/docs/blob/main/RESULTS.md). Every published number and state is registered in [`FACTS.md`](https://github.com/vxedge/docs/blob/main/FACTS.md).

## Links

- Docs: [vxedge/docs](https://github.com/vxedge/docs) ([GETTING-STARTED](https://github.com/vxedge/docs/blob/main/GETTING-STARTED.md), [ARCHITECTURE](https://github.com/vxedge/docs/blob/main/ARCHITECTURE.md))
- Measurement agents: [vxedge/vxagents](https://github.com/vxedge/vxagents)
- Telemetry service: [vxedge/vxtelemetry](https://github.com/vxedge/vxtelemetry)
- OTA service: [vxedge/vxota](https://github.com/vxedge/vxota)
- Experiments, manifests and results: [vxedge/vxperiments](https://github.com/vxedge/vxperiments)
- Website: <https://vxedge.dev>
- Contributing: [CONTRIBUTING](https://github.com/vxedge/.github/blob/main/CONTRIBUTING.md) · Security: [SECURITY](https://github.com/vxedge/.github/blob/main/SECURITY.md)
- License: Apache-2.0 ([`LICENSE`](LICENSE))
