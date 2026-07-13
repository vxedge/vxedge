# Changelog

All notable changes to this repo are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows
[Semantic Versioning](https://semver.org/) once the first tag is cut (see
`docs/VERSIONING.md` in the `docs` repo — no `v0.1.0` yet, pre-phase-1-gate).

## [Unreleased]

### Added
- vxAPI proto v1 (`proto/vxedge/v1/*`: common, jobs, events, telemetry, lifecycle) — the
  sole cross-language control-plane contract (2026-07-09).
- 4-crate Cargo workspace: `vxedge-core` (domain lib), `vxedged` (daemon), `vxedge-cli`
  (thin client), `vxedge-api` (generated bindings) (2026-07-09).
- Job engine + event bus (broadcast channel) + JSONL audit log in `vxedged`; CLI `up`/`down`
  stream job progress (2026-07-09).
- vxgateway backend (H1 minimum): Linux bridge + VLAN filtering, tc HTB per slice,
  Apply/Up/Down/Status wired with observe+verify, netns integration test in CI (2026-07-10).
- Zone backends: `container` (netns), `qemu-microvm` (raw QEMU), `libvirt` (virsh) — all
  behind the same `ZoneService`/`vxedge zone up|down|list` interface (2026-07-11).
- `SvcService.Up/Down/Status` live for `ota` (2026-07-12): generic process backend spawns
  `vxota serve` per the manifest's `services.<key>.config` (`artifacts`, `listen`),
  pidfile-tracked like the qemu-microvm zone backend; CLI `ota up|down` wired end-to-end.
  `telemetry` returns a clear unsupported error for now (still CLI-driven directly).
- CI: build/test/clippy/fmt, `buf` lint + PR breaking-change check on the proto, Dependabot,
  `cargo-deny`, privileged netns integration job.
- `vxedge manifest validate --manifest <path>` (2026-07-13): local-only (no daemon
  connection needed — the same read-only check `vxedge up` runs before provisioning,
  exposed standalone), `--json` for CI use.
- `vxedge doctor --tier N` (2026-07-13): host preflight against docs/TESTBED-SETUP.md
  tiers 0-3 (kernel/Rust/protoc/buf/iproute2; CAP_NET_ADMIN/nftables/PREEMPT_RT;
  KVM/QEMU/libvirt/OVS/WireGuard/GPU/container-runtime), actionable fix hints per failed
  check, `--json` for CI.
- `telemetry.prometheus.optional_scrape_targets` (2026-07-13) in the manifest schema —
  marks a scrape target as legitimately absent under some experiment arms; consumed by
  `vxtelemetry` (see its own CHANGELOG), added here for schema consistency (`vxedge-core`
  is the single source of truth for the manifest shape).

### Fixed
- A service spawned by `SvcService.Up` became a zombie after `SvcService.Down`'s `kill` —
  nothing ever called `wait()` on it, so `/proc/<pid>` and `kill -0` kept reporting it as
  alive indefinitely even though it had already exited. Found by an end-to-end smoke test
  (spawn, reach over HTTP, teardown, check for zombies), not by the unit tests. Fixed with
  a dedicated reaper thread per spawn (2026-07-12).

### Known gaps (tracked in `TASKS.md`, not yet in this changelog's scope)
- nftables, WireGuard/VXLAN/FRR, pure-rtnetlink NetOps port, converging reconciler,
  vehicle-manifest structs, `vxcompute` service scheduling (v1 co-locates services with
  vxedged on the host), `telemetry` in the generic service backend.
