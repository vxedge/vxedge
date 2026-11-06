# Changelog

All notable changes to this repo are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows
[Semantic Versioning](https://semver.org/) — see `docs/VERSIONING.md` in the `docs` repo.

## [Unreleased]

### Added

- `up` runs what the lock says. `experiment up`, `vehicle up`, `gateway up` and `Plan` refuse a
  `vxp/1` package that does not match its `vxp.lock` under the new
  `ERROR_REASON_LOCK_MISMATCH`, naming each file added, removed or changed and each reference that
  disagrees; a missing lock is a mismatch. `--unlocked` (`unlocked` on `GatewayUpRequest`,
  `ExperimentUpRequest`, `VehicleUpRequest` and `WorkspacePlanRequest`) skips the check while
  authoring. Locked or not, a `vxp/1` package that `pkg pack` refuses is refused by `up` too.
  Legacy descriptors and inline manifests are not checked.
- A run records its package and its kernel (`ARCHITECTURE.md` §13). The run record and the `Up`
  audit records gain `package_id`, `package_version`, `package_digest` and `package_locked` for a
  `vxp/1` package, and `kernel_release` and `kernel_preempt_rt` for every run, so a
  `kernel.preempt_rt: preferred` result says which kernel it came from. `Run` gains `package`
  (`PackageRef`: id, version, digest), `package_locked` and `kernel`. `TelemetryService.Collect`
  tells vxtelemetry whether the live run was locked, in `VXTELEMETRY_PACKAGE_LOCKED`: the next
  vxtelemetry records it in `metadata.json`, and 0.3.0 ignores it.

### Changed

- `wasmtime` and `wasmtime-wasi` 49.0.5 -> 50.0.1 (still pinned exactly), the runtime behind `vxcapsule`. No source change was needed. The same reflector capsule answers 199 of 200 probes with the same latency on both, and the startup line and error texts are identical.
- `rtnetlink` 0.23 -> 0.24 (with `netlink-packet-route` 0.34 and `netlink-proto` 0.14), the netlink client behind bridge, veth, address and route setup. No source change was needed; the privileged `netns-integration` job covers it.
- Lockfile update: the semver-compatible dependency updates `cargo update` offers (49 crates). No `Cargo.toml` change and no source change; build, test, clippy `-D warnings`, fmt and `cargo deny` pass. `wasmtime` and `wasmtime-wasi` stay pinned at `=49.0.5`.

## [0.6.1] - 2026-11-05

The toolchain moves to Rust 1.99.1, and `vxedge doctor` now compares versions against the pin instead
of matching the text `1.96`, so it keeps working as the pin moves. A `vxp/1` package can now be
created, locked, packed, unpacked and inspected offline with `vxedge pkg`, and the preflight enforces
what a package `requires`: KVM, a PREEMPT_RT kernel and each listed backend. A `libvirt` zone node now
runs with the host CPU, as a `qemu-microvm` node already did. vxAPI stays `vxedge.v1`, additive only.

### Added

- The preflight that `Plan`, `experiment up` and `vehicle up` run now enforces the rest of a
  `vxp/1` package's `requires`, under `ERROR_REASON_HOST_REQUIREMENT_UNMET` as `gpu` already was:
  `kvm: true` needs `/dev/kvm`, `kernel.preempt_rt: required` needs a PREEMPT_RT kernel, and each
  listed backend must be available here. `Plan` names every unmet requirement, not only the first.
  `kernel.preempt_rt: preferred` runs on any kernel; the run record does not yet note which kernel
  it was.
- `vxedge-core::pkg`, the package lock, packed form and package digest of `ARCHITECTURE.md`
  §6.0.7: a deterministic ustar writer and a reader that accepts only what it writes, `unpack`
  checked by re-packing, and `vxp.lock` built offline from the documents (published parts and
  images as the documents pin them). The packed-bytes test vector is frozen, and Python's
  `tarfile` writes the same bytes.
- `vxedge pkg new | validate | lock | pack | unpack | inspect`, local and offline. `pkg validate`
  adds the package checks to `experiment validate`'s and names each file the lock does not
  match; `pkg lock --check` reports without writing; `pkg pack` refuses a stale lock and a target
  inside the package, writes `<name>-<version>.vxp` beside it and prints the digest; `pkg inspect`
  prints descriptor, lock and digest from either form.

### Changed

- The toolchain moves from Rust 1.96.1 to 1.99.1 (`rust-toolchain.toml`, `rust-version` 1.99).
  `vxedge doctor` now compares versions instead of matching the text `1.96`: it requires a rustc
  at or above the pin in `rust-toolchain.toml` (read at build time, so the check follows the pin)
  and enforces the protoc 3.21 and buf 1.71 floors it already named, so a protoc older than 3.21
  now fails the check. `vxedge-api` allows two clippy lints that 1.99 raises in tonic and pbjson
  generated code.
- `Plan`'s per-backend host checks are named with the manifest's backend names
  (`backend qemu-microvm`, not `backend qemu_microvm`).
- `experiment new` scaffolds a `vxp/1` package: `vxp.yaml` with id `local/<id>` (or
  `--namespace <org>`) and its first `vxp.lock`, instead of a legacy `<id>.vxp`. Legacy
  workspaces still run; the `pkg` verbs tell them how to migrate.

### Fixed

- A `libvirt` zone node now gets the host CPU (`<cpu mode='host-passthrough'/>`), as a
  `qemu-microvm` node already does with `-cpu host`. With no CPU named, libvirt defaulted to
  `qemu64`, and a guest that needs the host's CPU features printed nothing.

## [0.6.0] - 2026-11-05

An experiment can now be described by a `vxp/1` package descriptor, `vxp.yaml`, with a namespaced
id, a SemVer version and the host it requires. A daemon refuses a descriptor schema it does not read
and a package whose `requires.vxedge` range excludes it, each with its own error reason. Legacy
`<name>.vxp` descriptors still load. qemu-microvm guests now each get their own MAC and the
virtio-mmio transport Zephyr needs, so several qemu guests on one slice all answer unicast; earlier
multi-guest qemu measurements may have been affected. vxAPI stays `vxedge.v1`, additive only.

### Added

- `vxp/1` package descriptors (`ARCHITECTURE.md` §6.0.3): `vxp.yaml` with `schema: vxp/1`, a
  `namespace/name` id whose name is the experiment id, a SemVer `version`, a required `entry`, and
  `title`, `license`, `authors`, `requires` and `provenance`. Unknown keys are refused, and `x-`
  keys are carried and ignored. The document glob skips `vxp.yaml`, in the CLI and the daemon alike.
- `ERROR_REASON_PACKAGE_SCHEMA_UNSUPPORTED`: `Plan`, `experiment up` and `vehicle up` refuse a
  descriptor whose `schema` this build does not read, naming the ones it does.
  `HostCapabilities.package_schemas` lists them.
- `ERROR_REASON_VXEDGE_VERSION_UNSUPPORTED`: a package whose `requires.vxedge` range excludes
  the daemon is refused before anything is provisioned.
- Validation refuses a `.vxpignore` that excludes the descriptor, `vxp.lock` or a document, which
  would leave the workspace identity blind to what the experiment is made of.

### Changed

- Legacy `<name>.vxp` descriptors load as before, and are refused beside a `vxp.yaml`. A `*.vxp`
  holding a tar archive is reported as a packed package rather than parsed, and a legacy file that
  declares a `schema` is told to become `vxp.yaml`.
- `requires.kvm`, `requires.kernel.preempt_rt` and `requires.backends` are read and checked for
  shape, but not yet enforced by the preflight; `requires.vxedge` and `requires.gpu` are.
- Backend names come from one table (`Backend::as_str`), which the manifest reader and the
  descriptor both use.

### Fixed

- qemu-microvm guests get a non-legacy virtio-mmio transport. QEMU's default is the legacy
  (version 1) one, which Zephyr's driver refuses, so a Zephyr guest found no network device. Linux
  guests speak both versions and are unchanged.
- qemu-microvm guests get a stable, unique MAC. QEMU gave every guest `52:54:00:12:34:56`, so on a
  vehicle with several qemu guests on one slice the bridge delivered unicast to one of them and the
  others answered nothing, while ARP still worked. The MAC comes from the node id, as it already did
  under Cloud Hypervisor, and both backends now share one function.

## [0.5.3] - 2026-11-04

A workspace's identity no longer changes when it is its own git repository or an editor leaves a
swap file, and every recorded identity names the rule that produced it. Identity values differ from
0.5.2's by design: a run recorded by an older daemon reads as `vxws/1` and is still restored after an
upgrade. The `.vxp` descriptor is now checked against the experiment it names. vxAPI stays
`vxedge.v1`, additive only (`Run.workspace_identity_algorithm`). Release together with vxtelemetry
0.3.0, which computes the same identities and archives every file they cover.

### Changed

- A workspace's `.vxp` descriptor is now checked against its documents. Its `id` must equal the
  experiment's id, and its `entry`, when given, must name the file holding the experiment.
  `experiment validate`, `manifest validate`, `WorkspaceService.Plan` and `up` all run the check,
  and the daemon refuses with `ERROR_REASON_WORKSPACE_INVALID` before provisioning anything.
- `Workspace::load` reads the descriptor, so a malformed `.vxp`, or a second one, fails
  validation instead of failing later at `experiment up`.
- The workspace identity is now computed under `vxws/2` (`ARCHITECTURE.md` §13). It skips `.git`
  at any depth, editor swap and backup files, and whatever a root `.vxpignore` names in
  `.gitignore` syntax, so a workspace that is its own repository, or that an editor has touched,
  keeps its identity. `results/` and `.git` cannot be re-included, and `.vxpignore` is itself
  hashed. Values differ from 0.5.2's by design.
- Every recorded identity names its algorithm: `Run.workspace_identity_algorithm` (new, additive),
  `identity_algorithm` in the run record, `workspace_identity_algorithm` in audit lines. A run
  recorded by an older daemon reads as `vxws/1`, is verified under `vxws/1`, and is still restored
  after an upgrade rather than refused as changed.
- `Workspace::content_sha256` is replaced by `Workspace::identity` and
  `Workspace::identity_with`, which return the value with its algorithm.

## [0.5.2] - 2026-11-04

Closes the vxAPI upgrade plan. `HostService.Capabilities` now answers from the checks `up` itself
makes, so it cannot tell an agent a backend is available when `up` would refuse it, and it reports
the GPU. `traffic up` refuses before starting when an endpoint is not on the host, and status reads are
four times faster. Verified live through `vxmcp`.

### Added

- `HostCapabilities.gpu`: the best NVIDIA GPU the driver reports, the probe a workspace's
  `requires` is checked against.
- `ERROR_REASON_ENDPOINT_NOT_ON_HOST`: `TrafficService.Up` refuses a profile whose node-side endpoint
  is inside a node, because its zone is up, and names `--stage gateway`. It used to start a job that
  failed with a shell error.

### Fixed

- `HostService.Capabilities` and `WorkspaceService.Plan`'s backend checks report the container
  backend only where Podman answers, as the preflight requires, and probe the emulator the
  qemu-microvm backend starts. They checked only that `ip` and a fixed binary name were on `PATH`.
- Status is observed concurrently: each node in its own probe, and `vehicle status` reads the
  gateway, nodes, services and signal plane at once. On `vx-vehicle-03`, `vehicle status` and
  `RunService.List` went from 6.0 s to 1.6 s and `zone list` from 2.4 s to 1.3 s. `RunService` reads
  one workspace status per call, and answers `STAGING` without observing while a lifecycle job runs.

## [0.5.1] - 2026-11-04

A run's state is now observed, not assumed. After a daemon stopped in the middle of `up`,
`RunService` reported the half-built run as `RESOURCE_STATE_UP`, so an agent trusting the typed field
would have taken it as ready. Verified live on `vx-vehicle-03` with the daemon stopped during the
zone, compute and signal-plane stages.

### Fixed

- `Run.state` rolls up the workspace's status: `UP` only when every zone, compute node, service and
  signal plane it declares is up, `DEGRADED` for a half-built run until `up` or `down` converges it,
  and `STAGING` while a lifecycle job holds the host. It was the gateway's state alone.

## [0.5.0] - 2026-11-04

vxAPI for agents. An agent can plan a workspace, bring it up, follow its jobs, read typed status,
run traffic, collect the evidence and recover from a restart, branching only on typed fields: enum
values, `ErrorReason` on every refusal, and run and job ids that never name the wrong thing. The
contract stays `vxedge.v1`, additive only; `buf breaking` passes against `v0.3.0`, and `vxmcp`
0.1.0 still works against this daemon. The release gate ran live through `vxmcp`: ten consecutive
`vx-vehicle-03` runs with a daemon restart each, cancel during `up`, libvirt on `exp-002-ws`, and
traffic and collection on `exp-001-ws`, each leaving the host clean
([evidence](https://github.com/vxedge/vxperiments/tree/main/results/vxapi-0.5.0-gate-2026-11-04)).

### Added

- `vxedge-api` gains a `serde` feature: every message and enum serializes in the canonical
  Protobuf JSON mapping, with enum values by name and fields at their default value emitted, not
  omitted. `vxmcp` returns its tool results this way.
- `TrafficService.Status`: each profile as running, finished or failed, from the exit code its
  process left, so an agent knows when a load run is over.
- `TelemetryService.Collect`: runs `vxtelemetry collect` as a job and returns the archive, with its
  sha256, as `Job.artifact`. It records the digest of every image the workspace runs.
  `vxedge telemetry up`, `down` and `collect` stop being scaffolds. `StreamMetrics` is not built.
- Event resume: `Subscribe { after_id }` replays from a bounded history of 1,024 events, and a
  cursor that history no longer holds is refused as `ERROR_REASON_EVENTS_LOST`. A subscriber that
  falls behind gets the same reason and its stream ends, instead of a silent gap.
- `run.up` and `run.down` events, whose resource is the run, and `run_id` on every event.
- `Subscribe.kinds` accepts a family: `zone` matches `zone.up` and `zone.down`.
- `WorkspaceService.Plan`: what `up` would do and whether it would be refused, with typed problems,
  planned nodes and services, every image, and host checks, without touching the host. It runs the
  same checks as `vehicle up` and `experiment up`, and a test fails if the two disagree.
- `HostService.Capabilities`: which backends this host can run, the part kinds, and how many runs it
  holds at once.
- Structured errors: every refusal carries `google.rpc.ErrorInfo` with an `ErrorReason` and the
  domain `vxedge.v1`, and a failed job carries `Job.error_reason`, including from its stages. A test
  fails if any refusal bypasses the reasons. New reasons: `INVALID_REQUEST`, `NOT_FOUND`,
  `BACKEND_UNAVAILABLE`, and `NODE_PORT_GONE` for `zone up` after `zone down` on a container node.
- `JobService.List`, filtered by state, kind, run and parent, and `JobService.Cancel`, which stops a
  job and its stages at their next step. A `down` cannot be cancelled.
- Composite jobs link their stages through `Job.parent_job_id`, and every job carries its `run_id`.
- `request_id` on `TrafficService.Up` and `NosCheckService.Verify` returns the job a retry started.
- Runs: `RunService` reports the provisioned workspace on this host, and verbs accept a `RunRef`,
  which is refused if it is not the active run. A run's id lasts from the first `up` to the last
  `down`, across repeated `up` and daemon restarts.
- Typed node status (`NodeStatus`) on zone and compute lists, `vehicle status` and `experiment
  status`: backend, zone, role, port, peer, VLAN and payload with digest. `experiment status` also
  carries the gateway.
- `RESOURCE_STATE_UNPROVISIONED`, for a declared device nothing provisions.
- `vehicle up` and `down` honour `stages`, with `--stage` on the CLI. `--stage compute` is new on both
  `vehicle` and `experiment`.
- A `kind: vehicle` document can declare `signal_plane.kuksa`, the Kuksa databroker placed on a container device with `on:`, under the same rules as a placed service. Its placement is checked when the workspace loads. `vehicle up` and `experiment up` start it in the device's container group, after compute and before services, `experiment up --stage signal-plane` starts it alone, and `status` and events report it as `SIGNAL_PLANE`. A databroker that exits right after it starts fails the job. It runs with `--insecure`, so slice isolation is its only protection.

### Changed

- Event ids carry a per-daemon prefix, so an id from before a restart cannot match a new event.
- `experiment up` runs the checks `vehicle up` does before touching the host: local images, guest
  bundles, capsule modules, another vehicle on the host, and placed services. Both also refuse a
  runtime dir too long for a Cloud Hypervisor API socket before any node starts.
- One lifecycle operation runs at a time: a second call for the same thing returns the running job,
  and any other is refused as `ERROR_REASON_BUSY` (`ABORTED`).
- Job ids carry a per-daemon prefix, and job history is bounded, so an old id is reported unknown
  rather than naming another job.
- A service that exits straight after starting fails `svc up`, as the signal plane already did.
- `ZoneService` no longer lists or addresses hpc devices, and `experiment up` brings them up through
  `ComputeService`, in a compute stage, so their audit lines and events say what they are.
  `vxedge zone up` on an hpc device now answers not found, naming `vxedge compute`.
- `experiment up` refuses a workspace that is not the active one, as the `vehicle` verbs do.
- `since_unix_ms` is 0 unless the transition time is known. `observed_unix_ms` says when a status was
  read.
- A stage the daemon does not know is refused rather than ignored.
- `compute list` shows the zone, like `zone list`.
- `experiment status` prints state names, and takes `--json`.

### Deprecated

- `Apply`, `Remove` and `PushImage` on `ZoneService` and `ComputeService`, which were never built.
  `TelemetryService.StreamMetrics` is marked not built in the contract.

### Fixed

- A staged `vehicle` or `experiment` verb is an exact duplicate only with the same stages. `up --stage gateway`
  while a full `up` runs is refused as `BUSY` instead of being handed the full job. Found live.

## [0.4.1] - 2026-11-02

A vehicle now outlives its daemon. Zone and compute verbs keep working after a daemon restart, a placed
service reads correctly however it is asked for, and `experiment status` answers only for what is
active. A live run on `vx-vehicle-02` covered a `kill -9` of the daemon: a new daemon read the real
vehicle, all 8 flow checks passed, and `vehicle down` left nothing behind.

### Added

- `vxedge ota status`, the CLI verb for `SvcService.Status`.
- `vxedge vehicle status --json`.
- `crates/vxedged/tests/rpc_contract.rs`: tests that run a real `vxedged` over a Unix socket through
  the generated clients, unprivileged, as part of plain `cargo test`.

### Changed

- `ExperimentService.Status` refuses a workspace that is not the active one, as the `vehicle` verbs
  do, and reports the declared services. An experiment brought up without its services stage now
  reads as degraded.
- `service.up` and `service.down` events use the id `SvcService.Status` reports (`svc-ota`, not `ota`).

### Fixed

- `SvcService` read a bare vehicle workspace as invalid, so `Up` failed and `Status` said a placed
  service was not running whatever it was doing.
- `SvcService.Status` with no workspace always read a placed service as down. It now uses the active
  workspace.
- Zone and compute verbs failed with "no active experiment" after a daemon restart, while the vehicle
  was still up. The daemon now rebuilds its state from the active record, and refuses a workspace
  edited since `up`.
- `ZoneService.List` reported hpc devices as zones. They now report as compute.

### Known limits

- `zone up` on a container node after `zone down` fails, because the gateway port is destroyed with
  the namespace. The error says so, and `vehicle up` recovers it.
- A workspace with no directory, such as an inline manifest, cannot be restored after a restart.
- The limits in 0.4.0 stand.

## [0.4.0] - 2026-11-02

A fifth zone backend, and every backend proven in one vehicle. `backend: wasm-capsule` runs a
WebAssembly component as a sensor or ECU node, for parts simulable as a sandboxed control loop rather
than a full guest kernel. `vx-vehicle-03` brings up six zones on qemu-microvm, cloud-hypervisor,
wasm-capsule and container from one workspace; the run is in
[`vxperiments/results/all-backends-2026-11-02`](https://github.com/vxedge/vxperiments/tree/main/results/all-backends-2026-11-02).
Its guests are probes, so it shows the backends provisioning and isolating together, and it is not
a measurement. The phase-3 gate on `vx-vehicle-02` was repeated on this code and still holds.

### Added

- `backend: wasm-capsule`: a zone node can run a WASI 0.2 command component, a `kind: wasm` part that
  is a local file or published by digest. Its network is a container node's: a named namespace with
  the gateway port as `eth0`. The new `vxcapsule` binary runs it inside that namespace, unprivileged
  and with no filesystem access, and ships beside `vxedged`. It runs as a separate process, so a stop
  reaches an idle capsule and a capsule outlives a `vxedged` restart.
- `vxedge doctor --tier 2` reports Cloud Hypervisor, as an optional check.

### Fixed

- A Cloud Hypervisor guest is seen from a daemon with another runtime dir, through its VMM's API
  socket. It used to read as down there, so `up` could start a second VMM and `vehicle down` could
  remove the gateway from under it. The host check for a vehicle this workspace does not declare
  names these guests too. Both daemons must run as the same user.
- Under `setcap`, `vxedged` could not create a node's named namespace, because `/run/netns` is root's.
  Creating and removing one now uses the `ip` sudo entry when `vxedged` is unprivileged.
- `vxedge experiment new` named a part that was never published, carrying another part's digest, so a
  generated workspace could not pull it. It now uses the local `vxprobe`, as the shipped experiments
  do.

### Security

- Lockfile update `pulldown-cmark` 0.13.5 -> 0.13.6, which fixes RUSTSEC-2026-0433 (quadratic parsing
  with `ENABLE_WIKILINKS`). It is a build-time dependency via `prost-build`.

### Migration

- A `wasm-capsule` node needs `vxcapsule` beside `vxedged`, on `PATH`, or named by
  `VXEDGE_CAPSULE_BIN`. The release archive carries it. Nothing else changes for existing workspaces.

### Known limits

- `wasm-capsule` is validated on the `reflector-capsule` part only, and that published package is
  private for now, so a public run builds it from `vxparts`.
- Cloud Hypervisor CPU pinning is untested, and why a guest needs a stable MAC is still not
  understood.
- `vxedged`'s runtime dir must be short: a Cloud Hypervisor node's API socket lives there, and a Unix
  socket path has a length limit. `up` fails naming the path, after earlier nodes have started, and
  `down` then cleans up.
- `experiment up` has no wrong-workspace check, and `experiment` verbs do not read the host.
- The limits in 0.3.0 on device attachments, `role: gateway`, `resources` and placed services stand.

## [0.3.0] - 2026-11-02

`vxedge vehicle up` now brings up a full vehicle from one workspace: a gateway, zones on several
backends, central compute and a service placed on a slice. This is the phase-3 gate, and it passed
on `vx-vehicle-02`; the run is in
[`vxperiments/results/phase-3-gate-2026-11-01`](https://github.com/vxedge/vxperiments/tree/main/results/phase-3-gate-2026-11-01).
The guests in that run are probes, so it is a provisioning result, not a measurement.

Also new: containers and 5G pods run over the Podman API, container zones can run an OCI image, zones
can boot under Cloud Hypervisor, and the `vehicle` verbs refuse to act on the wrong vehicle.

### Added

**Vehicles**

- A `kind: vehicle` document can declare `services`, each placed on a container device with `on:`.
  The service runs as a member of that device's container group, so it answers on the device's
  slice only. Placement is checked when the workspace loads, a placed image must be `localhost/` or
  digest-pinned, and one service type cannot be declared on both a vehicle and its experiment.
  `svc up`, `down` and `status` handle placed services.
- `localhost/…` part refs name an image in the host's Podman store: local, no digest needed, and run
  as a container device's payload.
- `vehicle up` and `down` have a compute stage. Hpc devices go through `ComputeService` in the order
  `ARCHITECTURE.md` §5.4 gives, so audit lines and events name them as compute.
- `VehicleStatus.gateway` (additive): per-slice, ingress filter and impairment detail for
  `vehicle status`.
- `JobService.Watch` on a composite job shows each sub-job's steps, not one line per stage.
- `VehicleService` audit lines carry the workspace sha256.

**Zones and containers**

- `backend: cloud-hypervisor`: a zone node can boot a guest bundle under Cloud Hypervisor, over its
  REST API on a per-node Unix socket. It adopts the gateway's tap without touching its addressing,
  and reads liveness from the socket, so a killed VMM reads as down and the next `up` recovers it.
  `qemu-microvm` remains available.
- A zone `container` node can run an OCI image. A node with a digest-pinned or `localhost/` container
  part runs it in a container group named after the node: an infra container owns the network
  namespace, the gateway port moves in as `eth0`, and the image starts as a member. Nodes with
  `local/…` parts stay a named netns the runner execs into, so every existing workspace behaves as
  before.
- `vxedge_core::containers`: container groups over Podman's REST API on the rootless user socket. A
  group replaces a `podman pod` and shares only its network namespace. Membership is by label and `up` converges by a spec hash, so a
  second `up` keeps what matches and recreates only what changed.

### Changed

- `vehicle status` plans from the workspace and observes the host, so it stays right after a daemon
  restart. It reports zones, compute and services by kind, prints state names and the rolled-up
  state, and lists declared devices nothing provisions.
- `vehicle up` plans every node and checks `localhost/` images and artifact stores before touching the
  host, so a workspace that cannot provision no longer leaves a gateway behind.
- A device with two slice attachments is refused at plan time. It used to fail on `eth0` in a
  container and silently lose its second NIC in a microVM.
- The 5G core, RAN and UE pods are driven through the Podman API, not the `podman` CLI. Container
  names (`vx5g-nrf`, `vx5g-ran-gnb`, ...) are unchanged, and each pod's infra container is the
  Kubernetes pause image, pinned by digest.
- `fiveg` and `traffic::profile_up` are `async`.
- A GitHub Release now shows that version's changelog section, and a tag with no section fails before
  anything is attested.
- `deny.toml`: removed the `RUSTSEC-2024-0436` (`paste`) ignore; the crate is no longer in the
  dependency tree.

### Fixed

**The wrong vehicle**

- `vehicle up` of a second vehicle over a running one took its bridge and left its nodes running
  without one, and `vehicle down` of the second vehicle then removed that bridge. `vehicle up`,
  `status` and `down` now refuse anything but the active workspace.
- That check survives a daemon restart, and it reads the host for gateway ports and guests the
  workspace does not declare, so a vehicle started under a daemon with another runtime dir is
  protected too. `experiment down` uses the same record of what is active.
- `vehicle status` reported the active vehicle under whatever id it was asked about.

**Teardown**

- `vehicle down` after a daemon restart skipped every node and removed the gateway from under them. It
  now plans from the workspace.
- A failing node in `down` no longer stops the nodes after it from being torn down.
- `zone down` on a container node returned before the kernel had finished destroying its gateway port,
  so a gateway `up` right after it could keep the dying port and end up with none. `down` now waits
  for the port to go.

**Status and errors**

- A microVM's status and `down` trust its pidfile only when that PID is this node's QEMU. A reused PID
  read as up, and `down` would have killed it. When the pidfile is another daemon's, the guest is
  found through `/proc`.
- `GatewayService.Status` reports a planned gateway with no bridge as down, not degraded.
- `experiment up` reported success with no zones when the zone plan failed. It now fails, naming the
  plan error.
- `virsh` failures were reported as a missing `CAP_NET_ADMIN`, which hid libvirt's real error.
- A `libvirt` `up` whose start failed never converged.
- A second `vxedge experiment up` over a running 5G core failed with `File exists` on the bridge
  attachment veth; it is now a no-op.

### Security

- The `libvirt` zone backend runs `virsh` as `vxedged`'s own user, no longer through `sudo`. `virsh` is
  a client of `libvirtd`, which authorizes callers by `libvirt` group membership, so escalating it
  only widened what `vxedged` ran as root. `vxedge doctor --tier 2` checks that `qemu:///system`
  answers without `sudo`.
- Lockfile update `pulldown-cmark` 0.13.4 -> 0.13.5, which fixes RUSTSEC-2026-0414 (exponential event
  growth with `ENABLE_WIKILINKS`). It is a build-time dependency via `prost-build`.

### Migration

- Under `setcap`, `vxedged` needs `cap_net_admin,cap_sys_admin`: container zones enter a network
  namespace, which is `setns(2)`.
- 5G hosts, OCI container nodes and placed services need the rootless Podman socket
  (`systemctl --user enable --now podman.socket`, plus `loginctl enable-linger` on a host you reach
  over SSH). 5G hosts also need `passt`. See `docs/TESTBED-SETUP.md` Tier 2. `vxedged` starts without
  the socket and connects on first use.
- `vxedged`'s user must be in the `libvirt` group for the `libvirt` backend.
- Pods created by an older `vxedged` carry no `io.vxedge.group` label, so this version cannot manage
  them. `vxedge experiment up` and `down` refuse to run over them and print the command to remove
  them once: `podman pod rm -f vx5g vx5g-ran $(podman pod ps -q --filter name=vx5g-ran-ue-)`.
- A script that parses `vehicle status` needs updating: the output now has state names and kinds.
- A script that brought one vehicle up over another now gets a refusal. Bring the first one down.

### Known limits

- A device takes one slice attachment. `role: gateway` devices run as ordinary nodes, and `resources`
  is recorded but not enforced.
- Only `ota` can be placed on a device. A placed service that dies makes its device read as down.
- `vehicle down` takes about 15 s per container, waiting out the stop timeout of a payload that does
  not handle SIGTERM.
- `localhost/` images are per host, so a run using them reproduces for its author until they are
  published.
- `experiment up` has no wrong-workspace check, and `experiment` verbs do not read the host. A
  container or Cloud Hypervisor guest is seen by the host check through its gateway port only.
- Another daemon cannot see a Cloud Hypervisor guest. Its kernel command line goes over the API, so
  the `/proc` lookup that finds a microVM does not apply. A daemon with a different runtime dir on the
  same workspace reads the guest as down, and `vehicle down` from there can remove the gateway from
  under it. This comes from reading the code, not from a run. Use one runtime dir.
- Cloud Hypervisor is validated on the `linux-probe` bundle and v54.0 only, and live-tested as root
  in a network namespace; an unprivileged VMM booted separately, outside this backend, and traffic
  through one is untested. CPU pinning is untested on this backend, which is not part of the phase-3
  gate: `vx-vehicle-02` uses `container` and `qemu-microvm`.
- A node under Cloud Hypervisor gets a stable MAC derived from its id: unicast to the guest failed
  without one, and why is not yet understood.
- `vehicle down` leaves a Cloud Hypervisor VMM that stopped answering still running, and the next `up`
  then fails loudly. With `VXEDGE_NETNS` set, an unprivileged `vxedged` refuses to start one, and
  `vxedge doctor` does not check for the binary yet.

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
