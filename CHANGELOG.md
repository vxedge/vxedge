# Changelog

All notable changes to this repo are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning follows
[Semantic Versioning](https://semver.org/) — see `docs/VERSIONING.md` in the `docs` repo.

## [Unreleased]

### Added

- A `kind: vehicle` document can declare `signal_plane.kuksa`, the Kuksa databroker placed on a container device with `on:`, under the same rules as a placed service. Its placement is checked when the workspace loads. `vehicle up` does not start it yet.

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
