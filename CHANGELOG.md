# Changelog

All notable changes to this repo are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows
[Semantic Versioning](https://semver.org/) — see `docs/VERSIONING.md` in the `docs` repo.

## [Unreleased]

### Added

- `vxedge_core::containers` (Track V1, `docs-internal/PROVISIONING.md` §7.2): container groups
  over Podman's REST API on the rootless user socket, via `bollard`. A group is what a `podman
  pod` was: an infra container owning a network namespace that members join. Membership is by
  label (`io.vxedge.group`), never by name prefix; `up` converges by a spec hash, so a second
  `up` keeps what matches and recreates only what changed. `LazyClient` connects on first use.

### Changed

- **The 5G core, RAN and UE pods are driven through the Podman API, not the `podman` CLI**
  (Track V2). Hosts need the rootless user socket (`systemctl --user enable --now
  podman.socket`, plus `loginctl enable-linger` on a host you reach over SSH) and `passt`;
  see `docs/TESTBED-SETUP.md` Tier 2. `vxedged` still starts without the socket and connects on
  first 5G use. Each pod's infra container is the Kubernetes pause image, pinned by digest.
  Container names (`vx5g-nrf`, `vx5g-ran-gnb`, ...) are unchanged.
- `fiveg` and `traffic::profile_up` are `async`; `vxedged` awaits them on its runtime instead of
  wrapping them in `spawn_blocking`.
- `deny.toml`: removed the `RUSTSEC-2024-0436` (`paste`) ignore; the crate is no longer in the dependency tree.

### Fixed

- A second `vxedge experiment up` over a running 5G core failed with `File exists` on the bridge
  attachment veth; it is now a no-op.
- `containers` groups get the `pasta` network namespace `podman pod create` gives a pod. Left
  to the API default (`bridge`), teardown failed on Ubuntu's AppArmor `pasta` profile.

### Security

- Lockfile update `pulldown-cmark` 0.13.4 -> 0.13.5, which fixes RUSTSEC-2026-0414 (exponential event growth with `ENABLE_WIKILINKS`). It is a build-time dependency via `prost-build`.

### Migration

- Pods created by an older `vxedged` carry no `io.vxedge.group` label, so this version cannot
  manage them. `vxedge experiment up` and `down` refuse to run over them and print the command
  to remove them once: `podman pod rm -f vx5g vx5g-ran $(podman pod ps -q --filter
  name=vx5g-ran-ue-)`.

## [0.2.0] - 2026-10-21

### Added

- Tag-triggered release workflow (`.github/workflows/release.yml`): builds `vxedge` and `vxedged` for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` (native Ubuntu 24.04 runners, `--locked`), publishes `vxedge-<version>-<target>.tar.gz` archives with a `SHA256SUMS`, and attaches build provenance (`gh attestation verify`). `workflow_dispatch` is a dry run that creates no release.
- Open-source release preparation: `LICENSE` (Apache-2.0), `NOTICE`, rewritten `README.md`. Crate `license` metadata is now `Apache-2.0` (was `MIT OR Apache-2.0`).

### Changed

- **BREAKING: host interface names derive from the device id, not from position.** `vz0`/`vzp0`
  become `vz-<stem>`/`vzp-<stem>`, where the stem is the sanitised device id, shortened to a
  readable head plus a hash of the whole id when it exceeds the kernel's 15-byte `IFNAMSIZ`
  limit (`node-cont-01` → `vz-node-c-32cc`). Campaign scripts address these names directly, so
  this breaks them once, deliberately — see `ARCHITECTURE.md` §6.0.5 for the contract.

  Positional names made traversal order load-bearing: adding a device renumbered the rest, and
  two views of the same topology could number it differently. exp-002 declares its arms
  `cont, microvm, libvirt` and sorts to `cont, libvirt, microvm`, so its H3 arms swapped
  interfaces between the two schema paths — the campaign would have measured libvirt and
  reported microvm, passing every test on the way. A name now depends on its own device and
  nothing else; two ids that collapse to one name are refused with both named. `Entity::
  device_order` is deleted, and the gateway and zone plans join by interface name rather than
  by index.

- **BREAKING: §6.1 experiment manifests and §6.2 vehicle manifests are deleted.** `vxedge up`,
  `vxedge experiment up`, the daemon and `vxedge manifest validate` all read §6.0 workspaces and
  nothing else. Pointing any of them at a retired manifest reports what the document is and
  where to go instead. `vxmodel render` (vxmodels) moved to workspaces first, so no CI gate was
  ever left red.

- `vxedge manifest validate` takes a §6.0 workspace — a directory, or one file of
  `---`-separated documents — and runs every cross-file check *and* every model invariant, the
  same checks `up` runs.

- `vxedge zone up|down --vxid` accepts a **device id** as well as a zone id. A zone is a region,
  and exp-002's three packaging arms share one; region-only selection could start only all
  three at once, which is the concurrent-guest CPU contention that invalidated exp-002b's first
  campaign.

- `vxedge zone list` reports each device's `port=` and `peer=`, so the device → interface
  mapping is answerable without recomputing a shortened name by hand.

### Added

- **Link endpoints take an object form**: `{ node, port, ip }` alongside the bare `node:port`
  string, pinning a host address on an attachment. exp-002's three arms share one VLAN and are
  told apart only by address, and §6.0 had nowhere to put one. Addresses are CIDR-checked at
  load time. The form is deserialized by hand rather than with `#[serde(untagged)]`, so a
  misspelled key inside the object is reported as a misspelled key.

- **A baseline part can state its `kind`** (`container` | `bundle`), validated against the
  device's backend. A part knows what it is and the device knows how it is run; deriving one
  from the other made a mismatch unrepresentable rather than rejected — the check H3 needs
  most, since it runs one function on three backends from three parts.

- **A workspace can be one file.** `---`-separated documents load exactly as a folder does, and
  `Workspace::from_yaml` gives callers with no directory — an inline manifest over vxAPI — the
  same loader. Every reference is by id, so the file boundary never carried meaning.

- `model::load_dir` / `model::load_yaml`: workspace → validated → checked model in one call, and
  the daemon's single entry point for manifest text.

### Removed

- `vxedge-core::manifest` in full — `ExperimentManifest`, `VehicleManifest` and the §6.1
  composition types they shared (~876 lines) — plus `gateway::plan::resolved_backend`, which
  existed to turn a §6.1 `image.kind` into a backend that §6.0 states outright.
- `vxedge-core::lower` (§6.0 → §6.1 lowering), `model::adapt` (§6.1 → model), and the
  `plan-diff` example. Lowering existed only so the daemon could hand `vxtelemetry` a §6.1 file;
  `vxtelemetry` reads workspaces now. `plan-diff` compared the two planning paths, and there is
  one.

### Fixed

- **A VM's guest bundle was unreachable in a §6.0 workspace.** The zone planner looked a
  device's software up by *function id equals device id* — true in §6.1, false here — so every
  libvirt and qemu-microvm arm planned as a VM with no bundle to boot. It resolves through
  `deployed_on` now, and a device hosting two bootable parts is refused rather than half-booted.

- A part error other than "no part at all" was swallowed while building the model, so a kind
  mismatch silently produced a function that ran nothing.

- `manifest validate` read a file with `serde_yaml_ng::from_str`, which fails outright on a
  `---`-separated document set — so a workspace held in one file fell through the sniffer into
  the §6.1 branch and was rejected. It reads the first document now.


### Added

- **`ExperimentService` is implemented in `vxedged`** — `vxedge experiment up` provisions the
  gateway (including the 5G core when the manifest declares `s-nssai` slices), then every zone,
  then the declared services; `down` reverses it; `status` aggregates. The RPC had been in
  `proto/vxedge/v1/lifecycle.proto` since phase 1 with nothing behind it, and campaign scripts
  did the sequencing themselves. **No proto change was needed.**

  **`up` does not start traffic, by design.** `up` is required to be idempotent and reconciling —
  running it twice is a no-op — and traffic generation is a time-bounded process rather than a
  converged state, so "start a 60-second load run" cannot be made idempotent. It stays
  `vxedge traffic up`. The split is also what a measurement campaign needs: a sweep varies
  offered load over a topology that must be identical across arms. The success message says so
  outright, so nobody waits for numbers that are not coming.

  Composition rather than reimplementation: each stage submits the *existing* service's job and
  awaits it, so nothing is duplicated and sub-jobs stay individually visible through
  `JobService` — a failure is attributable to a stage. `down` is best-effort, because a teardown
  that stops at the first error strands everything after it. The manifest is resolved once and
  handed to every stage as `inline_yaml`, so an edit made mid-run cannot leave the gateway on one
  revision and the zones on another.

- **A `ManifestRef.path` naming a directory is a §6.0 workspace.** The daemon loads, validates
  and lowers it via `vxedge_core::lower::lower_path_to_yaml`. The reproducibility checksum
  therefore covers the *lowered* manifest, which is the correct thing to hash: results are keyed
  to what actually ran, and lowering is pure, so the same workspace always yields the same hash.

### Removed

- **`vxedge experiment lower`.** Lowering is a manifest transformation the daemon owns; a verb
  for it in the CLI would have made every frontend reimplement it, and §6.0 is the only schema an
  OEM engineer authors — no frontend should need to know the older one exists.

### Changed

- **`vxedge experiment up|down|status` are addressed by the working directory**, not by
  `--manifest`. They resolve the single `.vxp` in the directory they were run from, the way
  `terraform apply` resolves the configuration in the current directory; `vxedge experiment new
  <name>` creates `./<name>/`; the global `--chdir <dir>` is the escape hatch for scripts.
  `experiment validate` and `experiment lower` lose their path argument for the same reason.

  Two behaviours are deliberate, both Terraform's. **Discovery does not walk up to parent
  directories** — `git` searches upward, Terraform does not, and running from inside `results/`
  must fail rather than silently resolve the parent experiment and re-provision it. And
  **exactly one `.vxp`** is required: the ambiguous error names the files it found, because
  which one to remove is the engineer's call and they cannot make it from a count.

  New `vxedge-core::package` owns the `.vxp` descriptor and discovery, kept separate from
  `Workspace::load` so a broken experiment document still produces a good "which workspace is
  this" error. `requires` is parsed but not yet enforced.

### Fixed

- **`watch_job` printed every progress step twice.** A watch stream delivers the current snapshot
  on subscribe and again on the first change. Dedup compares what is actually displayed, since
  `updated_unix_ms` differs on every message and would defeat a whole-value comparison.

- **`ZoneService`'s "no active experiment" error still told users to run
  `vxedge up --manifest <exp.yaml>`** — a flow that no longer exists.

- **Every error message the CLI produced was rendered with `Debug`, not `Display`.**
  `fn main() -> Result<_, _>` formats its error with `{:?}`, so a `thiserror` type printed as a
  struct dump — `NotAWorkspace { dir: "/x" }` instead of the sentence explaining what to do
  about it. Every actionable message in `vxedge-cli` and `vxedge-core` was being discarded that
  way. `main` now handles the error itself and walks the `source` chain, which additionally
  surfaces the underlying cause (an io error, a YAML parse position) that `Box<dyn Error>`'s own
  `Display` stops short of.

### Added

- `vxedge experiment lower [path] [-o <file>]` and `vxedge-core::lower` — a pure transformation
  from a §6.0 workspace folder to the §6.1 experiment manifest the daemon already runs. This is
  what closes the gap between "a workspace validates" and "a workspace runs" without putting
  orchestration in a frontend: the CLI stays a 1:1 vxAPI wrapper and the daemon is unchanged.

  Lowering hands its output to `ExperimentManifest::from_yaml`, so a lowering bug surfaces as a
  §6.1 validation error rather than a silently wrong manifest. It resolves what §6.0 keeps
  normalised and §6.1 repeats: a slice's `vlan` onto every node iface bound to that slice, and a
  traffic endpoint's component id into §6.1's `<zone-vxid>:<node-vxid>` form (the zone vxid is
  synthesized during lowering, so a workspace author cannot name it and must not have to). What
  it cannot carry is refused with the fix in the message, never guessed: a component with more
  than one part (§6.1 has one `image` per node — publish a `stack` composite), a component with
  no part, a zone with no `position`, a compute component used as a traffic endpoint.

  `channels` does not survive lowering — §6.1 has no interaction layer. That is a limitation of
  the older schema rather than of the workspace model, and it is why wiring `experiment up` to
  the workspace shape directly is the follow-up rather than this.

  Building it found six defects in the §6.0 scaffold and validator that neither review nor the
  validator caught, because none of them are shape errors — they are completeness ones. A
  gateway with no part; an invented `traffic_profiles[].slice` field that is not in §6.1;
  `memory_gb` where §6.1 says `memory`; a zone/node vxid collision; traffic endpoints and
  Prometheus scrape targets in forms nothing resolves. Four are fixed in the scaffold, two
  became new validation errors (below). Tests assert the lowered manifest survives
  `GatewayPlan::from_manifest` **and** `TrafficPlan::from_manifest`, not merely that it parses:
  §6.1's composition validator never looks at traffic endpoints, so "valid" was not enough to
  know the topology is provisionable.

- `vxedge experiment validate [path]` and `vxedge-core::workspace` — loads a workspace folder
  and runs every cross-file check, reporting all problems at once rather than the first.
  Checks: exactly one experiment; document- and component-id uniqueness across the workspace;
  vehicle/baseline references; `target_ids`, link endpoints and channel endpoints resolving to
  real components; `carried_by` naming real links **and the channel's endpoints actually being
  reachable over them**; the cross-file slice join (the vehicle names a slice, the experiment
  must define it); published refs being digest-pinned while local refs need not be; **every
  component being given software by some baseline** (a gateway that boots nothing validated
  cleanly before); and **traffic endpoints resolving** to a real component or one of the
  symbolic endpoints (§6.1 only rejects a profile when *both* sides fail to resolve, so a single
  typo would otherwise run and measure the wrong thing).

  Additive: it sits alongside the existing manifest path and changes nothing about it, so
  every current experiment keeps parsing. Loading is deliberately non-recursive so a
  `results/` archive cannot be absorbed into the experiment.

- `vxedge experiment new <id> [--template minimal|slicing]` — scaffolds an experiment
  workspace that is ready to edit, with no daemon, no network and no GPU. Generates the
  folder-workspace shape from `docs/ROADMAP.md` Track D: every file declares its `kind`, and
  everything is referenced by unique id rather than by path, so the files can be renamed
  freely. `vxedge-core::scaffold` follows the house layering — `Workspace::plan` is pure data
  and unit-tested, `Workspace::write` is the only part that touches the filesystem, and it
  refuses to write into an existing directory rather than merging into someone's folder.
  Workspace ids are validated against OCI repository-name rules at creation time, because the
  id becomes a repository path component on publish and failing there instead is much worse.

  **The generated workspace uses the proposed ARCHITECTURE.md §6 schema, which is still
  awaiting sign-off, and `experiment up` does not consume it yet.** It exists so §6 can be
  reviewed as something you can open and edit rather than as prose; the generated README says
  so plainly.

## [0.1.0] - 2026-07-13

**Phase-1 gate passed**: H1, H3, and H4 all have reproducible numbers (`vxperiments`).
Repositories were still private at this release.

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
