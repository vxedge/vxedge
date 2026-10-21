# vxedge

The control plane of vxedge, an open, all-software testbed that maps 5G network slices onto software-defined-vehicle (SDV) E/E architectures on a single machine.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![CI](https://github.com/vxedge/vxedge/actions/workflows/ci.yml/badge.svg)](https://github.com/vxedge/vxedge/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/tag/vxedge/vxedge?label=release)](https://github.com/vxedge/vxedge/tags)

## What it is

This repo holds the `vxedged` daemon, the **vxAPI** gRPC/Protobuf contract, the thin `vxedge` command-line client, and vxgateway, the software-defined edge gateway (bridge or Open vSwitch, VLAN, `tc` HTB shaping, WireGuard, VXLAN, and the Open5GS and UERANSIM 5G lifecycle). Everything the platform does is declared in YAML manifests; the CLI only wraps vxAPI calls. Every published number ships with the manifest and image digests that produced it.

Zone and compute images are not here; they live in `vxparts`. Acronyms are defined in [`ACRONYMS.md`](https://github.com/vxedge/docs/blob/main/ACRONYMS.md).

## Install

Prebuilt `vxedge` and `vxedged` for Linux x86-64 and ARM64 are attached to each [release](https://github.com/vxedge/vxedge/releases). Each release carries `SHA256SUMS` and build provenance. You need `gh` 2.49 or newer, and Ubuntu 24.04 or newer (the binaries are built on 24.04, glibc 2.39).

```bash
gh release download -R vxedge/vxedge --pattern '*x86_64*' --pattern SHA256SUMS      # aarch64 on ARM
sha256sum --ignore-missing -c SHA256SUMS                                             # must say OK
gh attestation verify vxedge-*-x86_64-unknown-linux-gnu.tar.gz -R vxedge/vxedge     # needs gh 2.49 or newer
tar xzf vxedge-*-x86_64-unknown-linux-gnu.tar.gz                                     # vxedge, vxedged, LICENSE, NOTICE
```

## Quick start

Daemon and CLI talk over a Unix socket. This smoke test needs no privileges:

```bash
./target/debug/vxedged --listen unix:///tmp/vxedge.sock --runtime-dir /tmp/vxrt &
./target/debug/vxedge --endpoint unix:///tmp/vxedge.sock status
# gateway vxgateway: state=1 detail=no gateway provisioned
./target/debug/vxedge --help          # full command tree
```

The paths are those of a [source build](#build-from-source); with the prebuilt archive, run the extracted `vxedged` and `vxedge` instead.

To bring up a bare vehicle (a `kind: vehicle` manifest plus a `kind: baseline`, no experiment file), run `vehicle up`, `status` and `down` against its workspace directory:

```bash
./target/debug/vxedge --endpoint unix:///tmp/vxedge.sock vehicle up     --manifest <vehicle workspace>
./target/debug/vxedge --endpoint unix:///tmp/vxedge.sock vehicle status --manifest <vehicle workspace>
./target/debug/vxedge --endpoint unix:///tmp/vxedge.sock vehicle down   --manifest <vehicle workspace>
```

Real provisioning (`vehicle up`, `zone up`) needs `CAP_NET_ADMIN`: run `vxedged` under sudo, or `sudo setcap cap_net_admin+ep target/debug/vxedged`. Without it, `up` validates the manifest and then fails with an actionable message, by design. A vehicle with virtual-machine zones also needs a guest bundle built once on your host; the steps are in [GETTING-STARTED](https://github.com/vxedge/docs/blob/main/GETTING-STARTED.md) and the per-verb state is in the [USER-GUIDE](https://github.com/vxedge/docs/blob/main/USER-GUIDE.md).

## Build from source

Requires Rust 1.96+ (pinned by `rust-toolchain.toml`) and `protoc`. New to the project? Start with [GETTING-STARTED](https://github.com/vxedge/docs/blob/main/GETTING-STARTED.md).

```bash
cargo build --workspace
cargo test --workspace
```

## Layout

| Path | What |
|---|---|
| `proto/vxedge/v1/` | **vxAPI**, the gRPC/Protobuf contract (authoritative for all SDKs) |
| `crates/vxedge-api` | Generated Rust bindings (tonic) |
| `crates/vxedge-core` | Domain library: manifest model and validation, reconciler, backends |
| `crates/vxedged` | Daemon serving vxAPI (Unix socket; mTLS TCP later) |
| `crates/vxedge-cli` | The `vxedge` binary, a thin vxAPI client |

## Status

Pre-release (0.x): the manifest schema and vxAPI may change between minor versions. Phase status and open work are in [`docs/ROADMAP.md`](https://github.com/vxedge/docs/blob/main/ROADMAP.md) and this repo's [`TASKS.md`](TASKS.md); every published state and number is registered in [`docs/FACTS.md`](https://github.com/vxedge/docs/blob/main/FACTS.md).

## Links

- Site: <https://vxedge.dev>
- Architecture: [`docs/ARCHITECTURE.md`](https://github.com/vxedge/docs/blob/main/ARCHITECTURE.md)
- Results: [`docs/RESULTS.md`](https://github.com/vxedge/docs/blob/main/RESULTS.md) · Roadmap: [`docs/ROADMAP.md`](https://github.com/vxedge/docs/blob/main/ROADMAP.md)
- Contributing: [CONTRIBUTING](https://github.com/vxedge/.github/blob/main/CONTRIBUTING.md) · Security: [SECURITY](https://github.com/vxedge/.github/blob/main/SECURITY.md)
- License: Apache-2.0 ([`LICENSE`](LICENSE))
